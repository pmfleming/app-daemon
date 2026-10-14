//! Two real JSONL processes and a real daemon on an isolated session bus.
use super::support::{DEADLINE, Session};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use shelllist_daemon_core::DEFAULT_JSONL_REQUEST_BYTES as MAX_JSONL_REQUEST_BYTES;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout},
    time::timeout,
};

struct Bridge {
    child: Child,
    input: ChildStdin,
    output: Lines<BufReader<ChildStdout>>,
    seen: Vec<Value>,
}
impl Bridge {
    fn start(session: &Session) -> Result<Self> {
        let mut child = session.bridge()?;
        Ok(Self {
            input: child.stdin.take().context("bridge stdin")?,
            output: BufReader::new(child.stdout.take().context("bridge stdout")?).lines(),
            child,
            seen: Vec::new(),
        })
    }
    async fn send(&mut self, value: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        timeout(DEADLINE, self.input.write_all(&bytes)).await??;
        Ok(())
    }
    async fn until(&mut self, predicate: impl Fn(&Value) -> bool) -> Result<Value> {
        timeout(DEADLINE, async {
            loop {
                let line = self
                    .output
                    .next_line()
                    .await?
                    .context("bridge closed output")?;
                let value: Value = serde_json::from_str(&line)?;
                self.seen.push(value.clone());
                if predicate(&value) {
                    return Ok(value);
                }
            }
        })
        .await
        .context("bridge response deadline")?
    }
    async fn response(&mut self, id: &str) -> Result<Value> {
        self.until(|v| v["kind"] == "response" && v["id"] == id)
            .await
    }
    async fn call(&mut self, id: &str, method: &str, params: Value) -> Result<Value> {
        self.send(json!({"op":"call", "id":id, "method":method, "params":params, "route":route(id, "call")})).await?;
        let reply = self.response(id).await?;
        assert_eq!(reply["route"], route(id, "call"));
        assert_eq!(reply["ok"], true, "{reply}");
        Ok(reply["response"].clone())
    }
    async fn subscribe(&mut self) -> Result<String> {
        self.send(json!({"op":"subscribe", "id":"subscribe", "streams":["applications.operation"], "route":route("subscribe", "subscription")})).await?;
        let reply = self.response("subscribe").await?;
        assert_eq!(reply["route"], route("subscribe", "subscription"));
        assert_eq!(reply["response"]["ok"], true, "{reply}");
        Ok(reply["response"]["data"]["subscription"]["id"]
            .as_str()
            .context("subscription id")?
            .into())
    }
    async fn completed(&mut self, id: &str) -> Result<()> {
        let terminal = |v: &Value| {
            v["kind"] == "event"
                && v["event"]["data"]["operation"]["id"] == id
                && v["event"]["data"]["operation"]["status"] == "completed"
        };
        if !self.seen.iter().any(terminal) {
            self.until(terminal).await?;
        }
        Ok(())
    }
    async fn shutdown(mut self) -> Result<Vec<Value>> {
        self.send(json!({"op":"shutdown", "id":"shutdown"})).await?;
        assert_eq!(
            self.response("shutdown").await?["response"]["shutdown"],
            true
        );
        assert!(timeout(DEADLINE, self.child.wait()).await??.success());
        // Drain EOF so ownership assertions include every emitted event.
        timeout(DEADLINE, async {
            while let Some(line) = self.output.next_line().await? {
                self.seen.push(serde_json::from_str(&line)?);
            }
            Result::<()>::Ok(())
        })
        .await??;
        Ok(self.seen)
    }
}
/// Manual measurement, not a timing threshold in CI. All effects use the fixture launcher.
#[tokio::test]
#[ignore = "manual current-source reliability baseline"]
async fn reliability_baseline() -> Result<()> {
    use std::time::{Duration, Instant};
    fn ticks(pid: u32) -> Result<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .context("proc stat")?
            .1
            .split_whitespace()
            .collect();
        Ok(fields[11].parse::<u64>()? + fields[12].parse::<u64>()?)
    }
    let mut session = Session::start(true).await?;
    let mut bridge = Bridge::start(&session)?;
    bridge.subscribe().await?;
    let mut latencies = Vec::new();
    for index in 0..200 {
        let started = Instant::now();
        assert_eq!(
            bridge
                .call(&format!("read-{index}"), "applications.revision", json!({}))
                .await?["ok"],
            true
        );
        latencies.push(started.elapsed().as_secs_f64() * 1_000_000.0);
    }
    for index in 0..10 {
        let result = bridge
            .call(
                &format!("effect-{index}"),
                "applications.execute",
                json!({"target_id":"ok.desktop", "action":"launch"}),
            )
            .await?;
        let id = result["data"]["operation"]["id"]
            .as_str()
            .context("operation id")?;
        bridge.completed(id).await?;
    }
    // Drain all previous traffic before observing idle. No polling runs here.
    bridge
        .call("idle-fence", "applications.revision", json!({}))
        .await?;
    let idle_start = bridge.seen.len();
    let pid = bridge.child.id().context("bridge pid")?;
    let before_ticks = ticks(pid)?;
    let started = Instant::now();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let idle_seconds = started.elapsed().as_secs_f64();
    let idle_cpu_ticks = ticks(pid)? - before_ticks;
    bridge
        .call("idle-end", "applications.revision", json!({}))
        .await?;
    let idle_events = bridge.seen[idle_start..]
        .iter()
        .filter(|v| v["kind"] == "event")
        .count();
    latencies.sort_by(f64::total_cmp);
    let frames = bridge.shutdown().await?;
    let events: Vec<_> = frames.iter().filter(|v| v["kind"] == "event").collect();
    println!(
        "SHELLLIST_RELIABILITY {}",
        json!({
            "scenario":"isolated-real-app-jsonl-and-dbus", "profile":"cargo-test-debug",
            "samples":latencies.len(), "latency_us":{"p50":latencies[99],"p95":latencies[189],"p99":latencies[197]},
            "event_count":events.len(), "event_json_bytes":events.iter().map(|v| v.to_string().len()).sum::<usize>(),
            "transport_recoveries":frames.iter().filter(|v| v["kind"] == "transport-error").count(),
            "idle_seconds":idle_seconds,"idle_bridge_cpu_ticks":idle_cpu_ticks,"idle_event_count":idle_events,
            "effects":"ten temporary fixture launcher operations; no live desktop effects"
        })
    );
    session.shutdown().await
}

