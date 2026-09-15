//! Forwarding mechanics; payloads and lag recovery remain daemon policy.
use serde_json::Value;
use shelllist_daemon_core::{ApiIdentity, Correlation};
use std::future::Future;
use tokio::sync::{broadcast, watch};
use zbus::object_server::SignalEmitter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchPhase {
    Initial,
    Changed,
}

/// Marks exactly the cloned value observed and releases the watch lock before I/O.
pub async fn forward_watch<T: Clone, F: Future<Output = ()>>(
    mut receiver: watch::Receiver<T>,
    mut emit: impl FnMut(T, WatchPhase) -> F,
) {
    let initial = receiver.borrow_and_update().clone();
    emit(initial, WatchPhase::Initial).await;
    while receiver.changed().await.is_ok() {
        let value = receiver.borrow_and_update().clone();
        emit(value, WatchPhase::Changed).await;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum BroadcastEvent<T> {
    Item(T),
    Lagged(u64),
}

/// Source closure ends forwarding. Lag is delivered explicitly, never silently skipped.
pub async fn forward_broadcast<T: Clone, F: Future<Output = ()>>(
    mut receiver: broadcast::Receiver<T>,
    mut emit: impl FnMut(BroadcastEvent<T>) -> F,
) {
    loop {
        let event = match receiver.recv().await {
            Ok(item) => BroadcastEvent::Item(item),
            Err(broadcast::error::RecvError::Lagged(count)) => BroadcastEvent::Lagged(count),
            Err(broadcast::error::RecvError::Closed) => return,
        };
        emit(event).await;
    }
}

/// Emits through the provided emitter, preserving its destination. Interface
/// declarations and any decision to broadcast or terminate on failure stay local.
pub async fn emit_json_event(
    emitter: &SignalEmitter<'_>,
    interface: &str,
    api: ApiIdentity,
    stream: &str,
    event: &str,
    correlation: Correlation<'_>,
    fields: Value,
) -> zbus::Result<()> {
    let value = shelllist_daemon_core::event_envelope(api, stream, event, correlation, fields);
    emitter
        .emit(interface, "Event", &(stream, value.to_string()))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn watch_emits_once_releases_borrow_and_stops_when_closed() {
        let (updates, receiver) = watch::channel(0);
        updates.send_replace(1);
        let (events, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(forward_watch(receiver, move |value, phase| {
            events.send((value, phase)).unwrap();
            std::future::ready(())
        }));
        assert_eq!(observed.recv().await, Some((1, WatchPhase::Initial)));
        assert!(observed.try_recv().is_err());
        updates.send_replace(2);
        assert_eq!(observed.recv().await, Some((2, WatchPhase::Changed)));
        assert!(observed.try_recv().is_err());
        drop(updates);
        task.await.unwrap();
        assert_eq!(observed.recv().await, None);
    }
    #[tokio::test]
    async fn lag_is_delivered_before_surviving_items() {
        let (sender, receiver) = broadcast::channel(1);
        sender.send(1).unwrap();
        sender.send(2).unwrap();
        drop(sender);
        let mut events = Vec::new();
        forward_broadcast(receiver, |event| {
            events.push(event);
            std::future::ready(())
        })
        .await;
        assert_eq!(
            events,
            vec![BroadcastEvent::Lagged(1), BroadcastEvent::Item(2)]
        );
    }
}
