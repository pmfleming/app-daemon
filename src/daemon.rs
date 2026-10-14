use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use shelllist_daemon_tokio::{OwnedTaskRegistry, directed_emitter};
use tokio::sync::broadcast;
use zbus::{connection, message::Header, object_server::SignalEmitter};

use crate::{
    api::{self, ApiService, BUS_NAME, OBJECT_PATH},
    protocol::{
        self,
        replies::{ErrorCode, Reply, RequestKind, Subscription},
    },
    service::{ApplicationService, StateRevision},
};

pub(crate) mod events;

pub struct AppDaemon {
    boundary: shelllist_daemon_tokio::ServiceBoundary,
    api: Arc<ApiService>,
    applications: Arc<ApplicationService>,
    subscriptions: Arc<OwnedTaskRegistry>,
}

#[zbus::interface(name = "org.laufan.AppDaemon2")]
impl AppDaemon {
    async fn call(
        &self,
        method: &str,
        params_json: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> String {
        let Some(declared) = protocol::methods::Method::parse(method) else {
            return api::error(
                ErrorCode::UnsupportedMethod,
                format!("Unsupported app-api method: {method}"),
            )
            .to_string();
        };
        let admitted = match self.boundary.call_policy(
            header.sender().map(|s| s.as_str()),
            method,
            params_json,
            declared.policy(),
        ) {
            Ok(admitted) => admitted,
            Err(response) => return response,
        };
        let params: Value = match serde_json::from_str(params_json) {
            Ok(value) => value,
            Err(error) => {
                return api::error(
                    ErrorCode::ValidationError,
                    format!("invalid params JSON: {error}"),
                )
                .to_string();
            }
        };
        let api = self.api.clone();
        let method = method.to_owned();
        let owner = header.sender().map(ToString::to_string);
        admitted
            .run(async move { api.dispatch_owned(&method, params, owner).await })
            .await
    }

    async fn subscribe(
        &self,
        streams: Vec<String>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> String {
        let _admitted = match self
            .boundary
            .subscribe(header.sender().map(|s| s.as_str()), &streams)
        {
            Ok(admitted) => admitted,
            Err(response) => return response,
        };
        let _permit = match self.applications.request_permit().await {
            Ok(permit) => permit,
            Err(error) => {
                return api::error(ErrorCode::DaemonUnavailable, error.to_string()).to_string();
            }
        };
        let selected = match selected_streams(&streams) {
            Ok(selected) => selected,
            Err(stream) => {
                return api::error(
                    ErrorCode::UnsupportedStream,
                    format!("Unsupported app-api stream: {stream}"),
                )
                .to_string();
            }
        };
        let id = self.subscriptions.next_id("subscription");
        let owner = header.sender().map(|owner| owner.to_owned());
        let destination = directed_emitter(&emitter, &header);
        let connection = destination.connection().clone();
        let changes = self.applications.subscribe_state();
        let operations = self.applications.subscribe_operations();
        let applications = Arc::clone(&self.applications);
        let events = forward_events(
            applications,
            changes,
            operations,
            destination,
            id.clone(),
            selected,
        );
        if let Err(error) = self.subscriptions.spawn_for_owner(
            id.clone(),
            owner.as_ref().map(ToString::to_string),
            &connection,
            events,
        ) {
            return api::error(ErrorCode::SubscriptionUnavailable, error.to_string()).to_string();
        }
        api::success(Reply::Subscription {
            subscription: Subscription { id },
        })
        .to_string()
    }

    async fn cancel(&self, request_id: &str, #[zbus(header)] header: Header<'_>) -> String {
        let _admitted = match self
            .boundary
            .cancel(header.sender().map(|s| s.as_str()), request_id)
        {
            Ok(admitted) => admitted,
            Err(response) => return response,
        };
        let owner = header.sender().map(ToString::to_string);
        if self
            .subscriptions
            .cancel_owned(request_id, owner.as_deref())
            .await
        {
            return api::success(Reply::Cancelled {
                cancelled: request_id.into(),
                kind: RequestKind::Subscription,
            })
            .to_string();
        }
        if self
            .applications
            .cancel_operation_owned(request_id, owner.as_deref())
            .await
            .is_some()
        {
            return api::success(Reply::Cancelled {
                cancelled: request_id.into(),
                kind: RequestKind::Operation,
            })
            .to_string();
        }
        api::error(
            ErrorCode::RequestNotFound,
            format!("No active request named {request_id}"),
        )
        .to_string()
    }

    #[zbus(signal)]
    async fn event(emitter: &SignalEmitter<'_>, stream: &str, event_json: &str)
    -> zbus::Result<()>;
}

fn selected_streams(streams: &[String]) -> Result<(bool, bool, bool), &str> {
    if let Some(stream) = streams
        .iter()
        .find(|stream| !protocol::STREAMS.contains(&stream.as_str()))
    {
        return Err(stream);
    }
    Ok((
        streams
            .iter()
            .any(|value| value == protocol::stream::APPLICATIONS),
        streams
            .iter()
            .any(|value| value == protocol::stream::WINDOWS),
        streams
            .iter()
            .any(|value| value == protocol::stream::OPERATION),
    ))
}

async fn forward_events(
    applications: Arc<ApplicationService>,
    mut changes: broadcast::Receiver<StateRevision>,
    mut operations: broadcast::Receiver<crate::model::OperationResult>,
    emitter: SignalEmitter<'static>,
    subscription_id: String,
    selected: (bool, bool, bool),
) {
    let mut previous = applications.revisions().await;
    for (stream, enabled) in [
        (protocol::stream::APPLICATIONS, selected.0),
        (protocol::stream::WINDOWS, selected.1),
        (protocol::stream::OPERATION, selected.2),
    ] {
        if enabled {
            events::emit(
                &emitter,
                events::subscribed(stream, &subscription_id, previous),
            )
            .await;
        }
    }
    loop {
        tokio::select! {
            state = changes.recv() => {
                let current = match state {
                    Ok(current) => current,
                    Err(broadcast::error::RecvError::Lagged(_)) => applications.revisions().await,
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                for (stream, revision) in revision_changes(selected, previous, current) {
                    events::emit(
                        &emitter,
                        events::changed(stream, &subscription_id, revision),
                    )
                    .await;
                }
                previous = current;
            }
            operation = operations.recv(), if selected.2 => {
                match operation {
                    Ok(operation) => {
                        events::emit(&emitter, events::operation(&subscription_id, &operation)).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        tracing::warn!(count, "application operation subscriber lagged");
                        events::emit(&emitter, events::operation_gap(&subscription_id, count)).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }
}

fn revision_changes(
    selected: (bool, bool, bool),
    previous: StateRevision,
    current: StateRevision,
) -> impl Iterator<Item = (&'static str, u64)> {
    [
        (
            selected.0
                && (current.catalog != previous.catalog
                    || current.settings != previous.settings
                    || current.runtime != previous.runtime),
            protocol::stream::APPLICATIONS,
            current.catalog ^ current.settings ^ current.runtime,
        ),
        (
            selected.1 && current.windows != previous.windows,
            protocol::stream::WINDOWS,
            current.windows,
        ),
    ]
    .into_iter()
    .filter_map(|(changed, stream, revision)| changed.then_some((stream, revision)))
}

pub async fn run() -> Result<()> {
    let applications = ApplicationService::new();
    let shutdown_applications = Arc::clone(&applications);
    let subscriptions = Arc::new(OwnedTaskRegistry::default());
    let daemon = AppDaemon {
        boundary: shelllist_daemon_tokio::ServiceBoundary::new(
            shelllist_daemon_core::ApiIdentity::new(api::PROTOCOL, api::VERSION as u32),
        ),
        api: Arc::new(ApiService::new(Arc::clone(&applications))),
        applications,
        subscriptions: Arc::clone(&subscriptions),
    };
    let _connection = connection::Builder::session()
        .context("connect to session D-Bus")?
        .name(BUS_NAME)
        .context("claim app-daemon bus name")?
        .serve_at(OBJECT_PATH, daemon)
        .context("export app-daemon interface")?
        .build()
        .await
        .context("start app-daemon D-Bus service")?;
    tracing::info!(
        bus_name = BUS_NAME,
        object_path = OBJECT_PATH,
        "app-daemon started"
    );
    let result = shelllist_daemon_tokio::wait_for_shutdown().await;
    subscriptions.shutdown().await;
    shutdown_applications.shutdown().await;
    result
}
