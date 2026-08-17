use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use super::{
    CgroupCounters, DiskBreakdown, DiskFile, DiskFileId, MemoryUsage, ProcessFiles, ProcessIo,
    ProcessStat, ResourceProvider,
};

pub(super) fn application_disk_usage(target_id: &str) -> DiskBreakdown {
    let target = target_id.trim_end_matches(".desktop");
    if target.is_empty() || target.starts_with("window-group:") {
        return DiskBreakdown::default();
    }
    let lowercase = target.to_ascii_lowercase();
    let mut names = HashSet::from([target.to_owned(), lowercase.clone()]);
    for candidate in [target, &lowercase] {
        if let Some(short) = candidate.rsplit('.').next().filter(|name| name.len() >= 4) {
            names.insert(short.to_owned());
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let data = xdg_directory(
        "XDG_DATA_HOME",
        home.as_ref().map(|path| path.join(".local/share")),
    );
    let config = xdg_directory(
        "XDG_CONFIG_HOME",
        home.as_ref().map(|path| path.join(".config")),
    );
    let state = xdg_directory(
        "XDG_STATE_HOME",
        home.as_ref().map(|path| path.join(".local/state")),
    );
    let cache = xdg_directory(
        "XDG_CACHE_HOME",
        home.as_ref().map(|path| path.join(".cache")),
    );
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let mut permanent_roots = Vec::new();
    let mut temporary_roots = Vec::new();
    for name in names {
        for root in [&data, &config, &state].into_iter().flatten() {
            permanent_roots.push(root.join(&name));
        }
        for root in [&cache, &runtime].into_iter().flatten() {
            temporary_roots.push(root.join(&name));
        }
    }
    if let Some(home) = home {
        for name in [target, &lowercase] {
            let flatpak = home.join(".var/app").join(name);
            permanent_roots.extend([flatpak.join("config"), flatpak.join("data")]);
            temporary_roots.push(flatpak.join("cache"));
        }
    }
    let permanent = allocated_directory_bytes(&permanent_roots);
    let temporary = allocated_directory_bytes(&temporary_roots);
    DiskBreakdown {
        total_bytes: permanent.saturating_add(temporary),
        temporary_bytes: temporary,
        permanent_bytes: permanent,
    }
}

pub(super) fn xdg_directory(variable: &str, fallback: Option<PathBuf>) -> Option<PathBuf> {
    std::env::var_os(variable).map(PathBuf::from).or(fallback)
}

pub(super) fn allocated_directory_bytes(roots: &[PathBuf]) -> u64 {
    let mut files = HashMap::<DiskFileId, u64>::new();
    for root in roots.iter().filter(|path| path.is_dir()) {
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
        {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            files
                .entry(DiskFileId {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                })
                .or_insert_with(|| metadata.blocks().saturating_mul(512));
        }
    }
    files.values().copied().sum()
}

pub(super) fn read_processes() -> HashMap<u32, ProcessStat> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return HashMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            parse_process_stat(&stat).map(|process| (pid, process))
        })
        .collect()
}

pub(super) fn process_children(processes: &HashMap<u32, ProcessStat>) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, process) in processes {
        children.entry(process.parent_pid).or_default().push(pid);
    }
    children
}

pub(super) fn shared_target_pids(
    targets: &HashMap<String, Vec<u32>>,
    children: &HashMap<u32, Vec<u32>>,
    cgroups: &HashMap<u32, HashSet<u32>>,
) -> HashSet<u32> {
    let mut owners = HashMap::<u32, u32>::new();
    for roots in targets.values() {
        let mut target_pids = HashSet::new();
        for root in roots {
            if let Some(members) = cgroups.get(root) {
                target_pids.extend(members);
            } else {
                target_pids.extend(descendants([*root], children));
            }
        }
        for pid in target_pids {
            *owners.entry(pid).or_default() += 1;
        }
    }
    owners
        .into_iter()
        .filter_map(|(pid, owners)| (owners > 1).then_some(pid))
        .collect()
}

pub(super) fn cgroup_paths_for_roots(
    provider: &dyn ResourceProvider,
    roots: &HashSet<u32>,
) -> HashMap<u32, String> {
    roots
        .iter()
        .filter_map(|&root| {
            provider
                .process_cgroup(root)
                .filter(|path| specific_application_cgroup(path))
                .map(|path| (root, path))
        })
        .collect()
}

pub(super) fn cgroup_members_for_paths(
    provider: &dyn ResourceProvider,
    paths: &HashMap<u32, String>,
) -> HashMap<u32, HashSet<u32>> {
    let mut by_path = HashMap::<String, HashSet<u32>>::new();
    paths
        .iter()
        .filter_map(|(&root, path)| {
            let members = by_path
                .entry(path.clone())
                .or_insert_with_key(|path| provider.cgroup_members(path))
                .clone();
            (!members.is_empty()).then_some((root, members))
        })
        .collect()
}

pub(crate) fn process_cgroup(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("0::").map(str::to_owned))
}

