//! Offline examples use the same reply DTOs and envelope constructors as dispatch.
use serde_json::Value;

use super::replies::{ErrorCode, Reply, RequestKind, Settings, Subscription};
use crate::{api, model::*};

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
        (
            "applications.query",
            Reply::Applications {
                applications: page(),
            },
        ),
        (
            "applications.refresh",
            Reply::Applications {
                applications: page(),
            },
        ),
        ("applications.revision", Reply::Revision { revision: 41 }),
        (
            "applications.history",
            Reply::History {
                history: ApplicationResourceHistory {
                    target_id: "Alpha".into(),
                    summary: HistorySummary {
                        revision: "history-example".into(),
                        ..Default::default()
                    },
                    points: vec![],
                    has_more: false,
                    next_cursor: None,
                },
            },
        ),
        (
            "applications.energyOverview",
            Reply::EnergyOverview {
                energy_overview: ApplicationEnergyOverview::default(),
            },
        ),
        (
            "applications.settings.update",
            Reply::Settings {
                settings: Settings {
                    target_id: "Alpha".into(),
                    category: "code".into(),
                    workspace_id: Some("3".into()),
                },
            },
        ),
        (
            "applications.execute",
            Reply::Operation {
                operation: operation("accepted"),
            },
        ),
        (
            "applications.operation.status",
            Reply::OperationStatus {
                operation_status: operation("completed"),
            },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_has_a_producer_reply_and_error_envelopes_keep_v1_shape() {
        let fixture = replies();
        for method in super::super::METHODS {
            let reply = &fixture[method];
            assert_eq!(reply["protocol"], "app-api", "{method}");
            assert_eq!(reply["version"], 1);
            assert_eq!(reply["ok"], true);
            assert!(reply["data"].is_object());
            assert!(reply.get("error").is_none());
        }
        for code in ErrorCode::ALL {
            let reply = &fixture["errors"][code.as_str()];
            assert_eq!(reply["ok"], false);
            assert_eq!(reply["error"]["code"], code.as_str());
            assert_eq!(reply["error"]["retryable"], false);
            assert!(reply.get("data").is_none());
        }
        assert!(
            fixture["applications.execute"]["data"]["operation"]
                .get("placement")
                .is_none()
        );
        assert!(fixture["applications.history"]["data"]["history"]["next_cursor"].is_null());
        assert_eq!(fixture["cancel-operation"]["data"]["kind"], "operation");
        assert_eq!(
            fixture["cancel-subscription"]["data"]["kind"],
            "subscription"
        );
        assert_eq!(
            fixture["subscribe"]["data"]["subscription"]["id"],
            "subscription-example"
        );
    }
}
