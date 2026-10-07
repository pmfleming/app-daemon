//! Verified launch ownership, independent of a process's current cgroup.
//! No process control, browser heuristics, or persisted PID-only identities.
use std::collections::{HashMap, HashSet, VecDeque};

use crate::{
    catalog::Catalog,
    process::{process_cgroup, process_children, process_stat_fields, read_processes},
};

mod systemd;
#[cfg(test)]
mod tests;
pub(crate) use systemd::recover;

pub(crate) type Identity = (u32, u64);

#[derive(Clone, Debug)]
pub(crate) struct Process {
    pub(crate) parent: u32,
    pub(crate) start: u64,
    pub(crate) cgroup: Option<String>,
}

pub(crate) fn process_identity(pid: u32) -> Option<Identity> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    Some((pid, parse_process(&stat)?.start))
}

fn parse_process(stat: &str) -> Option<Process> {
    let fields = process_stat_fields(stat)?;
    if matches!(*fields.first()?, "Z" | "X") {
        return None;
    }
    Some(Process {
        parent: fields.get(1)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
        cgroup: None,
    })
}

pub(crate) fn processes() -> HashMap<u32, Process> {
    use std::os::unix::fs::MetadataExt;
    let Ok(own) = std::fs::metadata("/proc/self") else {
        return HashMap::new();
    };
    let mut processes = read_processes(parse_process);
    processes.retain(|pid, process| {
        if !std::fs::metadata(format!("/proc/{pid}")).is_ok_and(|m| m.uid() == own.uid()) {
            return false;
        }
        process.cgroup = process_cgroup(*pid);
        process_identity(*pid) == Some((*pid, process.start))
    });
    processes
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Owner {
    // None is conflicting verified evidence: never fall back to a guessed owner.
    target: Option<String>,
    // Only descendants of the verified root can inherit its migrated scope.
    // Sharing that scope alone is not evidence of ownership.
    scope: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    owners: HashMap<Identity, Owner>,
}

impl Snapshot {
    pub(crate) fn target(&self, identity: Identity) -> Option<Option<&str>> {
        self.owners
            .get(&identity)
            .map(|owner| owner.target.as_deref())
    }

    pub(crate) fn live_target(&self, pid: u32) -> Option<Option<&str>> {
        self.target(process_identity(pid)?)
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (Identity, Option<&str>)> {
        self.owners
            .iter()
            .map(|(&identity, owner)| (identity, owner.target.as_deref()))
    }

    pub(crate) fn revision(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut entries = self.entries().collect::<Vec<_>>();
        entries.sort_unstable();
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        entries.hash(&mut hash);
        hash.finish()
    }
}

#[derive(Default)]
pub(crate) struct Ownership {
    anchors: HashMap<Identity, HashSet<String>>,
    snapshot: Snapshot,
}

impl Ownership {
    pub(crate) fn remember(
        &mut self,
        target: &str,
        identities: impl IntoIterator<Item = Identity>,
    ) {
        for identity in identities {
            self.anchors
                .entry(identity)
                .or_default()
                .insert(target.to_owned());
        }
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        self.snapshot.clone()
    }

    pub(crate) fn reconcile(
        &mut self,
        catalog: &Catalog,
        processes: &HashMap<u32, Process>,
    ) -> bool {
        let valid =
            |&(pid, start): &Identity| processes.get(&pid).is_some_and(|p| p.start == start);
        let eligible = |target: &str| {
            catalog
                .by_id(target)
                .is_some_and(|entry| !entry.launch_only)
        };
        self.anchors.retain(|identity, targets| {
            targets.retain(|target| eligible(target));
            valid(identity) && !targets.is_empty()
        });
        let mut next = Snapshot::default();
        for (&identity, targets) in &self.anchors {
            next.owners.insert(
                identity,
                Owner {
                    target: (targets.len() == 1).then(|| targets.iter().next().unwrap().clone()),
                    scope: processes[&identity.0].cgroup.as_deref().map(|path| {
                        catalog
                            .application_cgroup(path)
                            .map_or(path, |(_, path)| path)
                            .to_owned()
                    }),
                },
            );
        }
        // Keep already observed descendants across reparenting, but not a new
        // explicit application boundary, catalog removal, exit or PID reuse.
        for (&identity, owner) in &self.snapshot.owners {
            if valid(&identity)
                && owner.target.as_deref().is_none_or(eligible)
                && can_retain(catalog, processes, &self.anchors, identity, owner)
            {
                next.owners.entry(identity).or_insert_with(|| owner.clone());
            }
        }
        let children = process_children(processes.iter().filter_map(|(&pid, p)| {
            let parent = processes.get(&p.parent)?;
            (p.start >= parent.start).then_some((pid, p.parent))
        }));
        // Ancestors before descendants. Explicit anchors always win; retained
        // descendants are refreshed by their live parent, not traversal order.
        let mut pending = VecDeque::from_iter(
            next.owners
                .keys()
                .filter(|identity| {
                    let process = &processes[&identity.0];
                    self.anchors.contains_key(identity)
                        || !processes.get(&process.parent).is_some_and(|parent| {
                            next.owners
                                .get(&(process.parent, parent.start))
                                .is_some_and(|owner| accepts(catalog, process, owner))
                        })
                })
                .map(|id| id.0),
        );
        let mut visited = HashSet::new();
        while let Some(pid) = pending.pop_front() {
            if !visited.insert(pid) {
                continue;
            }
            let identity = (pid, processes[&pid].start);
            let owner = next.owners[&identity].clone();
            for &child in children.get(&pid).into_iter().flatten() {
                let identity = (child, processes[&child].start);
                if self.anchors.contains_key(&identity)
                    || !accepts(catalog, &processes[&child], &owner)
                {
                    continue;
                }
                if let Some(existing) = next.owners.get(&identity)
                    && existing.target != owner.target
                {
                    // Conflicting lineage is not a reason to choose a winner.
                    next.owners.insert(
                        identity,
                        Owner {
                            target: None,
                            scope: owner.scope.clone(),
                        },
                    );
                } else {
                    next.owners.insert(identity, owner.clone());
                }
                pending.push_back(child);
            }
        }
        let changed = self.snapshot != next;
        self.snapshot = next;
        changed
    }
}

fn can_retain(
    catalog: &Catalog,
    processes: &HashMap<u32, Process>,
    anchors: &HashMap<Identity, HashSet<String>>,
    identity: Identity,
    owner: &Owner,
) -> bool {
    let mut pid = identity.0;
    let mut seen = HashSet::new();
    while seen.insert(pid) {
        let Some(process) = processes.get(&pid) else {
            break;
        };
        if let Some(targets) = anchors.get(&(pid, process.start)) {
            return targets.len() == 1
                && owner
                    .target
                    .as_ref()
                    .is_some_and(|target| targets.contains(target));
        }
        if !accepts(catalog, process, owner) {
            return false;
        }
        if processes
            .get(&process.parent)
            .is_some_and(|parent| parent.start > process.start)
        {
            break;
        }
        pid = process.parent;
    }
    true
}

fn accepts(catalog: &Catalog, process: &Process, owner: &Owner) -> bool {
    process
        .cgroup
        .as_deref()
        .and_then(|path| catalog.application_cgroup(path))
        .is_none_or(|(scoped, path)| {
            Some(scoped) == owner.target.as_deref() || Some(path) == owner.scope.as_deref()
        })
}