pub(super) fn specific_application_cgroup(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    (name.ends_with(".scope") || name.ends_with(".service"))
        && (name.starts_with("app-") || name.contains("flatpak") || name.contains("snap."))
}

pub(super) fn read_cgroup_counters(path: &str) -> Option<CgroupCounters> {
    let root = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    let cpu = fs::read_to_string(root.join("cpu.stat"))
        .ok()
        .map(|value| whitespace_key_values(&value))?;
    let mut counters = CgroupCounters {
        cpu_usage_usec: cpu.get("usage_usec").copied().unwrap_or(0),
        memory_bytes: read_number(&root.join("memory.current")),
        swap_bytes: read_number(&root.join("memory.swap.current")),
        ..CgroupCounters::default()
    };
    if let Ok(io) = fs::read_to_string(root.join("io.stat")) {
        for values in io.lines().map(equals_key_values) {
            counters.read_bytes = counters
                .read_bytes
                .saturating_add(values.get("rbytes").copied().unwrap_or(0));
            counters.write_bytes = counters
                .write_bytes
                .saturating_add(values.get("wbytes").copied().unwrap_or(0));
            counters.read_operations = counters
                .read_operations
                .saturating_add(values.get("rios").copied().unwrap_or(0));
            counters.write_operations = counters
                .write_operations
                .saturating_add(values.get("wios").copied().unwrap_or(0));
        }
    }
    Some(counters)
}

pub(super) fn whitespace_key_values(value: &str) -> HashMap<String, u64> {
    value
        .lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .filter_map(|(key, value)| Some((key.to_owned(), value.trim().parse().ok()?)))
        .collect()
}

pub(super) fn equals_key_values(value: &str) -> HashMap<&str, u64> {
    value
        .split_whitespace()
        .filter_map(|field| field.split_once('='))
        .filter_map(|(key, value)| Some((key, value.parse().ok()?)))
        .collect()
}

