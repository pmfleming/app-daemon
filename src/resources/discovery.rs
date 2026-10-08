use super::provider::{ProcessStat, ResourceProvider};
use crate::{
    catalog::Catalog,
    process::{descendants_where, process_children},
};
use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    sync::Arc,
};

type Members = HashMap<String, HashSet<u32>>;
type Owners = HashMap<u32, Arc<str>>;

#[derive(Debug, Default)]
pub(super) struct KnownRoots {
    identities: HashMap<String, HashMap<u32, u64>>,
}

pub(super) struct Targets {
    pub roots: HashMap<String, Vec<u32>>,
    pub owners: Owners,
}

impl KnownRoots {
    pub fn discover(
        &mut self,
        provider: &dyn ResourceProvider,
        windows: &HashMap<String, Vec<u32>>,
        processes: &HashMap<u32, ProcessStat>,
        catalog: &Catalog,
        ownership: &crate::ownership::Snapshot,
    ) -> Targets {
        let (mut members, mut owners) = scoped_members(provider, processes, catalog);
        apply_ownership(&mut members, &mut owners, processes, ownership);
        self.restore(&mut members, &owners, windows, processes);
        for (id, pids) in windows {
            members.entry(id.clone()).or_default().extend(
                pids.iter()
                    .filter(|pid| processes.contains_key(pid))
                    .copied(),
            );
        }
        members.retain(|id, _| !catalog.by_id(id).is_some_and(|entry| entry.launch_only));
        let children =
            process_children(processes.iter().map(|(&pid, stat)| (pid, stat.parent_pid)));
        inherit_owners(&mut owners, &children);
        let mut roots = HashMap::new();
        for (id, pids) in members {
            // Retain children as PID/start-time identities after their window or
            // parent exits, but never traverse another application's boundary.
            let pids = descendants_where(pids, &children, |pid| {
                owners.get(&pid).is_none_or(|owner| owner.as_ref() == id)
            });
            if pids.is_empty() {
                continue;
            }
            roots.insert(id.clone(), root_pids(&pids, processes));
            self.identities.insert(
                id,
                pids.into_iter()
                    .filter_map(|pid| Some((pid, processes.get(&pid)?.start_ticks)))
                    .collect(),
            );
        }
        Targets { roots, owners }
    }

    fn restore(
        &mut self,
        members: &mut Members,
        owners: &Owners,
        windows: &HashMap<String, Vec<u32>>,
        processes: &HashMap<u32, ProcessStat>,
    ) {
        for (id, mut known) in std::mem::take(&mut self.identities) {
            known.retain(|pid, start| {
                let same_process = processes
                    .get(pid)
                    .is_some_and(|process| process.start_ticks == *start);
                let same_owner = owners.get(pid).is_none_or(|owner| owner.as_ref() == id);
                let reassigned = !windows.get(&id).is_some_and(|pids| pids.contains(pid))
                    && windows.values().any(|pids| pids.contains(pid));
                same_process && same_owner && !reassigned
            });
            members.entry(id).or_default().extend(known.into_keys());
        }
    }
}

// Verified ownership follows the process, not the name of a migrated browser scope.
fn apply_ownership(
    members: &mut Members,
    owners: &mut Owners,
    processes: &HashMap<u32, ProcessStat>,
    ownership: &crate::ownership::Snapshot,
) {
    for ((pid, _), target) in ownership.entries().filter(|((pid, start), _)| {
        processes
            .get(pid)
            .is_some_and(|process| process.start_ticks == *start)
    }) {
        if let Some(previous) = owners
            .remove(&pid)
            .and_then(|id| members.get_mut(id.as_ref()))
        {
            previous.remove(&pid);
        }
        owners.insert(pid, target.unwrap_or("ownership-conflict").into());
        if let Some(target) = target {
            members.entry(target.to_owned()).or_default().insert(pid);
        }
    }
}

fn scoped_members(
    provider: &dyn ResourceProvider,
    processes: &HashMap<u32, ProcessStat>,
    catalog: &Catalog,
) -> (Members, Owners) {
    // Resolve each unique cgroup only once, without cloning its catalog ID.
    let mut groups = HashMap::<String, Vec<u32>>::new();
    for (pid, path) in provider.owned_process_cgroups(processes) {
        groups.entry(path).or_default().push(pid);
    }
    let mut members = Members::new();
    let mut owners = Owners::new();
    for (path, pids) in groups {
        if let Some(id) = catalog.target_for_cgroup(&path) {
            let owner = Arc::<str>::from(id);
            owners.extend(pids.iter().map(|&pid| (pid, Arc::clone(&owner))));
            members.entry(id.to_owned()).or_default().extend(pids);
        }
    }
    (members, owners)
}

fn root_pids(pids: &HashSet<u32>, processes: &HashMap<u32, ProcessStat>) -> Vec<u32> {
    let mut roots = pids
        .iter()
        .filter(|pid| {
            processes
                .get(pid)
                .is_none_or(|process| !pids.contains(&process.parent_pid))
        })
        .copied()
        .collect::<Vec<_>>();
    if roots.is_empty() {
        roots.extend(pids.iter().min().copied());
    }
    roots.sort_unstable();
    roots
}

/// Propagate definite cgroup ownership to helpers lacking their own observation.
/// Seed all explicit owners first, so nested application boundaries always win.
fn inherit_owners(owners: &mut Owners, children: &HashMap<u32, Vec<u32>>) {
    let mut pending = owners.keys().copied().collect::<Vec<_>>();
    while let Some(pid) = pending.pop() {
        let Some(owner) = owners.get(&pid).cloned() else {
            continue;
        };
        for &child in children.get(&pid).into_iter().flatten() {
            if let Entry::Vacant(entry) = owners.entry(child) {
                entry.insert(Arc::clone(&owner));
                pending.push(child);
            }
        }
    }
}
