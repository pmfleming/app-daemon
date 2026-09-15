use super::{ProcessStat, ResourceProvider};
use crate::catalog::Catalog;
use std::collections::{HashMap, HashSet, hash_map::Entry};

#[derive(Debug, Default)]
pub(super) struct KnownRoots {
    identities: HashMap<String, HashMap<u32, u64>>,
}

pub(super) struct Targets {
    pub roots: HashMap<String, Vec<u32>>,
    pub owners: HashMap<u32, String>,
}

impl KnownRoots {
    pub fn discover(
        &mut self,
        provider: &dyn ResourceProvider,
        windows: &HashMap<String, Vec<u32>>,
        processes: &HashMap<u32, ProcessStat>,
        catalog: &Catalog,
    ) -> Targets {
        // Group paths before resolving catalog identity: many processes share a unit.
        let mut groups = HashMap::<String, Vec<u32>>::new();
        for (pid, path) in provider.owned_process_cgroups(processes) {
            groups.entry(path).or_default().push(pid);
        }
        let mut owners = HashMap::new();
        let mut members = HashMap::<String, HashSet<u32>>::new();
        for (path, pids) in groups {
            if let Some(id) = catalog.target_for_cgroup(&path) {
                owners.extend(pids.iter().map(|&pid| (pid, id.clone())));
                members.entry(id).or_default().extend(pids);
            }
        }
        for (id, pids) in &self.identities {
            if catalog.by_id(id).is_some_and(|entry| entry.launch_only) {
                continue;
            }
            for (&pid, &start) in pids {
                let reassigned = windows
                    .iter()
                    .any(|(other, pids)| other != id && pids.contains(&pid))
                    && !windows.get(id).is_some_and(|pids| pids.contains(&pid));
                if !reassigned
                    && processes
                        .get(&pid)
                        .is_some_and(|process| process.start_ticks == start)
                    && owners.get(&pid).is_none_or(|owner| owner == id)
                {
                    members.entry(id.clone()).or_default().insert(pid);
                }
            }
        }
        for (id, pids) in windows {
            if catalog.by_id(id).is_some_and(|entry| entry.launch_only) {
                continue;
            }
            members.entry(id.clone()).or_default().extend(
                pids.iter()
                    .filter(|pid| processes.contains_key(pid))
                    .copied(),
            );
        }
        let children = super::system::process_children(processes);
        inherit_owners(&mut owners, &children);
        // Remember observed children as PID/start-time identities too, so a helper
        // does not disappear from accounting merely because its parent/window exits.
        for (id, pids) in &mut members {
            *pids = super::system::descendants_where(pids.drain(), &children, |pid| {
                owners.get(&pid).is_none_or(|owner| owner == id)
            });
        }
        members.retain(|_, pids| !pids.is_empty());
        self.identities = members
            .iter()
            .map(|(id, pids)| {
                (
                    id.clone(),
                    pids.iter()
                        .filter_map(|pid| Some((*pid, processes.get(pid)?.start_ticks)))
                        .collect(),
                )
            })
            .collect();
        let roots = members
            .into_iter()
            .map(|(id, pids)| {
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
                (id, roots)
            })
            .collect();
        Targets { roots, owners }
    }
}

/// Propagate definite cgroup ownership to helpers lacking their own observation.
/// Seed all explicit owners first, so nested application boundaries always win.
fn inherit_owners(owners: &mut HashMap<u32, String>, children: &HashMap<u32, Vec<u32>>) {
    let mut pending = owners.keys().copied().collect::<Vec<_>>();
    while let Some(pid) = pending.pop() {
        let Some(owner) = owners.get(&pid).cloned() else {
            continue;
        };
        for &child in children.get(&pid).into_iter().flatten() {
            if let Entry::Vacant(entry) = owners.entry(child) {
                entry.insert(owner.clone());
                pending.push(child);
            }
        }
    }
}
