use anyhow::{Context, Result};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::watch;
use zbus::{message::Header, names::UniqueName, object_server::SignalEmitter};

use shelllist_daemon_core::DaemonEndpoint;

use crate::output_actor::{OutputCommand, OutputHandle};

async fn dbus_proxy(connection: &zbus::Connection) -> Result<zbus::Proxy<'_>> {
    zbus::Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .context("create D-Bus owner proxy")
}

#[derive(Clone)]
pub struct JsonDbusClient {
    connection: zbus::Connection,
    endpoint: DaemonEndpoint,
}

impl JsonDbusClient {
    #[must_use]
    pub const fn new(connection: zbus::Connection, endpoint: DaemonEndpoint) -> Self {
        Self {
            connection,
            endpoint,
        }
    }

    pub async fn session(endpoint: DaemonEndpoint) -> Result<Self> {
        let connection = zbus::Connection::session()
            .await
            .context("connect to session D-Bus")?;
        Ok(Self::new(connection, endpoint))
    }

    pub fn connection(&self) -> &zbus::Connection {
        &self.connection
    }

    async fn proxy(&self) -> Result<zbus::Proxy<'_>> {
        zbus::Proxy::new(
            &self.connection,
            self.endpoint.bus_name,
            self.endpoint.object_path,
            self.endpoint.interface,
        )
        .await
        .with_context(|| format!("create {} proxy", self.endpoint.executable))
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let proxy = self.proxy().await?;
        let params_json = serde_json::to_string(&params).context("encode call parameters")?;
        let response: String = proxy
            .call("Call", &(method, params_json.as_str()))
            .await
            .with_context(|| format!("call {}", self.endpoint.executable))?;
        serde_json::from_str(&response).context("decode daemon call response")
    }

    pub async fn subscribe(&self, streams: Vec<String>) -> Result<Value> {
        let proxy = self.proxy().await?;
        let response: String = proxy
            .call("Subscribe", &(streams,))
            .await
            .with_context(|| format!("subscribe to {}", self.endpoint.executable))?;
        serde_json::from_str(&response).context("decode daemon subscription response")
    }

    pub async fn cancel_json(&self, request_id: &str) -> Result<Value> {
        let proxy = self.proxy().await?;
        let response: String = proxy
            .call("Cancel", &(request_id,))
            .await
            .with_context(|| format!("cancel {} request", self.endpoint.executable))?;
        serde_json::from_str(&response).context("decode daemon cancellation response")
    }

    pub async fn cancel_unit(&self, request_id: &str) -> Result<Value> {
        let proxy = self.proxy().await?;
        proxy
            .call::<_, _, ()>("Cancel", &(request_id,))
            .await
            .with_context(|| format!("cancel {} request", self.endpoint.executable))?;
        Ok(json!({ "cancelled": request_id }))
    }

    pub(crate) async fn forward_events(
        &self,
        output: &OutputHandle,
        generation_ready: &watch::Sender<bool>,
    ) -> Result<()> {
        let proxy = self.proxy().await?;
        let mut events = proxy
            .receive_signal("Event")
            .await
            .context("receive daemon events")?;
        // Retain readiness even when no subscription is currently waiting.
        generation_ready.send_replace(true);
        while let Some(message) = events.next().await {
            let (stream, event_json): (String, String) = message
                .body()
                .deserialize()
                .context("decode daemon event signal")?;
            let event = serde_json::from_str::<Value>(&event_json)
                .unwrap_or_else(|_| json!({ "raw": event_json }));
            output.send(OutputCommand::Event { stream, event }).await?;
        }
        anyhow::bail!("daemon event stream ended")
    }

    pub(crate) async fn watch_replacement(&self) -> Result<()> {
        wait_for_name_replacement(&self.connection, self.endpoint.bus_name).await
    }
}

#[must_use]
pub fn directed_emitter(
    emitter: &SignalEmitter<'_>,
    header: &Header<'_>,
) -> SignalEmitter<'static> {
    match header.sender() {
        Some(sender) => emitter.to_owned().set_destination(sender.to_owned().into()),
        None => emitter.to_owned(),
    }
}

pub async fn wait_for_owner_loss(
    connection: &zbus::Connection,
    owner: UniqueName<'static>,
) -> Result<()> {
    wait_for_owner_name_loss(connection, owner.as_str()).await
}

pub async fn wait_for_owner_name_loss(connection: &zbus::Connection, owner: &str) -> Result<()> {
    crate::OwnerLossMonitor::new(connection.clone())
        .wait(owner)
        .await
}

async fn wait_for_name_replacement(connection: &zbus::Connection, bus_name: &str) -> Result<()> {
    wait_for_name_change(connection, bus_name, name_replaced).await
}

fn name_replaced(old_owner: &str, new_owner: &str) -> bool {
    !old_owner.is_empty() && old_owner != new_owner
}

async fn wait_for_name_change(
    connection: &zbus::Connection,
    watched_name: &str,
    matches: fn(&str, &str) -> bool,
) -> Result<()> {
    let proxy = dbus_proxy(connection).await?;
    let mut changes = proxy
        .receive_signal("NameOwnerChanged")
        .await
        .context("receive D-Bus owner changes")?;
    while let Some(message) = changes.next().await {
        let (name, old_owner, new_owner): (String, String, String) =
            message
                .body()
                .deserialize()
                .context("decode owner change")?;
        if name == watched_name && matches(&old_owner, &new_owner) {
            return Ok(());
        }
    }
    anyhow::bail!("D-Bus owner-change stream ended")
}
