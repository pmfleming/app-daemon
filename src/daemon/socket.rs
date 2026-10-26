//! In-process ingress through the same admitted service methods as D-Bus.
use super::{AppDaemon, OBJECT_PATH};
use anyhow::Result;
use serde_json::Value;
use shelllist_daemon_tokio::{JsonlListener, JsonlSocketConfig, LocalJsonlService, SocketSession};
use std::sync::Arc;

struct Service(zbus::Connection);
impl LocalJsonlService for Service {
    async fn call(&self, session: &SocketSession, method: &str, params: Value) -> Result<Value> {
        let interface = self
            .0
            .object_server()
            .interface::<_, AppDaemon>(OBJECT_PATH)
            .await?;
        let daemon = interface.get().await;
        decode(
            daemon
                .call_owned(method, &params.to_string(), Some(session.owner().into()))
                .await,
        )
    }
    async fn subscribe(&self, session: &SocketSession, streams: Vec<String>) -> Result<Value> {
        let interface = self
            .0
            .object_server()
            .interface::<_, AppDaemon>(OBJECT_PATH)
            .await?;
        let daemon = interface.get().await;
        let lifetime = session.clone();
        decode(
            daemon
                .subscribe_owned(
                    streams,
                    Some(session.owner().into()),
                    session.clone().into(),
                    async move { lifetime.closed().await },
                )
                .await,
        )
    }
    async fn cancel(&self, session: &SocketSession, id: &str) -> Result<Value> {
        let interface = self
            .0
            .object_server()
            .interface::<_, AppDaemon>(OBJECT_PATH)
            .await?;
        let daemon = interface.get().await;
        decode(daemon.cancel_owned(id, Some(session.owner())).await)
    }
    async fn disconnected(&self, session: &SocketSession) {
        if let Ok(interface) = self
            .0
            .object_server()
            .interface::<_, AppDaemon>(OBJECT_PATH)
            .await
        {
            let daemon = interface.get().await;
            daemon
                .applications
                .cancel_owner_operations(session.owner())
                .await;
        }
    }
}
#[cfg(test)]
#[path = "socket/qualification.rs"]
mod qualification;

fn decode(response: String) -> Result<Value> {
    Ok(serde_json::from_str(&response)?)
}

pub(super) async fn listen(
    connection: zbus::Connection,
    stop: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| anyhow::anyhow!("XDG_RUNTIME_DIR is required for the app socket"))?;
    JsonlListener::bind(std::path::Path::new(&runtime), "app-daemon")
        .await?
        .serve(
            Arc::new(Service(connection)),
            crate::client::AppCorrelation,
            JsonlSocketConfig::default(),
            stop,
        )
        .await
}
