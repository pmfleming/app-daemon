use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use shelllist_daemon_tokio::{OwnedTaskRegistry, directed_emitter, wait_for_owner_loss};
use tokio::sync::{broadcast, oneshot};
use zbus::{connection, message::Header, object_server::SignalEmitter};

use crate::{
    api::{self, ApiService, BUS_NAME, OBJECT_PATH},
    protocol,
    service::{ApplicationService, StateRevision},
};

pub struct AppDaemon {
    api: ApiService,
    applications: Arc<ApplicationService>,
    subscriptions: Arc<OwnedTaskRegistry>,
}

#[zbus::interface(name = "org.laufan.AppDaemon1")]
impl AppDaemon {
    async fn call(&self, method: &str, params_json: &str) -> String {
        let params: Value = match serde_json::from_str(params_json) {
            Ok(value) => value,
            Err(error) => {
                return api::error("validation-error", format!("invalid params JSON: {error}"))
                    .to_string();
            }
        };
        self.api.dispatch(method, params).await.to_string()
    }

    async fn subscribe(
        &self,
        streams: Vec<String>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> String {
        let selected = match selected_streams(&streams) {
            Ok(selected) => selected,
            Err(stream) => {
                return api::error(
                    "unsupported-stream",
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
        let subscriptions = Arc::clone(&self.subscriptions);
        let task_id = id.clone();
        let task_owner = owner.clone();
        let (start, ready) = oneshot::channel();
        let task = tokio::spawn(async move {
            if ready.await.is_err() {
                return;
            }
            let events = forward_events(
                applications,
                changes,
                operations,
                destination,
                task_id.clone(),
                selected,
            );
            match task_owner {
                Some(owner) => tokio::select! {
                    () = events => {}
                    _ = wait_for_owner_loss(&connection, owner) => {}
                },
                None => events.await,
            }
            subscriptions.remove(&task_id).await;
        });
        self.subscriptions
            .insert(id.clone(), owner.as_ref().map(ToString::to_string), task)
            .await;
        let _ = start.send(());
        api::success(json!({ "subscription": { "id": id } })).to_string()
    }

    async fn cancel(&self, request_id: &str, #[zbus(header)] header: Header<'_>) -> String {
        let owner = header.sender().map(ToString::to_string);
        if self
            .subscriptions
            .cancel_owned(request_id, owner.as_deref())
            .await
        {
            return api::success(json!({ "cancelled": request_id, "kind": "subscription" }))
                .to_string();
        }
        if self
            .applications
            .cancel_operation(request_id)
            .await
            .is_some()
        {
            return api::success(json!({ "cancelled": request_id, "kind": "operation" }))
                .to_string();
        }
        api::error(
            "request-not-found",
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
            emit_event(
                &emitter,
                stream,
                "subscribed",
                &subscription_id,
                json!({
                    "catalog_revision": previous.catalog,
                    "window_revision": previous.windows,
                    "settings_revision": previous.settings
                }),
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
                    emit_event(
                        &emitter,
                        stream,
                        "changed",
                        &subscription_id,
                        json!({ "revision": revision }),
                    )
                    .await;
                }
                previous = current;
            }
            operation = operations.recv(), if selected.2 => {
                match operation {
                    Ok(operation) => {
                        emit_event(
                            &emitter,
                            protocol::stream::OPERATION,
                            &operation.status,
                            &subscription_id,
                            json!({ "operation": operation }),
                        )
                        .await;
                    }
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        tracing::warn!(count, "application operation subscriber lagged");
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
                && (current.catalog != previous.catalog || current.settings != previous.settings),
            protocol::stream::APPLICATIONS,
            current.catalog ^ current.settings,
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

async fn emit_event(
    emitter: &SignalEmitter<'_>,
    stream: &str,
    event: &str,
    subscription_id: &str,
    fields: Value,
) {
    let mut value = json!({ "protocol": api::PROTOCOL, "version": api::VERSION, "stream": stream, "event": event, "subscription_id": subscription_id });
    if let (Some(target), Value::Object(extra)) = (value.as_object_mut(), fields) {
        target.extend(extra);
    }
    if let Err(error) = AppDaemon::event(emitter, stream, &value.to_string()).await {
        tracing::warn!(%stream, %error, "app-api event could not be emitted");
    }
}

pub async fn run() -> Result<()> {
    let applications = ApplicationService::new();
    let shutdown_applications = Arc::clone(&applications);
    let daemon = AppDaemon {
        api: ApiService::new(Arc::clone(&applications)),
        applications,
        subscriptions: Arc::new(OwnedTaskRegistry::default()),
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
    shutdown_applications.save_history_final().await;
    result
}

#[cfg(test)]
mod tests {
    use crate::{protocol, service::StateRevision};

    use super::revision_changes;

    #[test]
    fn filters_shared_state_changes_by_selected_streams() {
        let previous = StateRevision {
            catalog: 1,
            windows: 2,
            settings: 0,
        };
        let current = StateRevision {
            catalog: 3,
            windows: 4,
            settings: 0,
        };
        assert_eq!(
            revision_changes((true, false, false), previous, current).collect::<Vec<_>>(),
            [(protocol::stream::APPLICATIONS, 3)]
        );
        assert_eq!(
            revision_changes((false, true, false), previous, current).collect::<Vec<_>>(),
            [(protocol::stream::WINDOWS, 4)]
        );
    }
}
