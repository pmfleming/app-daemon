use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::{
    signal::{
        ctrl_c,
        unix::{SignalKind, signal},
    },
    sync::Mutex,
    task::JoinHandle,
    time,
};
use zbus::{connection, object_server::SignalEmitter};

use crate::{
    api::{self, ApiService, BUS_NAME, OBJECT_PATH},
    protocol,
    service::ApplicationService,
};

pub struct AppDaemon {
    api: ApiService,
    applications: Arc<ApplicationService>,
    sequence: AtomicU64,
    subscriptions: Mutex<HashMap<String, JoinHandle<()>>>,
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
        let id = format!(
            "subscription-{}",
            self.sequence.fetch_add(1, Ordering::Relaxed)
        );
        let task = tokio::spawn(poll_revisions(
            Arc::clone(&self.applications),
            emitter.to_owned(),
            id.clone(),
            selected,
        ));
        self.subscriptions.lock().await.insert(id.clone(), task);
        api::success(json!({ "subscription": { "id": id } })).to_string()
    }

    async fn cancel(&self, request_id: &str) -> String {
        if let Some(task) = self.subscriptions.lock().await.remove(request_id) {
            task.abort();
            return api::success(json!({ "cancelled": request_id, "kind": "subscription" }))
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

fn selected_streams(streams: &[String]) -> Result<(bool, bool), &str> {
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
    ))
}

async fn poll_revisions(
    applications: Arc<ApplicationService>,
    emitter: SignalEmitter<'static>,
    subscription_id: String,
    selected: (bool, bool),
) {
    let mut interval = time::interval(Duration::from_millis(900));
    let mut previous = applications.revisions().await;
    emit_event(
        &emitter,
        protocol::stream::APPLICATIONS,
        "subscribed",
        &subscription_id,
        json!({ "catalog_revision": previous.0, "window_revision": previous.1 }),
    )
    .await;
    loop {
        interval.tick().await;
        let current = applications.revisions().await;
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
}

fn revision_changes(
    selected: (bool, bool),
    previous: (u64, u64),
    current: (u64, u64),
) -> impl Iterator<Item = (&'static str, u64)> {
    [
        (
            selected.0 && current.0 != previous.0,
            protocol::stream::APPLICATIONS,
            current.0,
        ),
        (
            selected.1 && current.1 != previous.1,
            protocol::stream::WINDOWS,
            current.1,
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
        sequence: AtomicU64::new(1),
        subscriptions: Mutex::new(HashMap::new()),
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
    let mut terminate = signal(SignalKind::terminate()).context("listen for SIGTERM")?;
    let result = tokio::select! {
        result = ctrl_c() => result.context("wait for Ctrl-C"),
        _ = terminate.recv() => Ok(()),
    };
    shutdown_applications.save_history_final().await;
    result
}
