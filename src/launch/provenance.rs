//! Short-lived launch ownership, independent of an application's current cgroup.
use std::{collections::HashMap, fs};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Process {
    parent: u32,
    start: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Provenance {
    identities: HashMap<u32, u64>,
}

impl Provenance {
    /// Established application processes can service a singleton launch without
    /// entering the new launch unit. Capture identities BEFORE the handoff.
    pub(crate) fn for_application(
        catalog: &crate::catalog::Catalog,
        target: &str,
        window_pids: impl IntoIterator<Item = u32>,
    ) -> Self {
        let processes = processes();
        let mut roots = window_pids.into_iter().collect::<Vec<_>>();
        roots.extend(processes.keys().copied().filter(|pid| {
            crate::resources::process_cgroup(*pid)
                .and_then(|path| catalog.target_for_cgroup(&path).map(str::to_owned))
                .is_some_and(|id| id == target)
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
    }

    pub(super) fn remember(&mut self, pid: u32) {
        if let Some(process) = read_process(pid) {
            self.identities.insert(pid, process.start);
        }
    }

    pub(super) fn owns(&self, pid: u32) -> bool {
        read_process(pid).is_some_and(|process| self.identities.get(&pid) == Some(&process.start))
    }

    pub(super) fn observe(&mut self, unit: Option<&str>) {
        let processes = processes();
        let roots = processes
            .keys()
            .copied()
            .filter(|pid| {
                unit.is_some_and(|unit| {
                    crate::resources::process_cgroup(*pid)
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
        for pid in roots {
            if let Some(process) = processes.get(pid) {
                self.identities.insert(*pid, process.start);
            }
        }
        loop {
            let before = self.identities.len();
            for (&pid, process) in processes {
                if let Some(parent_start) = self.identities.get(&process.parent)
                    && process.start >= *parent_start
                {
                    self.identities.insert(pid, process.start);
                }
            }
            if self.identities.len() == before {
                break;
            }
        }
    }
}

fn read_process(pid: u32) -> Option<Process> {
    parse_process(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

fn parse_process(stat: &str) -> Option<Process> {
    // comm can contain spaces and parentheses. Fields after its final ')' start
    // with state (field 3); starttime is field 22 and ppid is field 4.
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    if matches!(*fields.first()?, "Z" | "X") {
        return None;
    }
    Some(Process {
        parent: fields.get(1)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
    })
}

fn processes() -> HashMap<u32, Process> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return HashMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            Some((pid, read_process(pid)?))
        })
        .collect()
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
    use super::*;

    #[test]
    fn retains_migrated_processes_and_observed_descendants_but_rejects_pid_reuse() {
        let mut owner = Provenance::default();
        let mut processes = HashMap::from([
            (
                10,
                Process {
                    parent: 1,
                    start: 100,
                },
            ),
            (
                11,
                Process {
                    parent: 10,
                    start: 101,
                },
            ),
            (
                12,
                Process {
                    parent: 11,
                    start: 102,
                },
            ),
            (
                20,
                Process {
                    parent: 1,
                    start: 99,
                },
            ),
        ]);
        owner.observe_processes(&processes, &[10]);
        assert_eq!(owner.identities.len(), 3);
        // No launch-cgroup members remain. An observed child is reparented.
        processes.remove(&10);
        processes.get_mut(&11).unwrap().parent = 1;
        owner.observe_processes(&processes, &[]);
        assert_eq!(owner.identities.len(), 2);
        assert!(!owner.identities.contains_key(&20));
        // The same numeric PIDs are not the same processes.
        processes.get_mut(&11).unwrap().start = 200;
        processes.get_mut(&12).unwrap().start = 201;
        owner.observe_processes(&processes, &[]);
        assert!(owner.identities.is_empty());
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
                start: 1234
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
