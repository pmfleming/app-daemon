//! App pilot: one declaration binds wire names, live DTOs, handlers and replies.
//! Policies describe existing semantics; they do not introduce retry/deadline middleware.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPolicy {
    Read,
    Refresh,
    AcceptedOperation,
    SettingsMutation,
}

// Keep this actual Serde type name: it is part of v1 malformed-request diagnostics.
#[derive(Deserialize, Serialize)]
pub(crate) struct OperationStatusParams {
    pub operation_id: String,
}

macro_rules! declare_methods {
    ($($variant:ident {
        name: $name:literal,
        params: $params:ty,
        result: $result:ty,
        handler: $handler:ident,
        reply: $reply:ident.$field:ident,
        policy: $policy:ident,
        stream: $stream:expr,
        example: $example:expr,
        $(pagination: $pagination:expr,)?
    })+) => {
        pub const METHODS: &[&str] = &[$($name),+];

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Method { $($variant),+ }
        impl Method {
            pub const ALL: &[Self] = &[$(Self::$variant),+];
            #[deny(unreachable_patterns)]
            pub fn parse(name: &str) -> Option<Self> {
                match name { $($name => Some(Self::$variant),)+ _ => None }
            }
            pub const fn name(self) -> &'static str {
                match self { $(Self::$variant => $name),+ }
            }
            pub const fn policy(self) -> ExecutionPolicy {
                match self { $(Self::$variant => ExecutionPolicy::$policy),+ }
            }
            pub const fn response_key(self) -> &'static str {
                match self { $(Self::$variant => stringify!($field)),+ }
            }
            pub fn registry_entry(self) -> Value {
                match self { $(Self::$variant => {
                    // The example must have the live parameter type, not just
                    // look like it after serialization.
                    let params: $params = $example;
                    json!({"name": $name, "params_example": params,
                        "response_key": stringify!($field), "stream": $stream
                        $(, "pagination": $pagination)?})
                }),+ }
            }
        }

        pub(crate) enum Request { $($variant($params)),+ }
        impl Request {
            pub(crate) fn decode(name: &str, params: Value) -> Result<Self, ApiError> {
                let method = Method::parse(name).ok_or_else(|| (
                    ErrorCode::UnsupportedMethod, format!("Unsupported app-api method: {name}")
                ))?;
                match method { $(Method::$variant => decode(params).map(Self::$variant)),+ }
            }
            pub(crate) async fn dispatch(self, api: &ApiService, owner: Option<String>) -> Result<Reply, ApiError> {
                match self { $(Self::$variant(params) => {
                    // Both the input and output must match the declaration.
                    let result: Result<$result, ApiError> = api.$handler(params, owner).await;
                    result.map($variant::reply)
                }),+ }
            }
        }

        $(pub(crate) struct $variant;
        impl $variant {
            // Also used by offline fixtures: a mismatched payload/key cannot
            // silently agree with a separate handwritten serializer.
            pub(crate) fn reply(value: $result) -> Reply { Reply::$reply { $field: value } }
        })+
    };
}

declare_methods! {
    Query {
        name: "applications.query",
        params: QueryParams,
        result: ApplicationPage,
        handler: query,
        reply: Applications.applications,
        policy: Read,
        stream: stream::APPLICATIONS,
        example: QueryParams { query: String::new(), category: String::new(), generation: 1, limit: 500 },
    }
    Revision {
        name: "applications.revision",
        // v1 deliberately ignores parameters, including non-object JSON.
        params: Value,
        result: u64,
        handler: revision,
        reply: Revision.revision,
        policy: Read,
        stream: stream::APPLICATIONS,
        example: json!({}),
    }
    History {
        name: "applications.history",
        params: ResourceHistoryParams,
        result: ApplicationResourceHistory,
        handler: history,
        reply: History.history,
        policy: Read,
        stream: stream::APPLICATIONS,
        example: ResourceHistoryParams { target_id: "org.example.App.desktop".into(), since_ms: Some(0), until_ms: Some(15000), cursor: None, limit: 1000 },
        pagination: json!({"kind":"opaque-cursor", "request_field":"cursor", "response_field":"next_cursor", "order":"oldest-first", "summary_field":"summary", "summary_scope":"full-selected-window", "summary_weighting":"observed-duration", "summary_revision_field":"revision"}),
    }
    EnergyOverview {
        name: "applications.energyOverview",
        params: EnergyOverviewParams,
        result: ApplicationEnergyOverview,
        handler: energy_overview,
        reply: EnergyOverview.energy_overview,
        policy: Read,
        stream: stream::APPLICATIONS,
        example: EnergyOverviewParams { since_ms: 0, limit: 20 },
    }
    Refresh {
        name: "applications.refresh",
        params: QueryParams,
        result: ApplicationPage,
        handler: refresh,
        reply: Applications.applications,
        policy: Refresh,
        stream: stream::APPLICATIONS,
        example: QueryParams { query: String::new(), category: String::new(), generation: 1, limit: 500 },
    }
    Execute {
        name: "applications.execute",
        params: ExecuteParams,
        result: OperationResult,
        handler: execute,
        reply: Operation.operation,
        policy: AcceptedOperation,
        stream: stream::OPERATION,
        example: ExecuteParams { target_id: "org.example.App.desktop".into(), action: ApplicationAction::Activate, window_id: None, desktop_action_id: None, expected_revision: Some(1), workspace_id: Some("2".into()) },
    }
    OperationStatus {
        name: "applications.operation.status",
        params: OperationStatusParams,
        result: OperationResult,
        handler: operation_status,
        reply: OperationStatus.operation_status,
        policy: Read,
        stream: stream::OPERATION,
        example: OperationStatusParams { operation_id: "operation-example".into() },
    }
    UpdateSettings {
        name: "applications.settings.update",
        params: UpdateSettingsParams,
        result: Settings,
        handler: update_settings,
        reply: Settings.settings,
        policy: SettingsMutation,
        stream: stream::APPLICATIONS,
        example: UpdateSettingsParams { target_id: "org.example.App.desktop".into(), category: "code".into() },
    }
}
