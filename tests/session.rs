mod support;

use anyhow::{Context, Result};
use app_daemon::api::{BUS_NAME, INTERFACE, OBJECT_PATH};
use serde_json::json;
use support::{Session, call, cancel, events, operation, raw_call, running, stopped};

#[tokio::test]
async fn dbus_validation_and_subscription_cancellation_are_owner_scoped() -> Result<()> {
    let mut session = Session::start(true).await?;
    let proxy = session.proxy().await?;
    for (method, params, code) in [
        ("unknown.method", "{}", "unsupported-method"),
        ("applications.query", "{", "validation-error"),
        (
            "applications.query",
            r#"{"limit":"wrong"}"#,
            "validation-error",
        ),
        (
            "applications.history",
            r#"{"target_id":"ok.desktop","cursor":"bad cursor"}"#,
            "validation-error",
        ),
    ] {
        let response = raw_call(&proxy, method, params).await?;
        assert_eq!(response["ok"], false, "{response}");
        assert_eq!(response["error"]["code"], code);
    }
    let invalid: String = proxy.call("Subscribe", &(vec!["unknown.stream"],)).await?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&invalid)?["error"]["code"],
        "unsupported-stream"
    );
    let subscribed: String = proxy
        .call("Subscribe", &(vec!["applications.operation"],))
        .await?;
    let subscribed: serde_json::Value = serde_json::from_str(&subscribed)?;
    let id = subscribed["data"]["subscription"]["id"]
        .as_str()
        .context("subscription id")?;
    let other = session.other_connection().await?;
    let stranger = zbus::Proxy::new(&other, BUS_NAME, OBJECT_PATH, INTERFACE).await?;
    assert_eq!(
        cancel(&stranger, id).await?["error"]["code"],
        "request-not-found"
    );
    assert_eq!(cancel(&proxy, id).await?["data"]["kind"], "subscription");
    assert_eq!(
        cancel(&proxy, id).await?["error"]["code"],
        "request-not-found"
    );
    drop(proxy);
    session.shutdown().await
}

#[tokio::test]
async fn uwsm_handoffs_report_receipts_failures_timeouts_and_owned_cancellation() -> Result<()> {
    let mut session = Session::start(true).await?;
    let proxy = session.proxy().await?;
    let mut stream = events(&proxy).await?;
    for (target, action, expected) in [
        ("ok.desktop", "launch", "ok.desktop"),
        ("ok.desktop", "desktop-action", "ok.desktop:inspect"),
    ] {
        let accepted = call(
            &proxy,
            "applications.execute",
            json!({
                "target_id": target, "action": action, "desktop_action_id": "inspect"
            }),
        )
        .await?;
        let id = accepted["data"]["operation"]["id"]
            .as_str()
            .context("accepted operation")?;
        assert_eq!(accepted["data"]["operation"]["status"], "accepted");
        let completed = operation(&mut stream, id, "completed").await?;
        assert_eq!(completed["launch_backend"], "uwsm-app");
        assert_eq!(completed["launch_scope"], "app-graphical.slice");
        assert_eq!(
            std::fs::read_to_string(session.state_path("arguments"))?,
            format!("-t\nservice\n--\n{expected}\n")
        );
    }
    for (target, message) in [
        ("fail.desktop", "fixture launch rejected"),
        ("slow.desktop", "timed out"),
    ] {
        let accepted = call(
            &proxy,
            "applications.execute",
            json!({"target_id": target, "action": "launch"}),
        )
        .await?;
        let id = accepted["data"]["operation"]["id"]
            .as_str()
            .context("accepted failure")?;
        let failed = operation(&mut stream, id, "failed").await?;
        assert!(
            failed["message"]
                .as_str()
                .context("failure message")?
                .contains(message),
            "{failed}"
        );
    }
    stopped(session.pid("handoff.pid").await?).await?;
    std::fs::remove_file(session.state_path("handoff.pid"))?;
    let accepted = call(
        &proxy,
        "applications.execute",
        json!({"target_id":"slow.desktop","action":"launch"}),
    )
    .await?;
    let id = accepted["data"]["operation"]["id"]
        .as_str()
        .context("cancellable operation")?;
    let pid = session.pid("handoff.pid").await?;
    let other = session.other_connection().await?;
    let stranger = zbus::Proxy::new(&other, BUS_NAME, OBJECT_PATH, INTERFACE).await?;
    assert_eq!(
        cancel(&stranger, id).await?["error"]["code"],
        "request-not-found"
    );
    assert!(running(pid));
    assert_eq!(cancel(&proxy, id).await?["data"]["kind"], "operation");
    operation(&mut stream, id, "cancelled").await?;
    stopped(pid).await?;
    assert_eq!(
        cancel(&proxy, id).await?["error"]["code"],
        "request-not-found"
    );
    drop(proxy);
    session.shutdown().await
}

#[tokio::test]
async fn direct_launch_detaches_and_survives_daemon_shutdown() -> Result<()> {
    let mut session = Session::start(false).await?;
    let proxy = session.proxy().await?;
    let mut stream = events(&proxy).await?;
    let accepted = call(
        &proxy,
        "applications.execute",
        json!({"target_id":"ok.desktop","action":"launch"}),
    )
    .await?;
    let id = accepted["data"]["operation"]["id"]
        .as_str()
        .context("direct operation")?;
    let completed = operation(&mut stream, id, "completed").await?;
    assert_eq!(completed["launch_backend"], "direct");
    let pid = session.pid("direct.pid").await?;
    assert!(running(pid), "completion must not wait for the app to exit");
    drop(proxy);
    session.shutdown().await?;
    assert!(
        running(pid),
        "shutdown must leave a launched application running"
    );
    Ok(())
}

#[tokio::test]
async fn shutdown_terminates_pending_handoffs_and_persists_history() -> Result<()> {
    let mut session = Session::start(true).await?;
    let proxy = session.proxy().await?;
    let accepted = call(
        &proxy,
        "applications.execute",
        json!({"target_id":"slow.desktop","action":"launch"}),
    )
    .await?;
    assert_eq!(accepted["ok"], true);
    let pid = session.pid("handoff.pid").await?;
    drop(proxy);
    session.shutdown().await?;
    stopped(pid).await
}
