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
        for (pid, start) in other.identities {
            if read_process(pid).is_some_and(|process| process.start == start) {
                self.identities.insert(pid, start);
            }
        }
        for (pid, start) in other.anchors {
            if self.identities.get(&pid) == Some(&start) {
                self.anchors.insert(pid, start);
            }
        }
    }

    pub(super) fn remember(&mut self, pid: u32) {
        if let Some(process) = read_process(pid) {
            self.identities.insert(pid, process.start);
            self.anchors.insert(pid, process.start);
        }
    }

    pub(super) fn roots(&self) -> Vec<Identity> {
        self.anchors
            .iter()
            .filter_map(|(&pid, &start)| {
                (read_process(pid)?.start == start).then_some((pid, start))
            })
            .collect()
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

#[cfg(test)]
mod tests {
    use super::{Process, Provenance, parse_process};
    use std::collections::HashMap;

    #[test]
    fn retains_migrated_processes_and_observed_descendants_but_rejects_pid_reuse() {
        let mut owner = Provenance::default();
        let mut processes = [
            (10, 1, 100),
            (11, 10, 101),
            (12, 11, 102),
            (20, 1, 99),
            (21, 10, 99),
            (30, 31, 300),
            (31, 30, 300),
        ]
        .into_iter()
        .map(|(pid, parent, start)| {
            (
                pid,
                Process {
                    parent,
                    start,
                    cgroup: None,
                },
            )
        })
        .collect::<HashMap<_, _>>();
        owner.observe_processes(&processes, &[10]);
        assert_eq!(owner.identities.len(), 3);
        assert_eq!(owner.anchors, HashMap::from([(10, 100)]));
        assert!(
            !owner.identities.contains_key(&21),
            "a child cannot predate its parent"
        );
        // No launch-cgroup members remain. An observed child is reparented.
        processes.remove(&10);
        processes.get_mut(&11).unwrap().parent = 1;
        owner.observe_processes(&processes, &[]);
        assert_eq!(owner.identities.len(), 2);
        assert!(
            owner.anchors.is_empty(),
            "reparented descendants are not new launch roots"
        );
        assert!(!owner.identities.contains_key(&20));
        // The same numeric PIDs are not the same processes.
        processes.get_mut(&11).unwrap().start = 200;
        processes.get_mut(&12).unwrap().start = 201;
        owner.observe_processes(&processes, &[]);
        assert!(owner.identities.is_empty());
        // Broken/cyclic ancestry and duplicate or missing roots still terminate.
        owner.observe_processes(&processes, &[30, 30, 999]);
        assert_eq!(owner.identities, HashMap::from([(30, 300), (31, 300)]));
    }

    #[test]
    fn parses_process_identity_without_trusting_comm_or_zombies() {
        let stat = format!(
            "42 (name with ) spaces) S 1 {} 1234",
            vec!["0"; 17].join(" ")
        );
        assert_eq!(
            parse_process(&stat),
            Some(Process {
                parent: 1,
                start: 1234,
                cgroup: None,
            })
        );
        assert!(parse_process(&stat.replace(") S", ") Z")).is_none());
        assert!(parse_process("malformed").is_none());
        let mut owner = Provenance::default();
        owner.remember(std::process::id());
        assert!(owner.owns(std::process::id()));
        assert!(!owner.owns(0));
    }
}
