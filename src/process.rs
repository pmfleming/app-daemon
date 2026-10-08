//! Shared /proc reads and cycle-safe process-tree traversal.
use std::{
    collections::{HashMap, HashSet},
    fs,
};

pub(crate) type Identity = (u32, u64);

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Process {
    pub(crate) parent: u32,
    pub(crate) start: u64,
    pub(crate) cgroup: Option<String>,
}

pub(crate) fn process_identity(pid: u32) -> Option<Identity> {
    Some((pid, read_process(pid)?.start))
}

pub(crate) fn read_process(pid: u32) -> Option<Process> {
    parse_process(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

pub(crate) fn parse_process(stat: &str) -> Option<Process> {
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

/// Exclude impossible ancestry edges caused by PID reuse during a procfs scan.
pub(crate) fn live_process_children(processes: &HashMap<u32, Process>) -> HashMap<u32, Vec<u32>> {
    process_children(processes.iter().filter_map(|(&pid, process)| {
        let parent = processes.get(&process.parent)?;
        (process.start >= parent.start).then_some((pid, process.parent))
    }))
}

pub(crate) fn process_cgroup(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("0::").map(str::to_owned))
}

pub(crate) fn read_processes<T>(parse: impl Fn(&str) -> Option<T>) -> HashMap<u32, T> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return HashMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            Some((pid, parse(&stat)?))
        })
        .collect()
}

pub(crate) fn process_stat_fields(value: &str) -> Option<Vec<&str>> {
    // comm can contain spaces and parentheses; state follows its final ')'.
    Some(value.rsplit_once(')')?.1.split_whitespace().collect())
}

pub(crate) fn process_children(
    parents: impl IntoIterator<Item = (u32, u32)>,
) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, parent) in parents {
        children.entry(parent).or_default().push(pid);
    }
    children
}

pub(crate) fn descendants(
    roots: impl IntoIterator<Item = u32>,
    children: &HashMap<u32, Vec<u32>>,
) -> HashSet<u32> {
    descendants_where(roots.into_iter().filter(|pid| *pid > 0), children, |_| true)
}

/// Traverse accepted processes only: rejecting a parent also prunes its children.
pub(crate) fn descendants_where(
    roots: impl IntoIterator<Item = u32>,
    children: &HashMap<u32, Vec<u32>>,
    accept: impl Fn(u32) -> bool,
) -> HashSet<u32> {
    let mut pending = roots.into_iter().collect::<Vec<_>>();
    let mut included = HashSet::new();
    while let Some(pid) = pending.pop() {
        if accept(pid) && included.insert(pid) {
            pending.extend(children.get(&pid).into_iter().flatten());
        }
    }
    included
}
