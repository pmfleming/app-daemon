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
    api::{self, ApiService},
    protocol,
    service::ApplicationService,
};

pub const BUS_NAME: &str = "org.laufan.AppDaemon";
pub const OBJECT_PATH: &str = "/org/laufan/AppDaemon";
pub const INTERFACE: &str = "org.laufan.AppDaemon1";

pub struct AppDaemon {
    api: Arc<ApiService>,
    applications: Arc<ApplicationService>,
    sequence: AtomicU64,
    subscriptions: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
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
        if let Some(stream) = streams
            .iter()
            .find(|stream| !protocol::STREAMS.contains(&stream.as_str()))
        {
            return api::error(
                "unsupported-stream",
                format!("Unsupported app-api stream: {stream}"),
            )
            .to_string();
        }
        let id = format!(
            "subscription-{}",
            self.sequence.fetch_add(1, Ordering::Relaxed)
        );
        let subscription_id = id.clone();
        let selected = streams;
        let applications = Arc::clone(&self.applications);
        let owned_emitter = emitter.to_owned();
        let task_id = id.clone();
        let task = tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_millis(900));
            let mut previous = applications.revisions().await;
            emit_event(
                &owned_emitter,
                protocol::stream::APPLICATIONS,
                "subscribed",
                &task_id,
                json!({ "catalog_revision": previous.0, "window_revision": previous.1 }),
            )
            .await;
            loop {
                interval.tick().await;
                let current = applications.revisions().await;
                if current.0 != previous.0
                    && selected
                        .iter()
                        .any(|value| value == protocol::stream::APPLICATIONS)
                {
                    emit_event(
                        &owned_emitter,
                        protocol::stream::APPLICATIONS,
                        "changed",
                        &task_id,
                        json!({ "revision": current.0 }),
                    )
                    .await;
                }
                if current.1 != previous.1
                    && selected
                        .iter()
                        .any(|value| value == protocol::stream::WINDOWS)
                {
                    emit_event(
                        &owned_emitter,
                        protocol::stream::WINDOWS,
                        "changed",
                        &task_id,
                        json!({ "revision": current.1 }),
                    )
                    .await;
                }
                previous = current;
            }
        });
        self.subscriptions
            .lock()
            .await
            .insert(subscription_id.clone(), task);
        api::success(json!({ "subscription": { "id": subscription_id } })).to_string()
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

async fn emit_event(
    emitter: &SignalEmitter<'_>,
    stream: &str,
    event: &str,
    subscription_id: &str,
    fields: Value,
) {
    let mut value = json!({ "protocol": api::PROTOCOL, "version": api::VERSION, "stream": stream, "event": event, "subscription_id": subscription_id });
    if let (Some(target), Some(extra)) = (value.as_object_mut(), fields.as_object()) {
        target.extend(extra.clone());
    }
    if let Err(error) = AppDaemon::event(emitter, stream, &value.to_string()).await {
        tracing::warn!(%stream, %error, "app-api event could not be emitted");
    }
}

pub async fn run() -> Result<()> {
    let applications = ApplicationService::new();
    let daemon = AppDaemon {
        api: Arc::new(ApiService::new(Arc::clone(&applications))),
        applications,
        sequence: AtomicU64::new(1),
        subscriptions: Arc::new(Mutex::new(HashMap::new())),
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
    tokio::select! { result = ctrl_c() => result.context("wait for Ctrl-C"), _ = terminate.recv() => Ok(()) }
}
