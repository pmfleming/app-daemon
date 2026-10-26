//! Actual admitted app service, private process/bus and fake handoffs only.
use super::*;
use crate::bridge::Bridge;
use shelllist_daemon_core::DEFAULT_JSONL_REQUEST_BYTES;
use shelllist_daemon_tokio::JsonlSocketConfig;

fn call_record(id: &str, method: &str, params: Value) -> Value {
    json!({"op":"call", "id":id, "method":method, "params":params,
        "route":{"consumerId":"payload", "localId":"read", "kind":"call", "generation":1}})
}

async fn send(peer: &mut Peer, request: &Value) -> Result<()> {
    timeout(
        support::DEADLINE,
        peer.io
            .get_mut()
            .write_all(format!("{request}\n").as_bytes()),
    )
    .await??;
    Ok(())
}

async fn response(peer: &mut Peer, request: Value) -> Result<(usize, Value)> {
    send(peer, &request).await?;
    let mut line = String::new();
    ensure!(
        timeout(support::DEADLINE, peer.io.read_line(&mut line)).await?? > 0,
        "payload response EOF"
    );
    let record: Value = serde_json::from_str(&line)?;
    ensure!(record["kind"] == "response", "not a response");
    ensure!(record["id"] == request["id"], "lost response ID");
    ensure!(record["route"] == request["route"], "lost response route");
    Ok((line.len(), record["response"].clone()))
}

#[tokio::test]
async fn exact_request_boundaries_and_retained_transport_validation_parity() -> Result<()> {
    let mut session = Session::start(true).await?;
    let mut a = Peer::open(&session).await?;
    let mut b = Peer::open(&session).await?;
    let mut bridge = Bridge::start(&session)?;
    let proxy = session.proxy().await?;
    for (method, params) in [
        ("applications.revision", json!({})),
        (
            "applications.query",
            json!({"query":"ok.desktop","limit":1,"generation":7}),
        ),
        ("applications.query", json!({"limit":"wrong"})),
        (
            "applications.execute",
            json!({"target_id":"ok.desktop","action":"not-an-action"}),
        ),
        (
            "applications.operation.status",
            json!({"operation_id":"unknown"}),
        ),
        (
            "applications.settings.update",
            json!({"target_id":"ok.desktop","category":"invalid"}),
        ),
        ("unknown.method", json!({})),
    ] {
        let expected = support::call(&proxy, method, params.clone()).await?;
        ensure!(
            a.call(method, params.clone()).await? == expected,
            "socket parity: {method}"
        );
        ensure!(
            bridge.call("parity", method, params).await? == expected,
            "bridge parity: {method}"
        );
    }
    let limit = JsonlSocketConfig::default().maximum_request_bytes;
    ensure!(
        limit == DEFAULT_JSONL_REQUEST_BYTES,
        "app production input changed"
    );
    for ending in [b"\n".as_slice(), b"\r\n".as_slice()] {
        let mut request = call_record("maximum", "applications.revision", json!({"padding":""}));
        let padding_bytes = limit - serde_json::to_vec(&request)?.len();
        request["params"]["padding"] =
            json!("é🦀".repeat(padding_bytes / 6) + &"x".repeat(padding_bytes % 6));
        let mut bytes = serde_json::to_vec(&request)?;
        ensure!(bytes.len() == limit, "structured UTF-8 request sizing");
        bytes.extend_from_slice(ending);
        timeout(support::DEADLINE, a.io.get_mut().write_all(&bytes)).await??;
        let accepted = a.record().await?;
        ensure!(accepted["response"]["ok"] == true, "exact input rejected");
        ensure!(
            accepted["id"] == request["id"] && accepted["route"] == request["route"],
            "exact input correlation"
        );
    }
    // Otherwise-valid effect, exactly one byte over, held without its terminator.
    let mut bytes = serde_json::to_vec(&call_record(
        "oversized",
        "applications.execute",
        json!({"target_id":"ok.desktop","action":"launch"}),
    ))?;
    bytes.resize(limit + 1, b' ');
    timeout(support::DEADLINE, a.io.get_mut().write_all(&bytes)).await??;
    ensure!(
        b.call("applications.revision", json!({})).await?["ok"] == true,
        "healthy peer blocked by unterminated input"
    );
    timeout(support::DEADLINE, a.io.get_mut().write_all(b"\n")).await??;
    let rejected = a.record().await?;
    ensure!(
        rejected["kind"] == "protocol-error",
        "oversized effect not rejected"
    );
    ensure!(
        rejected["error"] == format!("JSONL request exceeds {limit} bytes"),
        "wrong framing boundary"
    );
    ensure!(
        a.call("applications.revision", json!({})).await?["ok"] == true,
        "no recovery after oversize"
    );
    // D-Bus bounds params JSON, whereas the JSONL ceiling includes the envelope.
    let mut params = json!({"padding":""});
    let padding_bytes = limit - serde_json::to_vec(&params)?.len();
    params["padding"] = json!("x".repeat(padding_bytes));
    let params = params.to_string();
    ensure!(params.len() == limit, "structured D-Bus params sizing");
    ensure!(
        support::raw_call(&proxy, "applications.revision", &params).await?["ok"] == true,
        "exact D-Bus params rejected"
    );
    let mut effect = json!({"target_id":"ok.desktop","action":"launch"}).to_string();
    effect.extend(std::iter::repeat_n(' ', limit + 1 - effect.len()));
    let rejected = support::raw_call(&proxy, "applications.execute", &effect).await?;
    ensure!(
        rejected["error"]["code"] == "request-too-large",
        "D-Bus oversized effect admitted"
    );
    ensure!(
        !session.state_path("arguments").exists(),
        "rejected effect reached fake handoff"
    );
    bridge.shutdown().await?;
    drop(proxy);
    drop(a);
    drop(b);
    session.shutdown().await
}

