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
    // Ordinary coverage remains a prerequisite; the second pass adds one near-limit
    // UTF-8 result, routed alongside a healthy subscribed frontend consumer.
    drop(Peer::open(&session).await?);
    run_native_probe(&session, &source, &program, 0).await?;
    let (limit, comment) = payload::large_catalog(&session).await?;
    let bytes = comment.len();
    ensure!(
        (limit - 128 * 1024..limit).contains(&bytes),
        "native fixture size"
    );
    drop(comment);
    eprintln!("APP_SOCKET_PROBE_FIXTURE payload_bytes={bytes} result_limit={limit}");
    let mut healthy = Peer::open(&session).await?;
    let subscription = healthy.subscribe().await?;
    run_native_probe(&session, &source, &program, bytes).await?;
    ensure!(
        healthy.call("applications.revision", json!({})).await?["ok"] == true,
        "native large read retired healthy socket owner"
    );
    ensure!(
        healthy
            .request(json!({"op":"cancel", "request_id":subscription}))
            .await?["data"]["kind"]
            == "subscription",
        "native large read retired healthy subscription"
    );
    drop(healthy);
    ensure!(
        !session.state_path("arguments").exists(),
        "read probe launched an application"
    );
    session.shutdown().await
}

async fn run_native_probe(
    session: &Session,
    source: &std::path::Path,
    program: &std::ffi::OsStr,
    payload_bytes: usize,
) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new(program)
        .args(["--no-color", "--path"])
        .arg(source.join("tests/e2e/app-unix-jsonl.qml"))
        .env_clear()
        .env("HOME", session.root.path())
        .env("XDG_RUNTIME_DIR", session.root.path().join("run"))
        .env("XDG_CONFIG_HOME", session.root.path().join("config"))
        .env("XDG_CACHE_HOME", session.root.path().join("cache"))
        .env("XDG_STATE_HOME", session.root.path().join("state"))
        .env("XDG_DATA_HOME", session.root.path().join("data"))
        .env("XDG_DATA_DIRS", session.root.path().join("empty"))
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
        .env("APP_SOCKET_PROBE_BYTES", payload_bytes.to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    // Never accumulate a payload-sized diagnostic, even on a negative path.
    const LOG_LIMIT: u64 = 32 * 1024;
    let mut stdout = child
        .stdout
        .take()
        .context("native stdout")?
        .take(LOG_LIMIT + 1);
    let mut stderr = child
        .stderr
        .take()
        .context("native stderr")?
        .take(LOG_LIMIT + 1);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let result = timeout(support::DEADLINE, async {
        tokio::try_join!(
            child.wait(),
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        )
    })
    .await;
    if !matches!(&result, Ok(Ok(_))) {
        // Dropping a timed-out future only requests kill-on-drop. Own and await
        // this private child before returning an I/O/deadline error to the fixture.
        timeout(Duration::from_secs(2), child.kill())
            .await
            .context("native probe reap deadline")??;
    }
    let (status, _, _) = result.context("native probe deadline")??;
    ensure!(
        out.len() <= LOG_LIMIT as usize && err.len() <= LOG_LIMIT as usize,
        "native diagnostic limit"
    );
    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    let marker = if payload_bytes == 0 {
        "APP_SOCKET_PROBE_PASS reads-and-renewal"
    } else {
        "APP_SOCKET_PROBE_PASS large-utf8-and-healthy-peer"
    };
    ensure!(status.success() && log.contains(marker), "{log}");
    Ok(())
}

#[tokio::test]
async fn native_probe_timeout_reaps_private_child() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut session = Session::start(true).await?;
    let program = session.root.path().join("bin/native-stall");
    std::fs::write(
        &program,
        format!(
            "#!{}\nprintf '%s' \"$$\" > \"$XDG_STATE_HOME/native-probe.pid\"\nexec '{}' 60\n",
            support::tool("sh")?.display(),
            support::tool("sleep")?.display()
        ),
    )?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;
    // The fake entrypoint ignores Quickshell arguments, execs only private fake
    // work, and never opens a bus/compositor. No real native runtime is required.
    let result = run_native_probe(&session, session.root.path(), program.as_os_str(), 0).await;
    assert!(result.is_err_and(|error| error.to_string() == "native probe deadline"));
    let pid = session.pid("native-probe.pid").await?;
    assert!(
        !std::path::Path::new(&format!("/proc/{}", pid.as_raw_pid())).exists(),
        "timed-out native fixture child must be reaped before returning"
    );
    session.shutdown().await
}

#[path = "socket/payload.rs"]
mod payload;

pub(super) struct Peer {
    io: BufReader<UnixStream>,
    events: VecDeque<Value>,
    sequence: u64,
}
impl Peer {
    pub(super) async fn open(session: &Session) -> Result<Self> {
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
        timeout(
            support::DEADLINE,
            self.io
                .get_mut()
                .write_all(format!("{request}\n").as_bytes()),
        )
        .await??;
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
    pub(super) async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
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
