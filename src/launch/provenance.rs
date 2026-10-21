//! Short-lived launch ownership, independent of an application's current cgroup.
use crate::process::{
    Identity, Process, descendants_where, live_process_children, parse_process, process_cgroup,
    read_process, read_processes,
};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Provenance {
    identities: HashMap<u32, u64>,
    // Keep original anchors separate from observed descendants. A reparented
    // child must never become a new authoritative launch root merely by outliving
    // its parent (it may have entered another application's unit).
    anchors: HashMap<u32, u64>,
}

impl Provenance {
    /// Established application processes can service a singleton launch without
    /// entering the new launch unit. Capture identities BEFORE the handoff.
    pub(crate) fn for_application(
        catalog: &crate::catalog::Catalog,
        target: &str,
        window_pids: impl IntoIterator<Item = u32>,
    ) -> Self {
        let processes = read_processes(parse_process);
        let mut roots = window_pids.into_iter().collect::<Vec<_>>();
        roots.extend(processes.keys().copied().filter(|pid| {
            process_cgroup(*pid)
                .is_some_and(|path| catalog.target_for_cgroup(&path) == Some(target))
        }));
        let mut owner = Self::default();
        owner.observe_processes(&processes, &roots);
        owner
    }

    pub(super) fn merge(&mut self, other: Self) {
        self.identities
            .extend(other.identities.into_iter().filter(|&(pid, start)| {
                read_process(pid).is_some_and(|process| process.start == start)
            }));
        self.anchors.extend(
            other
                .anchors
                .into_iter()
                .filter(|(pid, start)| self.identities.get(pid) == Some(start)),
        );
    }

    pub(super) fn remember(&mut self, pid: u32) {
        if let Some(process) = read_process(pid) {
            self.identities.insert(pid, process.start);
            self.anchors.insert(pid, process.start);
        }
    }

    pub(super) fn roots(&self) -> impl Iterator<Item = Identity> + '_ {
        self.anchors.iter().filter_map(|(&pid, &start)| {
            (read_process(pid)?.start == start).then_some((pid, start))
        })
    }

    pub(super) fn owns(&self, pid: u32) -> bool {
        read_process(pid).is_some_and(|process| self.identities.get(&pid) == Some(&process.start))
    }

    pub(super) fn observe(&mut self, unit: Option<&str>) {
        let processes = read_processes(parse_process);
        let roots = processes
            .keys()
            .copied()
            .filter(|pid| {
                unit.is_some_and(|unit| {
                    process_cgroup(*pid)
                        .is_some_and(|path| path.split('/').any(|part| part == unit))
                })
            })
            .collect::<Vec<_>>();
        self.observe_processes(&processes, &roots);
    }

    fn observe_processes(&mut self, processes: &HashMap<u32, Process>, roots: &[u32]) {
        self.identities.retain(|pid, start| {
            processes
                .get(pid)
                .is_some_and(|process| process.start == *start)
        });
        self.anchors
            .retain(|pid, start| self.identities.get(pid) == Some(start));
        let root_set = roots.iter().copied().collect::<HashSet<_>>();
        for &pid in roots {
            if let Some(process) = processes.get(&pid)
                && !self.identities.contains_key(&pid)
                && !self.identities.contains_key(&process.parent)
                && !root_set.contains(&process.parent)
            {
                self.anchors.insert(pid, process.start);
            }
        }
        // Build only valid ancestry edges, then visit each descendant once.
        let children = live_process_children(processes);
        let owned = descendants_where(
            roots.iter().chain(self.identities.keys()).copied(),
            &children,
            |pid| processes.contains_key(&pid),
        );
        self.identities = owned
            .into_iter()
            .filter_map(|pid| Some((pid, processes.get(&pid)?.start)))
            .collect();
    }
}

/// A unique launch unit can retain MainPID even after Chromium migrates that
/// process to its own scope. Anchor the PID/start-time pair immediately after
/// handoff; later ownership checks never trust a bare retained PID.
pub(super) async fn unit_main_pid(unit: &str) -> Option<u32> {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        let connection = zbus::Connection::session().await.ok()?;
        let manager = zbus::Proxy::new(
            &connection,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .await
        .ok()?;
        let path: zbus::zvariant::OwnedObjectPath = manager.call("GetUnit", &(unit,)).await.ok()?;
        let service = zbus::Proxy::new(
            &connection,
            "org.freedesktop.systemd1",
            path,
            "org.freedesktop.systemd1.Service",
        )
        .await
        .ok()?;
        service
            .get_property::<u32>("MainPID")
            .await
            .ok()
            .filter(|pid| *pid != 0)
    })
    .await
    .ok()
    .flatten()
}

pub(super) async fn bus_owner_pid(connection: &zbus::Connection, name: &str) -> Option<u32> {
    let bus = zbus::fdo::DBusProxy::new(connection).await.ok()?;
    let name = zbus::names::BusName::try_from(name).ok()?;
    let owner = bus.get_name_owner(name.clone()).await.ok()?;
    let pid = bus
        .get_connection_unix_process_id(owner.clone().into())
        .await
        .ok()?;
    (bus.get_name_owner(name).await.ok()? == owner).then_some(pid)
}