// The real daemon samples resource metadata independently. Leave 64 KiB headroom
// here; exact byte boundaries use the tracker-free admitted-service unit fixture.
async fn large_catalog(session: &Session) -> Result<(usize, String)> {
    let proxy = session.proxy().await?;
    let path = session.root.path().join("data/applications/large.desktop");
    let entry = "[Desktop Entry]\nType=Application\nName=Large fixture\nExec=true\nComment=";
    std::fs::write(&path, format!("{entry}\n"))?;
    ensure!(
        support::call(
            &proxy,
            "applications.refresh",
            json!({"query":"absent-fixture"})
        )
        .await?["ok"]
            == true,
        "fixture refresh"
    );
    let params = json!({"query":"large.desktop","limit":1,"generation":0});
    let baseline = support::call(&proxy, "applications.query", params.clone()).await?;
    ensure!(baseline["ok"] == true, "baseline query");
    let policy = app_daemon::protocol::methods::Method::parse("applications.query")
        .context("query policy")?
        .policy();
    let limit = policy.result_bytes;
    let bytes = limit - serde_json::to_vec(&baseline)?.len() - 64 * 1024;
    let comment = "é🦀".repeat(bytes / 6) + &"x".repeat(bytes % 6);
    ensure!(comment.len() == bytes, "UTF-8 fixture sizing");
    std::fs::write(&path, format!("{entry}{comment}\n"))?;
    ensure!(
        support::call(
            &proxy,
            "applications.refresh",
            json!({"query":"absent-fixture"})
        )
        .await?["ok"]
            == true,
        "large fixture refresh"
    );
    Ok((limit, comment))
}

#[tokio::test]
#[ignore = "explicit private near-16 MiB actual-process UTF-8 qualification"]
async fn large_utf8_result_survives_socket_bridge_and_dbus() -> Result<()> {
    let mut session = Session::start(true).await?;
    let (limit, comment) = large_catalog(&session).await?;
    let mut peer = Peer::open(&session).await?;
    let mut bridge = Bridge::start(&session)?;
    let proxy = session.proxy().await?;
    let params = json!({"query":"large.desktop","limit":1,"generation":0});
    let request = call_record("large", "applications.query", params.clone());
    let (wire_bytes, actual) = response(&mut peer, request.clone()).await?;
    let mut envelope = shelllist_daemon_core::response_message("large", actual.clone());
    envelope["route"] = request["route"].clone();
    ensure!(
        wire_bytes == serde_json::to_vec(&envelope)?.len() + 1,
        "JSONL envelope or LF accounting"
    );
    for value in [
        actual,
        bridge
            .call("large", "applications.query", params.clone())
            .await?,
        support::call(&proxy, "applications.query", params).await?,
    ] {
        ensure!(value["ok"] == true, "large read rejected");
        let page = &value["data"]["applications"];
        ensure!(
            page["applications"]
                .as_array()
                .is_some_and(|rows| rows.len() == 1),
            "large catalog row count"
        );
        ensure!(
            page["applications"][0]["id"] == "large.desktop" && page["generation"] == 0,
            "large identity/generation"
        );
        ensure!(
            page["applications"][0]["comment"] == comment,
            "large UTF-8 fixture corrupted"
        );
        let bytes = serde_json::to_vec(&value)?.len();
        ensure!(
            (limit - 128 * 1024..=limit).contains(&bytes),
            "not a near-limit result"
        );
    }
    ensure!(
        peer.call("applications.revision", json!({})).await?["ok"] == true,
        "large result lost socket"
    );
    ensure!(
        bridge
            .call("healthy", "applications.revision", json!({}))
            .await?["ok"]
            == true,
        "large result lost bridge"
    );
    ensure!(
        !session.state_path("arguments").exists(),
        "read qualification launched fake application"
    );
    bridge.shutdown().await?;
    drop(proxy);
    drop(peer);
    session.shutdown().await
}