pub(super) fn read_number(path: &Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

pub(super) fn read_cgroup_members(path: &str) -> HashSet<u32> {
    let root = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == "cgroup.procs")
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .flat_map(|value| {
            value
                .lines()
                .filter_map(|line| line.parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

pub(super) fn descendants(
    roots: impl IntoIterator<Item = u32>,
    children: &HashMap<u32, Vec<u32>>,
) -> HashSet<u32> {
    let mut pending = roots.into_iter().filter(|pid| *pid > 0).collect::<Vec<_>>();
    let mut included = HashSet::new();
    while let Some(pid) = pending.pop() {
        if included.insert(pid)
            && let Some(process_children) = children.get(&pid)
        {
            pending.extend(process_children);
        }
    }
    included
}

pub(super) fn read_process_file_sets(pid: u32) -> ProcessFiles {
    let fd_directory = format!("/proc/{pid}/fd");
    let Some((open, sockets)) = read_open_files_and_sockets(&fd_directory) else {
        return ProcessFiles::default();
    };
    let mut referenced = open.clone();
    merge_disk_files(
        &mut referenced,
        &read_regular_files(&format!("/proc/{pid}/map_files")),
    );
    ProcessFiles {
        open,
        referenced,
        sockets,
        fd_available: true,
    }
}

pub(super) fn read_open_files_and_sockets(
    directory: &str,
) -> Option<(HashMap<DiskFileId, DiskFile>, HashSet<u64>)> {
    let entries = fs::read_dir(directory).ok()?;
    let mut files = HashMap::new();
    let mut sockets = HashSet::new();
    for entry in entries.filter_map(Result::ok) {
        let Ok(link) = fs::read_link(entry.path()) else {
            continue;
        };
        if let Some(inode) = link
            .to_str()
            .and_then(|value| value.strip_prefix("socket:["))
            .and_then(|value| value.strip_suffix(']'))
            .and_then(|value| value.parse().ok())
        {
            sockets.insert(inode);
        }
        let Ok(metadata) = fs::metadata(entry.path()) else {
            continue;
        };
        if metadata.file_type().is_file() {
            files.insert(
                DiskFileId {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                },
                DiskFile {
                    bytes: metadata.blocks().saturating_mul(512),
                    temporary: temporary_path(&link),
                },
            );
        }
    }
    Some((files, sockets))
}

pub(super) fn read_regular_files(directory: &str) -> HashMap<DiskFileId, DiskFile> {
    let Ok(entries) = fs::read_dir(directory) else {
        return HashMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let link = fs::read_link(entry.path()).ok()?;
            let metadata = fs::metadata(entry.path()).ok()?;
            metadata.file_type().is_file().then(|| {
                (
                    DiskFileId {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                    },
                    DiskFile {
                        bytes: metadata.blocks().saturating_mul(512),
                        temporary: temporary_path(&link),
                    },
                )
            })
        })
        .collect()
}

pub(super) fn merge_disk_files(
    target: &mut HashMap<DiskFileId, DiskFile>,
    source: &HashMap<DiskFileId, DiskFile>,
) {
    for (&id, &file) in source {
        target
            .entry(id)
            .and_modify(|current| {
                current.bytes = current.bytes.max(file.bytes);
                current.temporary |= file.temporary;
            })
            .or_insert(file);
    }
}

pub(super) fn temporary_path(path: &Path) -> bool {
    path.starts_with("/tmp")
        || path.starts_with("/var/tmp")
        || path.starts_with("/dev/shm")
        || std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|root| path.starts_with(root))
        || std::env::var_os("XDG_CACHE_HOME").is_some_and(|root| path.starts_with(root))
        || std::env::var_os("HOME")
            .is_some_and(|home| path.starts_with(Path::new(&home).join(".cache")))
}

pub(super) fn parse_process_stat(value: &str) -> Option<ProcessStat> {
    let fields = process_stat_fields(value)?;
    Some(ProcessStat {
        parent_pid: parse_field(&fields, 1)?,
        total_ticks: parse_field::<u64>(&fields, 11)?.saturating_add(parse_field(&fields, 12)?),
        start_ticks: parse_field(&fields, 19)?,
        major_faults: parse_field(&fields, 9)?,
        thread_count: parse_field(&fields, 17)?,
    })
}

pub(super) fn process_stat_fields(value: &str) -> Option<Vec<&str>> {
    let command_end = value.rfind(')')?;
    Some(value.get(command_end + 1..)?.split_whitespace().collect())
}

pub(super) fn parse_field<T: std::str::FromStr>(fields: &[&str], index: usize) -> Option<T> {
    fields.get(index)?.parse().ok()
}

pub(super) fn read_process_io(pid: u32) -> Option<ProcessIo> {
    let value = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    let values = numeric_key_values(&value);
    Some(ProcessIo {
        physical_read_bytes: values.get("read_bytes").copied().unwrap_or(0),
        physical_write_bytes: values.get("write_bytes").copied().unwrap_or(0),
        logical_read_bytes: values.get("rchar").copied().unwrap_or(0),
        logical_write_bytes: values.get("wchar").copied().unwrap_or(0),
        read_operations: values.get("syscr").copied().unwrap_or(0),
        write_operations: values.get("syscw").copied().unwrap_or(0),
        cancelled_write_bytes: values.get("cancelled_write_bytes").copied().unwrap_or(0),
    })
}

pub(super) fn numeric_key_values(value: &str) -> HashMap<&str, u64> {
    value
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter_map(|(key, value)| Some((key, value.trim().parse().ok()?)))
        .collect()
}

pub(super) fn read_system_cpu() -> (u64, usize) {
    let Ok(stat) = fs::read_to_string("/proc/stat") else {
        return (0, 1);
    };
    let mut total = 0_u64;
    let mut logical_cpus = 0_usize;
    for line in stat.lines() {
        if let Some(values) = line.strip_prefix("cpu ") {
            let fields = values
                .split_whitespace()
                .filter_map(|value| value.parse::<u64>().ok())
                .collect::<Vec<_>>();
            // The first eight counters include steal but exclude guest and guest_nice,
            // which are already represented in user and nice.
            total = fields.iter().take(8).copied().sum::<u64>();
        } else if line
            .strip_prefix("cpu")
            .and_then(|value| value.split_whitespace().next())
            .is_some_and(|value| value.chars().all(|character| character.is_ascii_digit()))
        {
            logical_cpus += 1;
        }
    }
    (total, logical_cpus.max(1))
}

pub(super) fn read_process_memory(pid: u32) -> MemoryUsage {
    let rollup = fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()
        .map(|value| memory_key_values(&value));
    if let Some(values) = rollup {
        let private_kib = values
            .get("Private_Clean")
            .copied()
            .unwrap_or(0)
            .saturating_add(values.get("Private_Dirty").copied().unwrap_or(0))
            .saturating_add(values.get("Private_Hugetlb").copied().unwrap_or(0));
        return MemoryUsage {
            rss_bytes: values.get("Rss").copied().unwrap_or(0).saturating_mul(1024),
            pss_bytes: values.get("Pss").copied().unwrap_or(0).saturating_mul(1024),
            private_bytes: private_kib.saturating_mul(1024),
            swap_bytes: values
                .get("SwapPss")
                .or_else(|| values.get("Swap"))
                .copied()
                .unwrap_or(0)
                .saturating_mul(1024),
            rss_available: values.contains_key("Rss"),
            pss_available: values.contains_key("Pss"),
        };
    }
    let values = fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .map(|value| memory_key_values(&value))
        .unwrap_or_default();
    MemoryUsage {
        rss_bytes: values
            .get("VmRSS")
            .copied()
            .unwrap_or(0)
            .saturating_mul(1024),
        swap_bytes: values
            .get("VmSwap")
            .copied()
            .unwrap_or(0)
            .saturating_mul(1024),
        rss_available: values.contains_key("VmRSS"),
        ..MemoryUsage::default()
    }
}

pub(super) fn memory_key_values(value: &str) -> HashMap<String, u64> {
    value
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter_map(|(key, value)| {
            Some((
                key.to_owned(),
                value.split_whitespace().next()?.parse().ok()?,
            ))
        })
        .collect()
}
