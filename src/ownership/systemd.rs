//! Bounded recovery from live user-manager services, never ExecMainPID history.
use crate::{
    catalog::Catalog,
    process::{Identity, process_identity},
};
use futures::{StreamExt, stream};
use std::time::Duration;
use zbus::zvariant::OwnedObjectPath;

type Unit = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);
const BUS: &str = "org.freedesktop.systemd1";

pub(crate) async fn recover(catalog: &Catalog) -> Vec<(String, Identity)> {
    // A missing/unresponsive manager must not block sampling or startup. Claims
    // already observed remain valid for their process lifetime during an outage.
    let mut recovered = Vec::new();
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        recover_inner(catalog, &mut recovered),
    )
    .await;
    recovered
}

async fn recover_inner(catalog: &Catalog, recovered: &mut Vec<(String, Identity)>) -> Option<()> {
    let connection = zbus::Connection::session().await.ok()?;
    let manager = zbus::Proxy::new(
        &connection,
        BUS,
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .await
    .ok()?;
    let units: Vec<Unit> = manager.call("ListUnits", &()).await.ok()?;
    let candidates = units.into_iter().filter_map(|unit| {
        if unit.3 != "active" || !unit.0.ends_with(".service") {
            return None;
        }
        Some((catalog.target_for_cgroup(&unit.0)?.to_owned(), unit.6))
    });
    let mut claims = stream::iter(candidates)
        .map(|(target, path)| {
            let connection = &connection;
            async move {
                let identity = tokio::time::timeout(
                    Duration::from_millis(250),
                    live_main_process(connection, path),
                )
                .await
                .ok()
                .flatten()?;
                Some((target, identity))
            }
        })
        .buffer_unordered(8);
    while let Some(claim) = claims.next().await {
        if let Some(claim) = claim {
            recovered.push(claim);
        }
    }
    Some(())
}

async fn live_main_process(
    connection: &zbus::Connection,
    path: OwnedObjectPath,
) -> Option<Identity> {
    let unit = uncached_proxy(connection, &path, "org.freedesktop.systemd1.Unit").await?;
    let service = uncached_proxy(connection, &path, "org.freedesktop.systemd1.Service").await?;
    let invocation: Vec<u8> = unit.get_property("InvocationID").await.ok()?;
    if invocation.len() != 16 || invocation.iter().all(|b| *b == 0) {
        return None;
    }
    let pid = service.get_property::<u32>("MainPID").await.ok()?;
    if pid == 0 {
        return None;
    }
    let identity = process_identity(pid)?;
    // These proxies never cache: a restart between reads must invalidate the claim.
    let current_pid: u32 = service.get_property("MainPID").await.ok()?;
    let current_invocation: Vec<u8> = unit.get_property("InvocationID").await.ok()?;
    let active: String = unit.get_property("ActiveState").await.ok()?;
    (active == "active"
        && current_pid == pid
        && invocation == current_invocation
        && process_identity(pid) == Some(identity))
    .then_some(identity)
}

async fn uncached_proxy<'a>(
    connection: &zbus::Connection,
    path: &'a OwnedObjectPath,
    interface: &'static str,
) -> Option<zbus::Proxy<'a>> {
    zbus::proxy::Builder::new(connection)
        .destination(BUS)
        .ok()?
        .path(path.as_str())
        .ok()?
        .interface(interface)
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()
}
