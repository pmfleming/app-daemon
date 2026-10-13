//! One declaration binds live DTOs, handler dispatch, schemas, policies and replies.
use super::{
    replies::{ErrorCode, Reply, Settings},
    stream,
};
use crate::{
    api::{ApiError, ApiService, decode},
    model::{
        ApplicationEnergyOverview, ApplicationPage, ApplicationResourceHistory, OperationResult,
    },
    service::{
        ApplicationAction, EnergyOverviewParams, ExecuteParams, QueryParams, ResourceHistoryParams,
        UpdateSettingsParams,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shelllist_daemon_core::{ExecutionMode, MethodPolicy, Sensitivity};

#[derive(Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperationStatusParams {
    pub operation_id: String,
}

const READ: MethodPolicy = MethodPolicy::new(
    ExecutionMode::Read,
    Sensitivity::SensitiveRead,
    "applications",
);
const EFFECT: MethodPolicy =
    MethodPolicy::new(ExecutionMode::Effect, Sensitivity::Effect, "applications");
const OPERATION: MethodPolicy = MethodPolicy {
    ack_budget_ms: 1000,
    ..MethodPolicy::new(
        ExecutionMode::Operation,
        Sensitivity::Effect,
        "applications",
    )
};

shelllist_daemon_core::declare_methods! {
    api: ApiService, error: ApiError, reply: Reply,
    decode: decode, unsupported: |name| (ErrorCode::UnsupportedMethod, format!("Unsupported app-api method: {name}"));
    Query {
        name: "applications.query", params: QueryParams, result: ApplicationPage, handler: query,
        reply: |applications| Reply::Applications { applications }, response_key: "applications",
        policy: READ, stream: stream::APPLICATIONS,
        example: QueryParams { query: String::new(), category: String::new(), generation: 1, limit: 500 },
    }
    Revision {
        name: "applications.revision", params: Value, result: u64, handler: revision,
        reply: |revision| Reply::Revision { revision }, response_key: "revision",
        policy: READ, stream: stream::APPLICATIONS, example: json!({}),
    }
    History {
        name: "applications.history", params: ResourceHistoryParams, result: ApplicationResourceHistory, handler: history,
        reply: |history| Reply::History { history }, response_key: "history",
        policy: READ, stream: stream::APPLICATIONS,
        example: ResourceHistoryParams { target_id: "org.example.App.desktop".into(), since_ms: Some(0), until_ms: Some(15000), cursor: None, limit: 1000 },
        pagination: json!({"kind":"opaque-cursor", "request_field":"cursor", "response_field":"next_cursor", "order":"oldest-first", "summary_field":"summary", "summary_scope":"full-selected-window", "summary_weighting":"observed-duration", "summary_revision_field":"revision"}),
    }
    EnergyOverview {
        name: "applications.energyOverview", params: EnergyOverviewParams, result: ApplicationEnergyOverview, handler: energy_overview,
        reply: |energy_overview| Reply::EnergyOverview { energy_overview }, response_key: "energy_overview",
        policy: READ, stream: stream::APPLICATIONS, example: EnergyOverviewParams { since_ms: 0, limit: 20 },
    }
    Refresh {
        name: "applications.refresh", params: QueryParams, result: ApplicationPage, handler: refresh,
        reply: |applications| Reply::Applications { applications }, response_key: "applications",
        policy: EFFECT, stream: stream::APPLICATIONS,
        example: QueryParams { query: String::new(), category: String::new(), generation: 1, limit: 500 },
    }
    Execute {
        name: "applications.execute", params: ExecuteParams, result: OperationResult, handler: execute,
        reply: |operation| Reply::Operation { operation }, response_key: "operation",
        policy: OPERATION, stream: stream::OPERATION,
        example: ExecuteParams { target_id: "org.example.App.desktop".into(), action: ApplicationAction::Activate, window_id: None, desktop_action_id: None, expected_revision: Some(1), workspace_id: Some("2".into()) },
    }
    OperationStatus {
        name: "applications.operation.status", params: OperationStatusParams, result: OperationResult, handler: operation_status,
        reply: |operation_status| Reply::OperationStatus { operation_status }, response_key: "operation_status",
        policy: READ, stream: stream::OPERATION,
        example: OperationStatusParams { operation_id: "operation-example".into() },
    }
    UpdateSettings {
        name: "applications.settings.update", params: UpdateSettingsParams, result: Settings, handler: update_settings,
        reply: |settings| Reply::Settings { settings }, response_key: "settings",
        policy: EFFECT, stream: stream::APPLICATIONS,
        example: UpdateSettingsParams { target_id: "org.example.App.desktop".into(), category: "code".into() },
    }
}
