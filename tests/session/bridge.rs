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
        self.until(|v| {
            v["kind"] == "event"
                && v["event"]["operation"]["id"] == id
                && v["event"]["operation"]["status"] == "completed"
        })
        .await?;
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
                .any(|v| v["kind"] == "event" && v["event"]["operation"]["id"] == foreign)
        );
    }
    session.shutdown().await
}
