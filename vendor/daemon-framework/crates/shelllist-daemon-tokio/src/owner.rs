//! Connection-scoped unique-name monitoring. No process-global bus state.
use anyhow::{Context, Result};
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::{OnceCell, broadcast, watch};

#[derive(Clone)]
pub struct OwnerLossMonitor(Arc<Monitor>);

struct Monitor {
    connection: zbus::Connection,
    started: OnceCell<Running>,
}

struct Running {
    losses: broadcast::Sender<String>,
    stopped: watch::Receiver<bool>,
    _task: crate::AbortOnDrop,
}

impl OwnerLossMonitor {
    #[must_use]
    pub fn new(connection: zbus::Connection) -> Self {
        Self(Arc::new(Monitor {
            connection,
            started: OnceCell::new(),
        }))
    }

    async fn running(&self) -> Result<&Running> {
        self.0
            .started
            .get_or_try_init(|| async {
                let proxy = proxy(&self.0.connection).await?;
                let mut changes = proxy
                    .receive_signal("NameOwnerChanged")
                    .await
                    .context("receive D-Bus owner changes")?;
                let (losses, _) = broadcast::channel(64);
                let publisher = losses.clone();
                let (stopped, state) = watch::channel(false);
                let task = crate::spawn_named("D-Bus-owner-monitor", async move {
                    while let Some(message) = changes.next().await {
                        let Ok((name, old, new)) =
                            message.body().deserialize::<(String, String, String)>()
                        else {
                            continue;
                        };
                        if name.starts_with(':') && !old.is_empty() && new.is_empty() {
                            let _ = publisher.send(name);
                        }
                    }
                    stopped.send_replace(true);
                    tracing::warn!("D-Bus owner monitor stopped");
                });
                Ok(Running {
                    losses,
                    stopped: state,
                    _task: crate::AbortOnDrop(task.abort_handle()),
                })
            })
            .await
    }

    /// Subscribe before checking ownership to close the check/wait race.
    /// Monitor termination is an error, never a permanently pending wait.
    pub async fn wait(&self, owner: &str) -> Result<()> {
        let mut losses = self.subscribe().await?;
        if !self.has_owner(owner).await? {
            return Ok(());
        }
        loop {
            match losses.recv().await {
                Ok(lost) if lost == owner => return Ok(()),
                Ok(_) => {}
                Err(OwnerLossError::Lagged(_)) => {
                    if !self.has_owner(owner).await? {
                        return Ok(());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub async fn has_owner(&self, owner: &str) -> Result<bool> {
        proxy(&self.0.connection)
            .await?
            .call("NameHasOwner", &(owner,))
            .await
            .context("check D-Bus owner")
    }

    /// Actor users must reconcile their owner set after Lagged and stop on Stopped.
    pub async fn subscribe(&self) -> Result<OwnerLossReceiver> {
        let running = self.running().await?;
        Ok(OwnerLossReceiver {
            losses: running.losses.subscribe(),
            stopped: running.stopped.clone(),
        })
    }
}

pub struct OwnerLossReceiver {
    losses: broadcast::Receiver<String>,
    stopped: watch::Receiver<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerLossError {
    Lagged(u64),
    Stopped,
}

impl std::fmt::Display for OwnerLossError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Lagged(count) => write!(f, "missed {count} D-Bus owner losses"),
            Self::Stopped => f.write_str("D-Bus owner monitor stopped"),
        }
    }
}
impl std::error::Error for OwnerLossError {}

impl OwnerLossReceiver {
    pub async fn recv(&mut self) -> Result<String, OwnerLossError> {
        if *self.stopped.borrow() {
            return Err(OwnerLossError::Stopped);
        }
        tokio::select! {
            biased;
            _ = self.stopped.changed() => Err(OwnerLossError::Stopped),
            loss = self.losses.recv() => loss.map_err(|error| match error {
                broadcast::error::RecvError::Lagged(count) => OwnerLossError::Lagged(count),
                broadcast::error::RecvError::Closed => OwnerLossError::Stopped,
            }),
        }
    }
}

async fn proxy(connection: &zbus::Connection) -> Result<zbus::Proxy<'_>> {
    zbus::Proxy::new(
        connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .context("create D-Bus owner proxy")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn stopped_monitor_wakes_existing_and_late_receivers() {
        let (losses, receiver) = broadcast::channel(1);
        let (stopped, state) = watch::channel(false);
        let mut receiver = OwnerLossReceiver {
            losses: receiver,
            stopped: state.clone(),
        };
        stopped.send_replace(true);
        assert_eq!(receiver.recv().await, Err(OwnerLossError::Stopped));
        let mut late = OwnerLossReceiver {
            losses: losses.subscribe(),
            stopped: state,
        };
        assert_eq!(late.recv().await, Err(OwnerLossError::Stopped));
    }
    #[tokio::test]
    async fn lag_is_explicit_and_dropped_monitor_is_terminal() {
        let (losses, receiver) = broadcast::channel(1);
        let (stopped, state) = watch::channel(false);
        let mut receiver = OwnerLossReceiver {
            losses: receiver,
            stopped: state,
        };
        losses.send(":1.1".into()).unwrap();
        losses.send(":1.2".into()).unwrap();
        assert_eq!(receiver.recv().await, Err(OwnerLossError::Lagged(1)));
        assert_eq!(receiver.recv().await.unwrap(), ":1.2");
        drop(stopped);
        assert_eq!(receiver.recv().await, Err(OwnerLossError::Stopped));
    }
}
