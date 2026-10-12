//! Typed app-api v1 reply bodies. The transport envelope remains unchanged.
use serde::Serialize;

use crate::model::{
    ApplicationEnergyOverview, ApplicationPage, ApplicationResourceHistory, OperationResult,
};

#[derive(Serialize)]
#[serde(untagged)]
pub enum Reply {
    Applications {
        applications: ApplicationPage,
    },
    Revision {
        revision: u64,
    },
    History {
        history: ApplicationResourceHistory,
    },
    EnergyOverview {
        energy_overview: ApplicationEnergyOverview,
    },
    Settings {
        settings: Settings,
    },
    Operation {
        operation: OperationResult,
    },
    OperationStatus {
        operation_status: OperationResult,
    },
    Subscription {
        subscription: Subscription,
    },
    Cancelled {
        cancelled: String,
        kind: RequestKind,
    },
}

#[derive(Serialize)]
pub struct Settings {
    pub target_id: String,
    pub category: String,
    pub workspace_id: Option<String>,
}

#[derive(Serialize)]
pub struct Subscription {
    pub id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestKind {
    Subscription,
    Operation,
}

#[derive(Debug, Clone, Copy)]
pub enum ErrorCode {
    ValidationError,
    DaemonUnavailable,
    UnsupportedMethod,
    OperationFailed,
    RequestNotFound,
    UnsupportedStream,
    SubscriptionUnavailable,
}

impl ErrorCode {
    pub const ALL: &[Self] = &[
        Self::ValidationError,
        Self::DaemonUnavailable,
        Self::UnsupportedMethod,
        Self::OperationFailed,
        Self::RequestNotFound,
        Self::UnsupportedStream,
        Self::SubscriptionUnavailable,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ValidationError => "validation-error",
            Self::DaemonUnavailable => "daemon-unavailable",
            Self::UnsupportedMethod => "unsupported-method",
            Self::OperationFailed => "operation-failed",
            Self::RequestNotFound => "request-not-found",
            Self::UnsupportedStream => "unsupported-stream",
            Self::SubscriptionUnavailable => "subscription-unavailable",
        }
    }
}
