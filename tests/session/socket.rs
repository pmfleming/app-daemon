//! Native sockets against the actual daemon OS process and isolated desktop fixtures.
use crate::support::{self, Session};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::VecDeque;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::{Duration, sleep, timeout},
};

#[tokio::test]
#[ignore = "explicit private native Quickshell acceptance; requires SHELLLIST_SOURCE and SHELLLIST_QUICKSHELL"]
async fn native_quickshell_app_socket_acceptance() -> Result<()> {
    let mut session = Session::start(true).await?;
    let source =
        std::path::PathBuf::from(std::env::var_os("SHELLLIST_SOURCE").context("SHELLLIST_SOURCE")?);
    let program = std::env::var_os("SHELLLIST_QUICKSHELL").context("SHELLLIST_QUICKSHELL")?;
    // Wait for listener readiness independently of D-Bus name acquisition.
    drop(Peer::open(&session).await?);
    let child = tokio::process::Command::new(program)
        .args(["--no-color", "--path"])
        .arg(source.join("tests/e2e/app-unix-jsonl.qml"))
        .env_clear()
        .env("HOME", session.root.path())
        .env("XDG_RUNTIME_DIR", session.root.path().join("run"))
        .env("QML_IMPORT_PATH", source.join("qml"))
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_QUICK_BACKEND", "software")
        .env(
            "DBUS_SYSTEM_BUS_ADDRESS",
            "unix:path=/nonexistent-system-bus",
        )
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent-session-bus",
        )
        .env("SHELLLIST_APP_SOCKET", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let output = timeout(support::DEADLINE, child.wait_with_output()).await??;
    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        output.status.success() && log.contains("APP_SOCKET_PROBE_PASS"),
        "{log}"
    );
    ensure!(
        !session.state_path("arguments").exists(),
        "read probe launched an application"
    );
    session.shutdown().await
}