async fn stalled_reader_owner_isolation(daemon_workers: usize) -> Result<()> {
    let mut session = Session::start_with_workers(true, daemon_workers).await?;
    // A second fake slow target permits two independently owned accepted handoffs.
    std::fs::copy(
        session.root.path().join("data/applications/slow.desktop"),
        session
            .root
            .path()
            .join("data/applications/slow-peer.desktop"),
    )?;
    let _ = large_catalog(&session).await?;
    let mut stalled = Peer::open(&session).await?;
    let mut healthy = Peer::open(&session).await?;
    let subscription = healthy.subscribe().await?;
    let a = stalled
        .call(
            "applications.execute",
            json!({"target_id":"slow.desktop","action":"launch"}),
        )
        .await?;
    let b = healthy
        .call(
            "applications.execute",
            json!({"target_id":"slow-peer.desktop","action":"launch"}),
        )
        .await?;
    let id_a = a["data"]["operation"]["id"]
        .as_str()
        .context("stalled operation")?;
    let id_b = b["data"]["operation"]["id"]
        .as_str()
        .context("healthy operation")?;
    let pid_a = session.pid("handoff.pid").await?;
    let pid_b = session.pid("handoff-peer.pid").await?;
    // Stop reading only this peer. Production's five-second complete-record write
    // deadline, real kernel backpressure and actual service cleanup are exercised.
    send(
        &mut stalled,
        &call_record(
            "stalled",
            "applications.query",
            json!({"query":"large.desktop","limit":1,"generation":0}),
        ),
    )
    .await?;
    let mut reads = 0;
    timeout(support::DEADLINE, async {
        loop {
            ensure!(
                support::running(pid_b),
                "healthy accepted handoff cancelled"
            );
            ensure!(
                healthy.call("applications.revision", json!({})).await?["ok"] == true,
                "healthy read failed"
            );
            let status = healthy
                .call(
                    "applications.operation.status",
                    json!({"operation_id":id_b}),
                )
                .await?;
            ensure!(
                status["ok"] == true && status["data"]["operation_status"]["status"] == "running",
                "healthy owned status lost"
            );
            reads += 1;
            if !support::running(pid_a) {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        Result::<()>::Ok(())
    })
    .await
    .context("stalled reader did not retire owner")??;
    ensure!(reads > 1, "fixture did not exercise pending write");
    ensure!(
        healthy
            .call(
                "applications.operation.status",
                json!({"operation_id":id_a})
            )
            .await?["error"]["code"]
            == "request-not-found",
        "stalled owner leaked"
    );
    ensure!(
        !healthy
            .events
            .iter()
            .any(|event| event.pointer("/event/data/operation/id") == Some(&json!(id_a))),
        "stalled owner event leaked"
    );
    let cancelled = healthy
        .request(json!({"op":"cancel","request_id":subscription}))
        .await?;
    ensure!(
        cancelled["data"]["kind"] == "subscription",
        "healthy subscription was retired with stalled owner"
    );
    drop(stalled);
    // Disconnect is cancellation, not evidence of effect rollback or replay.
    drop(healthy);
    support::stopped(pid_b).await?;
    session.shutdown().await
}

#[tokio::test]
#[ignore = "explicit private large-response stalled-reader qualification"]
async fn stalled_reader_preserves_healthy_owner_single_worker() -> Result<()> {
    stalled_reader_owner_isolation(1).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit private large-response stalled-reader qualification"]
async fn stalled_reader_preserves_healthy_owner_two_workers() -> Result<()> {
    stalled_reader_owner_isolation(2).await
}
