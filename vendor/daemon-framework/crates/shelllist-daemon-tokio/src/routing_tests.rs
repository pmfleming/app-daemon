use anyhow::Result;
use serde_json::{Value, json};
use shelllist_daemon_core::{ClientRoute, RouteKind};
use tokio::io::sink;

use super::{BasicCorrelation, OutputCommand, OutputState, emit_command};

fn route(consumer: &str, generation: u64, kind: RouteKind) -> ClientRoute {
    ClientRoute {
        consumer_id: consumer.into(),
        local_id: "same-local-id".into(),
        generation,
        kind,
    }
}

fn subscription(id: &str, owner: ClientRoute) -> OutputCommand {
    OutputCommand::Response {
        id: "subscribe".into(),
        result: Ok(json!({ "data": { "subscription": { "id": id } } })),
        cancelled_request_id: None,
        route: Some(owner),
    }
}

fn lines(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn replies_echo_routes_without_retaining_ordinary_requests() -> Result<()> {
    let mut state = OutputState::new(BasicCorrelation, 16);
    let mut writer = Vec::new();
    for (consumer, result) in [
        ("a", Ok(json!({ "ok": true }))),
        ("b", Err("refused".into())),
    ] {
        emit_command(
            &mut writer,
            &mut state,
            OutputCommand::Response {
                id: "same-local-id".into(),
                result,
                cancelled_request_id: None,
                route: Some(route(consumer, 7, RouteKind::Call)),
            },
        )
        .await?;
    }
    let output = lines(&writer);
    assert_eq!(output[0]["route"]["consumerId"], "a");
    assert_eq!(output[1]["route"]["consumerId"], "b");
    assert_eq!(output[1]["route"]["generation"], 7);
    assert_eq!(output[1]["error"], "refused");
    assert!(state.subscription_routes.is_empty());
    assert!(state.active_ids.is_empty());
    Ok(())
}

#[tokio::test]
async fn early_events_follow_the_addressed_reply_and_stay_owner_scoped() -> Result<()> {
    let mut state = OutputState::new(BasicCorrelation, 16);
    let mut writer = Vec::new();
    emit_command(
        &mut writer,
        &mut state,
        OutputCommand::Event {
            stream: "updates".into(),
            event: json!({ "subscription_id": "sub-a" }),
        },
    )
    .await?;
    assert!(writer.is_empty());
    emit_command(
        &mut writer,
        &mut state,
        subscription("sub-a", route("a", 1, RouteKind::BaseSubscription)),
    )
    .await?;
    emit_command(
        &mut writer,
        &mut state,
        subscription("sub-b", route("b", 1, RouteKind::Subscription)),
    )
    .await?;
    emit_command(
        &mut writer,
        &mut state,
        OutputCommand::Event {
            stream: "updates".into(),
            event: json!({ "subscription_id": "sub-b" }),
        },
    )
    .await?;
    let output = lines(&writer);
    assert_eq!(output.len(), 4);
    assert_eq!(output[0]["kind"], "response");
    assert_eq!(output[1]["kind"], "event");
    assert_eq!(output[1]["route"]["consumerId"], "a");
    assert_eq!(output[3]["route"]["consumerId"], "b");
    assert_eq!(
        state.owned_ids(&route("a", 1, RouteKind::Control)),
        ["sub-a"]
    );
    assert!(
        state
            .owned_ids(&route("a", 2, RouteKind::Control))
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn cancellation_failure_retains_ownership_success_and_reset_remove_it() -> Result<()> {
    let mut state = OutputState::new(BasicCorrelation, 16);
    let mut writer = Vec::new();
    emit_command(
        &mut writer,
        &mut state,
        subscription("sub-a", route("a", 1, RouteKind::Subscription)),
    )
    .await?;
    emit_command(
        &mut writer,
        &mut state,
        OutputCommand::Response {
            id: "cancel".into(),
            result: Err("temporarily unavailable".into()),
            cancelled_request_id: None,
            route: Some(route("a", 1, RouteKind::Control)),
        },
    )
    .await?;
    assert_eq!(
        state.owned_ids(&route("a", 1, RouteKind::Control)),
        ["sub-a"]
    );
    emit_command(
        &mut writer,
        &mut state,
        OutputCommand::Cancelled("sub-a".into()),
    )
    .await?;
    assert!(state.subscription_routes.is_empty());
    let length = writer.len();
    emit_command(
        &mut writer,
        &mut state,
        OutputCommand::Event {
            stream: "updates".into(),
            event: json!({ "subscription_id": "sub-a" }),
        },
    )
    .await?;
    assert_eq!(
        writer.len(),
        length,
        "late cancelled events must be suppressed"
    );
    emit_command(
        &mut writer,
        &mut state,
        subscription("sub-b", route("b", 2, RouteKind::Subscription)),
    )
    .await?;
    emit_command(&mut writer, &mut state, OutputCommand::ResetCorrelation).await?;
    assert!(state.subscription_routes.is_empty());
    assert!(state.active_ids.is_empty());
    assert!(state.pending_events.is_empty());
    Ok(())
}

#[tokio::test]
async fn twenty_thousand_requests_and_subscription_churn_stay_bounded() -> Result<()> {
    let mut state = OutputState::new(BasicCorrelation, 16);
    let mut writer = sink();
    for index in 0..20_000 {
        let owner = route("clipboard", index / 5, RouteKind::Call);
        emit_command(
            &mut writer,
            &mut state,
            OutputCommand::Response {
                id: format!("query-{}", index / 5),
                result: Ok(json!({ "ok": true })),
                cancelled_request_id: None,
                route: Some(owner),
            },
        )
        .await?;
        assert!(state.subscription_routes.is_empty());
        if index % 5 == 0 {
            let id = format!("sub-{index}");
            emit_command(
                &mut writer,
                &mut state,
                subscription(&id, route("clipboard", index, RouteKind::BaseSubscription)),
            )
            .await?;
            emit_command(&mut writer, &mut state, OutputCommand::Cancelled(id)).await?;
        }
        assert!(state.active_ids.is_empty());
        assert!(state.suppressed_ids.len() <= 16);
        assert!(state.suppressed_order.len() <= 16);
    }
    Ok(())
}