struct Peer {
    io: BufReader<UnixStream>,
    events: VecDeque<Value>,
    sequence: u64,
}
impl Peer {
    async fn open(session: &Session) -> Result<Self> {
        let io = timeout(support::DEADLINE, async {
            loop {
                match UnixStream::connect(session.root.path().join("run/app-daemon.sock")).await {
                    Ok(io) => return io,
                    Err(_) => sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await?;
        Ok(Self {
            io: BufReader::new(io),
            events: VecDeque::new(),
            sequence: 0,
        })
    }
    async fn record(&mut self) -> Result<Value> {
        let mut line = String::new();
        ensure!(
            timeout(support::DEADLINE, self.io.read_line(&mut line)).await?? > 0,
            "socket EOF"
        );
        Ok(serde_json::from_str(&line)?)
    }
    async fn request(&mut self, mut request: Value) -> Result<Value> {
        self.sequence += 1;
        request["id"] = json!(format!("request-{}", self.sequence));
        let kind = match request["op"].as_str() {
            Some("call") => "call",
            Some("subscribe") => "subscription",
            _ => "control",
        };
        request["route"] =
            json!({"consumerId":"view", "localId":"read", "kind":kind, "generation":1});
        self.io
            .get_mut()
            .write_all(format!("{request}\n").as_bytes())
            .await?;
        loop {
            let reply = self.record().await?;
            ensure!(
                reply["kind"] != "protocol-error" && reply["kind"] != "transport-error",
                "{reply}"
            );
            if reply["kind"] == "response" && reply["id"] == request["id"] {
                ensure!(
                    reply["route"] == request["route"],
                    "route must survive direct ingress"
                );
                return Ok(reply["response"].clone());
            }
            ensure!(self.events.len() < 128, "bounded fixture backlog");
            self.events.push_back(reply);
        }
    }
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request(json!({"op":"call", "method":method,"params":params}))
            .await
    }
    async fn subscribe(&mut self) -> Result<String> {
        let response = self.request(json!({"op":"subscribe", "streams":["applications.changed","windows.changed","applications.operation"]})).await?;
        Ok(response["data"]["subscription"]["id"]
            .as_str()
            .context("subscription ID")?
            .into())
    }
}

#[tokio::test]
async fn socket_dispatch_owner_cleanup_and_real_process_crash_are_isolated() -> Result<()> {
    let mut session = Session::start(true).await?;
    let mut a = Peer::open(&session).await?;
    let mut b = Peer::open(&session).await?;
    {
        let proxy = session.proxy().await?;
        for (method, params) in [
            ("applications.revision", json!({})),
            ("applications.query", json!({"limit":"wrong"})),
            (
                "applications.operation.status",
                json!({"operation_id":"unknown"}),
            ),
            ("unknown.method", json!({})),
        ] {
            assert_eq!(
                a.call(method, params.clone()).await?,
                support::call(&proxy, method, params).await?
            );
        }
    }
    let subscription = a.subscribe().await?;
    b.subscribe().await?;
    assert_eq!(
        b.request(json!({"op":"cancel", "request_id":subscription}))
            .await?["error"]["code"],
        "request-not-found"
    );
    let accepted = a
        .call(
            "applications.execute",
            json!({"target_id":"slow.desktop","action":"launch"}),
        )
        .await?;
    let operation = accepted["data"]["operation"]["id"]
        .as_str()
        .context("accepted operation")?;
    let pid = session.pid("handoff.pid").await?;
    assert_eq!(
        b.call(
            "applications.operation.status",
            json!({"operation_id":operation})
        )
        .await?["error"]["code"],
        "request-not-found"
    );
    assert_eq!(
        b.request(json!({"op":"cancel", "request_id":operation}))
            .await?["error"]["code"],
        "request-not-found"
    );
    drop(a);
    support::stopped(pid).await?;
    // The surviving owner's reads and subscriptions remain usable. Foreign
    // lifecycle payloads must not leak through its broadcast subscription.
    assert_eq!(
        b.call("applications.revision", json!({})).await?["ok"],
        true
    );
    assert!(
        !b.events
            .iter()
            .any(|e| e.pointer("/event/data/operation/id") == Some(&json!(operation)))
    );
    let released = b.request(json!({"op":"release"})).await?;
    assert!(released["released"].as_u64().unwrap_or_default() >= 1);
    // The effect starts, but this peer never consumes its acceptance. Closing
    // still retires late accepted work without affecting b's transport.
    std::fs::remove_file(session.state_path("handoff.pid"))?;
    let mut unacknowledged = Peer::open(&session).await?;
    unacknowledged.io.get_mut().write_all(b"{\"id\":\"unread\",\"op\":\"call\",\"method\":\"applications.execute\",\"params\":{\"target_id\":\"slow.desktop\",\"action\":\"launch\"}}\n").await?;
    let unread_pid = session.pid("handoff.pid").await?;
    drop(unacknowledged);
    support::stopped(unread_pid).await?;

    // SIGKILL during an accepted handoff: unlike graceful disconnect, an OS
    // crash cannot promise rollback. The new owner must report unknown, not
    // infer completion or replay. Session::drop kills only this private child.
    std::fs::remove_file(session.state_path("handoff.pid"))?;
    let mut crashing = Peer::open(&session).await?;
    let uncertain = crashing
        .call(
            "applications.execute",
            json!({"target_id":"slow.desktop", "action":"launch"}),
        )
        .await?;
    let uncertain_id = uncertain["data"]["operation"]["id"]
        .as_str()
        .context("uncertain ID")?;
    let uncertain_pid = session.pid("handoff.pid").await?;
    session.crash_restart().await?;
    drop(crashing);
    let mut fresh = Peer::open(&session).await?;
    fresh.subscribe().await?;
    assert_eq!(
        fresh
            .call(
                "applications.operation.status",
                json!({"operation_id":operation})
            )
            .await?["error"]["code"],
        "request-not-found"
    );
    assert_eq!(
        fresh
            .call("applications.query", json!({"limit":1000}))
            .await?["ok"],
        true
    );
    assert_eq!(
        fresh
            .call(
                "applications.operation.status",
                json!({"operation_id":uncertain_id})
            )
            .await?["error"]["code"],
        "request-not-found"
    );
    // Reconnect reads must not restart either handoff. The retained PID is
    // evidence of no replay, not evidence of the uncertain action's success.
    assert_eq!(session.pid("handoff.pid").await?, uncertain_pid);
    drop(fresh);
    drop(b);
    session.shutdown().await
}
