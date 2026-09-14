pub mod work_area;
#[cfg(test)]
mod work_area_tests;

use std::{env, os::unix::fs::MetadataExt, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::mpsc,
    time,
};

const IPC_TIMEOUT: Duration = Duration::from_secs(2);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const COMMAND_SOCKET: &str = ".socket.sock";
const EVENT_SOCKET: &str = ".socket2.sock";

#[derive(Debug, Clone)]
pub struct Client {
    runtime_dir: PathBuf,
    signature: Option<String>,
}

impl Default for Client {
    fn default() -> Self {
        Self::from_environment()
    }
}

impl Client {
    #[must_use]
    pub fn from_environment() -> Self {
        let runtime_dir = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(default_runtime_dir);
        let signature = env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .ok()
            .filter(|value| valid_signature(value));
        Self {
            runtime_dir,
            signature,
        }
    }

    #[must_use]
    pub fn new(runtime_dir: PathBuf, signature: Option<String>) -> Self {
        Self {
            runtime_dir,
            signature: signature.filter(|value| valid_signature(value)),
        }
    }

    pub async fn request(&self, command: &str) -> Result<String> {
        let socket = self
            .instance_dir(COMMAND_SOCKET)
            .await?
            .join(COMMAND_SOCKET);
        time::timeout(IPC_TIMEOUT, request_socket(&socket, command))
            .await
            .context("Hyprland command timed out")?
    }

    pub async fn work_areas(
        &self,
    ) -> Result<std::collections::BTreeMap<String, work_area::Insets>> {
        let reply = self.request("[[BATCH]]j/monitors;j/workspaces;j/workspacerules;j/clients;j/getoption general:gaps_out").await?;
        work_area::parse_work_areas(&reply)
    }

    pub async fn event_socket(&self) -> Result<UnixStream> {
        let socket = self.instance_dir(EVENT_SOCKET).await?.join(EVENT_SOCKET);
        time::timeout(IPC_TIMEOUT, UnixStream::connect(&socket))
            .await
            .context("Hyprland event connection timed out")?
            .with_context(|| format!("connect to Hyprland event socket {}", socket.display()))
    }

    async fn instance_dir(&self, required_socket: &str) -> Result<PathBuf> {
        for root in self.roots() {
            if let Some(signature) = &self.signature {
                let preferred = root.join(signature);
                if fs::try_exists(preferred.join(required_socket))
                    .await
                    .unwrap_or(false)
                {
                    return Ok(preferred);
                }
            }
            let Ok(mut entries) = fs::read_dir(&root).await else {
                continue;
            };
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if fs::try_exists(path.join(required_socket))
                    .await
                    .unwrap_or(false)
                {
                    return Ok(path);
                }
            }
        }
        bail!("no active Hyprland IPC instance is available")
    }

    fn roots(&self) -> Vec<PathBuf> {
        let runtime = self.runtime_dir.join("hypr");
        let temporary = PathBuf::from("/tmp/hypr");
        if runtime == temporary {
            vec![runtime]
        } else {
            vec![runtime, temporary]
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Connected,
    Disconnected,
    Message(String),
}

/// Compatibility stream for clients that reconcile on every compositor event.
pub async fn watch_events(sender: mpsc::Sender<()>) {
    watch_events_with(Client::default(), sender, |_| ()).await;
}

/// Includes connection transitions so consumers can reduce healthy polling,
/// filter unrelated events, and refresh immediately when delivery recovers.
pub async fn watch_events_detailed(sender: mpsc::Sender<Event>) {
    watch_events_with(Client::default(), sender, |event| event).await;
}

async fn watch_events_with<T, F>(client: Client, sender: mpsc::Sender<T>, map: F)
where
    F: Fn(Event) -> T,
{
    while !sender.is_closed() {
        if let Ok(stream) = client.event_socket().await {
            if sender.send(map(Event::Connected)).await.is_err() {
                return;
            }
            let mut lines = BufReader::new(stream).lines();
            loop {
                tokio::select! {
                    _ = sender.closed() => return,
                    line = lines.next_line() => match line {
                        Ok(Some(line)) => if sender.send(map(Event::Message(line))).await.is_err() { return; },
                        _ => break,
                    }
                }
            }
            if sender.send(map(Event::Disconnected)).await.is_err() {
                return;
            }
        }
        // Bound reconnect attempts even if a socket accepts and immediately
        // closes (the old EOF path could spin without any delay).
        tokio::select! {
            _ = sender.closed() => return,
            _ = time::sleep(RECONNECT_DELAY) => {},
        }
    }
}

async fn request_socket(path: &std::path::Path, command: &str) -> Result<String> {
    let mut stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("connect to Hyprland command socket {}", path.display()))?;
    stream.write_all(command.as_bytes()).await?;
    stream.shutdown().await?;
    let mut response = String::new();
    const MAX_REPLY_BYTES: u64 = 16 * 1024 * 1024;
    stream
        .take(MAX_REPLY_BYTES + 1)
        .read_to_string(&mut response)
        .await?;
    anyhow::ensure!(
        response.len() as u64 <= MAX_REPLY_BYTES,
        "Hyprland reply is too large"
    );
    Ok(response)
}

fn default_runtime_dir() -> PathBuf {
    let uid = std::fs::metadata("/proc/self").map_or(0, |process| process.uid());
    PathBuf::from(format!("/run/user/{uid}"))
}

fn valid_signature(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-.".contains(character))
}

#[cfg(test)]
mod tests {
    use super::valid_signature;

    #[tokio::test]
    async fn request_uses_bounded_socket_protocol_without_a_process() {
        use super::*;
        let root = tempfile::tempdir().unwrap();
        let instance = root.path().join("hypr/test");
        fs::create_dir_all(&instance).await.unwrap();
        let listener = tokio::net::UnixListener::bind(instance.join(COMMAND_SOCKET)).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut command = String::new();
            stream.read_to_string(&mut command).await.unwrap();
            assert_eq!(command, "j/clients");
            stream.write_all(b"[]").await.unwrap();
        });
        let client = Client::new(root.path().into(), Some("test".into()));
        assert_eq!(client.request("j/clients").await.unwrap(), "[]");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn event_stream_reports_reconnects_and_backs_off_after_eof() {
        use super::*;
        let root = tempfile::tempdir().unwrap();
        let instance = root.path().join("hypr/test");
        fs::create_dir_all(&instance).await.unwrap();
        let listener = tokio::net::UnixListener::bind(instance.join(EVENT_SOCKET)).unwrap();
        let (sender, mut events) = mpsc::channel(8);
        let client = Client::new(root.path().into(), Some("test".into()));
        let task = tokio::spawn(watch_events_with(client, sender, |event| event));
        let (mut stream, _) = listener.accept().await.unwrap();
        assert_eq!(events.recv().await.unwrap(), Event::Connected);
        stream
            .write_all(b"openwindow>>123,1,app,title\n")
            .await
            .unwrap();
        assert_eq!(
            events.recv().await.unwrap(),
            Event::Message("openwindow>>123,1,app,title".into())
        );
        drop(stream);
        assert_eq!(events.recv().await.unwrap(), Event::Disconnected);
        assert!(
            time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        let (_stream, _) = time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(events.recv().await.unwrap(), Event::Connected);
        drop(events);
        time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn rejects_signatures_that_can_escape_the_runtime_root() {
        assert!(valid_signature("instance_123.456"));
        assert!(!valid_signature("../other"));
        assert!(!valid_signature("nested/instance"));
        assert!(!valid_signature(""));
    }
}