fn route(id: &str, kind: &str) -> Value {
    // Identical local identifiers must not turn two bridge connections into one owner.
    json!({"consumerId":"same-view", "localId":id, "generation":1, "kind":kind})
}

#[tokio::test]
async fn oversized_effect_is_not_dispatched_and_two_bridges_remain_owned_and_usable() -> Result<()>
{
    let mut session = Session::start(true).await?;
    let mut a = Bridge::start(&session)?;
    let mut b = Bridge::start(&session)?;
    let a_subscription = a.subscribe().await?;
    let b_subscription = b.subscribe().await?;
    assert_ne!(a_subscription, b_subscription);
    // This is otherwise a valid effect: the service accepts unknown additive parameters.
    // Leave it unterminated while the other connection proves it can still make progress.
    let oversized = json!({"op":"call", "id":"oversized", "method":"applications.execute",
        "params":{"target_id":"ok.desktop", "action":"launch", "padding":"x".repeat(MAX_JSONL_REQUEST_BYTES + 1)}, "route":route("oversized", "call")});
    let bytes = serde_json::to_vec(&oversized)?;
    timeout(DEADLINE, a.input.write_all(&bytes)).await??;
    assert_eq!(
        b.call("healthy", "applications.revision", json!({}))
            .await?["ok"],
        true
    );
    a.input.write_all(b"\n").await?;
    let rejected = a
        .until(|v| {
            v["kind"] == "protocol-error" || (v["kind"] == "response" && v["id"] == "oversized")
        })
        .await?;
    assert_eq!(
        rejected["kind"], "protocol-error",
        "oversized effect was dispatched: {rejected}"
    );
    assert_eq!(
        rejected["error"],
        format!("JSONL request exceeds {MAX_JSONL_REQUEST_BYTES} bytes")
    );
    assert_eq!(
        a.call("after-oversize", "applications.revision", json!({}))
            .await?["ok"],
        true
    );
    assert!(
        !session.state_path("arguments").exists(),
        "rejected request must never invoke the fake launcher"
    );

    b.send(json!({"op":"cancel", "id":"foreign-cancel", "request_id":a_subscription, "route":route("foreign-cancel", "control")})).await?;
    assert_eq!(
        b.response("foreign-cancel").await?["response"]["error"]["code"],
        "request-not-found"
    );
    let accepted_a = a
        .call(
            "execute",
            "applications.execute",
            json!({"target_id":"ok.desktop", "action":"launch"}),
        )
        .await?;
    let id_a = accepted_a["data"]["operation"]["id"]
        .as_str()
        .context("operation A")?
        .to_owned();
    a.completed(&id_a).await?;
    assert_eq!(
        b.call(
            "foreign-status",
            "applications.operation.status",
            json!({"operation_id":id_a})
        )
        .await?["error"]["code"],
        "request-not-found"
    );
    assert_eq!(
        a.call(
            "own-status",
            "applications.operation.status",
            json!({"operation_id":id_a})
        )
        .await?["data"]["operation_status"]["status"],
        "completed"
    );
    let accepted_b = b
        .call(
            "execute",
            "applications.execute",
            json!({"target_id":"ok.desktop", "action":"launch"}),
        )
        .await?;
    let id_b = accepted_b["data"]["operation"]["id"]
        .as_str()
        .context("operation B")?
        .to_owned();
    b.completed(&id_b).await?;
    assert_ne!(id_a, id_b);
    // Cancellation remains usable after malformed input, without restarting or replaying.
    a.send(json!({"op":"cancel", "id":"own-cancel", "request_id":a_subscription, "route":route("own-cancel", "control")})).await?;
    assert_eq!(
        a.response("own-cancel").await?["response"]["data"]["kind"],
        "subscription"
    );
    let seen_a = a.shutdown().await?;
    let seen_b = b.shutdown().await?;
    for (seen, foreign) in [(seen_a, id_b), (seen_b, id_a)] {
        assert!(
            !seen
                .iter()
                .any(|v| v["kind"] == "event" && v["event"]["data"]["operation"]["id"] == foreign)
        );
    }
    session.shutdown().await
}
