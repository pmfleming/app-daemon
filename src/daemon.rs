use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use shelllist_daemon_tokio::{EventEmitter, OwnedTaskRegistry, directed_emitter};
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
mod socket;

pub struct AppDaemon {
    boundary: shelllist_daemon_tokio::ServiceBoundary,
    api: Arc<ApiService>,
    applications: Arc<ApplicationService>,
    subscriptions: Arc<OwnedTaskRegistry>,
}

impl AppDaemon {
    async fn call_owned(&self, method: &str, params_json: &str, owner: Option<String>) -> String {
        let Some(declared) = protocol::methods::Method::parse(method) else {
            return api::error(
                ErrorCode::UnsupportedMethod,
                format!("Unsupported app-api method: {method}"),
            )
            .to_string();
        };
        let admitted = match self.boundary.call_policy(
            owner.as_deref(),
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
        let method = declared.name();
        admitted
            .run(async move { api.dispatch_owned(method, params, owner).await })
            .await
    }

    async fn subscribe_owned(
        &self,
        streams: Vec<String>,
        owner: Option<String>,
        destination: EventEmitter<'static>,
        stop: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> String {
        let _admitted = match self.boundary.subscribe(owner.as_deref(), &streams) {
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
            owner.clone(),
        );
        if let Err(error) = self
            .subscriptions
            .spawn_until(id.clone(), owner, stop, events)
        {
            return api::error(ErrorCode::SubscriptionUnavailable, error.to_string()).to_string();
        }
        api::success(Reply::Subscription {
            subscription: Subscription { id },
        })
        .to_string()
    }

    async fn cancel_owned(&self, request_id: &str, owner: Option<&str>) -> String {
        let _admitted = match self.boundary.cancel(owner, request_id) {
            Ok(admitted) => admitted,
            Err(response) => return response,
        };
        if self.subscriptions.cancel_owned(request_id, owner).await {
            return api::success(Reply::Cancelled {
                cancelled: request_id.into(),
                kind: RequestKind::Subscription,
            })
            .to_string();
        }
        if self
            .applications
            .cancel_operation_owned(request_id, owner)
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
}

#[zbus::interface(name = "org.laufan.AppDaemon2")]
impl AppDaemon {
    async fn call(
        &self,
        method: &str,
        params_json: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> String {
        self.call_owned(
            method,
            params_json,
            header.sender().map(ToString::to_string),
        )
        .await
    }

    async fn subscribe(
        &self,
        streams: Vec<String>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> String {
        let owner = header.sender().map(ToString::to_string);
        let watched = owner.clone();
        let monitor = self.subscriptions.owner_monitor(emitter.connection());
        self.subscribe_owned(
            streams,
            owner,
            directed_emitter(&emitter, &header).into(),
            async move {
                if let Some(owner) = watched {
                    let _ = monitor.wait(&owner).await;
                } else {
                    std::future::pending::<()>().await;
                }
            },
        )
        .await
    }

    async fn cancel(&self, request_id: &str, #[zbus(header)] header: Header<'_>) -> String {
        self.cancel_owned(request_id, header.sender().map(|s| s.as_str()))
            .await
    }

    #[zbus(signal)]
    async fn event(emitter: &SignalEmitter<'_>, stream: &str, event_json: &str)
    -> zbus::Result<()>;
}

fn selected_streams(streams: &[String]) -> Result<(bool, bool, bool, bool), &str> {
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
        streams
            .iter()
            .any(|value| value == protocol::stream::CATALOG),
    ))
}

async fn forward_events(
    applications: Arc<ApplicationService>,
    mut changes: broadcast::Receiver<StateRevision>,
    mut operations: broadcast::Receiver<crate::model::OperationResult>,
    emitter: EventEmitter<'static>,
    subscription_id: String,
    selected: (bool, bool, bool, bool),
    owner: Option<String>,
) {
    let mut previous = applications.revisions().await;
    for (stream, enabled) in [
        (protocol::stream::APPLICATIONS, selected.0),
        (protocol::stream::WINDOWS, selected.1),
        (protocol::stream::OPERATION, selected.2),
        (protocol::stream::CATALOG, selected.3),
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
                        // Subscription visibility follows the same ownership gate as status.
                        if applications.operation_status_owned(&operation.id, owner.as_deref()).await.is_some() {
                            events::emit(&emitter, events::operation(&subscription_id, &operation)).await;
                        }
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
    selected: (bool, bool, bool, bool),
    previous: StateRevision,
    current: StateRevision,
) -> impl Iterator<Item = (&'static str, u64)> {
    [
        (
            selected.3
                && (current.catalog != previous.catalog || current.settings != previous.settings),
            protocol::stream::CATALOG,
            current.catalog ^ current.settings,
        ),
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
    // Register before publishing D-Bus readiness. A client may request shutdown
    // while launch recovery is still running; retain that signal for cleanup
    // instead of allowing the default SIGTERM action to kill the process.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("listen for SIGTERM")?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("listen for SIGINT")?;
    let applications = ApplicationService::new().await?;
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
    let connection = connection::Builder::session()
        .context("connect to session D-Bus")?
        .name(BUS_NAME)
        .context("claim app-daemon bus name")?
        .serve_at(OBJECT_PATH, daemon)
        .context("export app-daemon interface")?
        .build()
        .await
        .context("start app-daemon D-Bus service")?;
    shutdown_applications.recover_launches().await;
    tracing::info!(
        bus_name = BUS_NAME,
        object_path = OBJECT_PATH,
        "app-daemon started"
    );
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let mut listener = tokio::spawn(socket::listen(connection.clone(), async {
        let _ = stopped.await;
    }));
    let (mut result, joined) = tokio::select! {
        _ = terminate.recv() => (Ok(()), false),
        _ = interrupt.recv() => (Ok(()), false),
        result = &mut listener => (result.context("join app socket listener").and_then(|r| r), true),
    };
    let _ = stop.send(());
    if !joined {
        result = result.and(
            listener
                .await
                .context("join app socket shutdown")
                .and_then(|r| r),
        );
    }
    subscriptions.shutdown().await;
    shutdown_applications.shutdown().await;
    result
}
