//! Isolated real-bus tests; never change the process session-bus environment.
use futures::StreamExt;
use serde_json::json;
use shelllist_daemon_core::{ApiIdentity, Correlation};
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Bus {
    child: Child,
    address: String,
}
impl Bus {
    fn start() -> Self {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("dbus-daemon is required for transport tests");
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Self {
            child,
            address: address.trim().into(),
        }
    }
    async fn connection(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    }
}
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn monitors_with_identical_unique_names_on_different_buses_are_independent() {
    let first = Bus::start();
    let second = Bus::start();
    let server_a = first.connection().await;
    let server_b = second.connection().await;
    let caller_a = first.connection().await;
    let caller_b = second.connection().await;
    let owner = caller_a.unique_name().unwrap().to_string();
    assert_eq!(owner, caller_b.unique_name().unwrap().as_str());
    let monitor_a = crate::OwnerLossMonitor::new(server_a);
    let monitor_b = crate::OwnerLossMonitor::new(server_b);
    // Establish both match rules before disconnecting either caller.
    let _a = monitor_a.subscribe().await.unwrap();
    let _b = monitor_b.subscribe().await.unwrap();
    caller_a.close().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), monitor_a.wait(&owner))
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), monitor_b.wait(&owner))
            .await
            .is_err()
    );
    caller_b.close().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), monitor_b.wait(&owner))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn owner_loss_ends_and_unregisters_managed_work() {
    let bus = Bus::start();
    let server = bus.connection().await;
    let caller = bus.connection().await;
    let owner = caller.unique_name().unwrap().to_string();
    let registry = crate::OwnedTaskRegistry::default();
    let marker = std::sync::Arc::new(());
    let held = marker.clone();
    registry
        .spawn_for_owner("sub".into(), Some(owner.clone()), &server, async move {
            let _held = held;
            std::future::pending::<()>().await;
        })
        .unwrap();
    assert!(!registry.cancel_owned("sub", Some(":unrelated")).await);
    caller.close().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while std::sync::Arc::strong_count(&marker) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!registry.cancel_owned("sub", Some(&owner)).await);
}

#[tokio::test]
async fn losing_the_bus_wakes_monitor_receivers_with_failure() {
    let bus = Bus::start();
    let monitor = crate::OwnerLossMonitor::new(bus.connection().await);
    let mut losses = monitor.subscribe().await.unwrap();
    drop(bus);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), losses.recv())
            .await
            .unwrap(),
        Err(crate::OwnerLossError::Stopped)
    );
}

#[tokio::test]
async fn shared_emission_preserves_wire_shape_and_directed_delivery() {
    let bus = Bus::start();
    let server = bus.connection().await;
    let caller = bus.connection().await;
    let other = bus.connection().await;
    let destination = server.unique_name().unwrap().as_str();
    let caller_proxy = zbus::Proxy::new(&caller, destination, "/test", "org.shelllist.Test")
        .await
        .unwrap();
    let other_proxy = zbus::Proxy::new(&other, destination, "/test", "org.shelllist.Test")
        .await
        .unwrap();
    let mut received = caller_proxy.receive_signal("Event").await.unwrap();
    let mut unrelated = other_proxy.receive_signal("Event").await.unwrap();
    let emitter = zbus::object_server::SignalEmitter::new(&server, "/test")
        .unwrap()
        .set_destination(caller.unique_name().unwrap().clone().into());
    crate::emit_json_event(
        &emitter,
        "org.shelllist.Test",
        ApiIdentity::new("test-api", 1),
        "test.changed",
        "changed",
        Correlation::Subscription("sub-1"),
        json!({ "data": { "revision": 7 } }),
    )
    .await
    .unwrap();
    let message = tokio::time::timeout(Duration::from_secs(2), received.next())
        .await
        .unwrap()
        .unwrap();
    let (stream, event): (String, String) = message.body().deserialize().unwrap();
    assert_eq!(stream, "test.changed");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&event).unwrap(),
        json!({
            "protocol": "test-api", "version": 1, "stream": "test.changed", "event": "changed", "subscription_id": "sub-1", "data": { "revision": 7 }
        })
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), unrelated.next())
            .await
            .is_err()
    );
}
