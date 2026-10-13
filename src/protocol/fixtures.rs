//! Offline examples use the same reply DTOs and envelope constructors as dispatch.
use serde_json::Value;

use super::{
    methods::{self, Method},
    replies::{ErrorCode, Reply, RequestKind, Settings, Subscription},
};
use crate::{
    api,
    model::{
        ApplicationEnergyOverview, ApplicationIdentity, ApplicationPage,
        ApplicationResourceHistory, ApplicationRuntime, ApplicationSummary, HistorySummary,
        OperationResult, ResourceUsage,
    },
};

pub(super) fn operation(status: &str) -> OperationResult {
    OperationResult {
        id: "operation-example".into(),
        action: "activate".into(),
        target_id: "Alpha".into(),
        status: status.into(),
        message: format!("Application action {status}"),
        close: None,
        launch_backend: None,
        launch_scope: None,
        placement: None,
    }
}

fn page() -> ApplicationPage {
    ApplicationPage {
        revision: 41,
        generation: 7,
        has_more: false,
        hyprland_available: true,
        applications: vec![ApplicationSummary {
            identity: ApplicationIdentity {
                id: "Alpha".into(),
                kind: "desktop-application".into(),
                name: "Alpha".into(),
                generic_name: "Example application".into(),
                comment: String::new(),
                icon: String::new(),
                keywords: vec![],
                categories: vec![],
                category: "shell".into(),
                default_workspace_id: Some("1".into()),
                startup_class: "Alpha".into(),
            },
            revision: 41,
            runtime: ApplicationRuntime {
                running: false,
                focused: false,
                running_count: 0,
                resources: ResourceUsage::default(),
                instances: vec![],
            },
            desktop_actions: vec![],
            match_score: 0,
            match_kind: "all".into(),
            runtime_score: 0,
            score: 0,
        }],
    }
}

pub(super) fn replies() -> Value {
    let mut fixture = serde_json::Map::new();
    for (method, reply) in [
        (Method::Query.name(), methods::Query::reply(page())),
        (Method::Refresh.name(), methods::Refresh::reply(page())),
        (Method::Revision.name(), methods::Revision::reply(41)),
        (
            Method::History.name(),
            methods::History::reply(ApplicationResourceHistory {
                target_id: "Alpha".into(),
                summary: HistorySummary {
                    revision: "history-example".into(),
                    ..Default::default()
                },
                points: vec![],
                has_more: false,
                next_cursor: None,
            }),
        ),
        (
            Method::EnergyOverview.name(),
            methods::EnergyOverview::reply(ApplicationEnergyOverview::default()),
        ),
        (
            Method::UpdateSettings.name(),
            methods::UpdateSettings::reply(Settings {
                target_id: "Alpha".into(),
                category: "code".into(),
                workspace_id: Some("3".into()),
            }),
        ),
        (
            Method::Execute.name(),
            methods::Execute::reply(operation("accepted")),
        ),
        (
            Method::OperationStatus.name(),
            methods::OperationStatus::reply(operation("completed")),
        ),
        (
            "subscribe",
            Reply::Subscription {
                subscription: Subscription {
                    id: "subscription-example".into(),
                },
            },
        ),
        (
            "cancel-subscription",
            Reply::Cancelled {
                cancelled: "subscription-example".into(),
                kind: RequestKind::Subscription,
            },
        ),
        (
            "cancel-operation",
            Reply::Cancelled {
                cancelled: "operation-example".into(),
                kind: RequestKind::Operation,
            },
        ),
    ] {
        fixture.insert(method.into(), api::success(reply));
    }
    fixture.insert(
        "errors".into(),
        Value::Object(
            ErrorCode::ALL
                .iter()
                .map(|code| {
                    (
                        code.as_str().into(),
                        api::error(*code, format!("Example {}", code.as_str())),
                    )
                })
                .collect(),
        ),
    );
    Value::Object(fixture)
}
