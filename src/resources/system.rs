use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use crate::process::{descendants, process_cgroup, process_stat_fields, read_processes};

use super::{
    energy,
    gpu::read_gpu_processes,
    network::read_network_counters,
    provider::{
        BatterySample, CgroupCounters, CgroupIo, DiskBreakdown, DiskFile, DiskFileId,
        EnergyProvider, GpuProcessStat, MemoryUsage, NetworkCounters, ProcessFiles, ProcessIo,
        ProcessStat, ResourceProvider,
    },
};

#[derive(Debug, Default)]
pub(super) struct LinuxResourceProvider;

impl EnergyProvider for LinuxResourceProvider {
    fn rapl_zones(&self) -> HashMap<PathBuf, (u64, u64)> {
        energy::read_rapl_zones()
    }

    fn batteries(&self) -> BatterySample {
        energy::read_batteries()
    }
}

impl ResourceProvider for LinuxResourceProvider {
    fn system_cpu(&self) -> (u64, usize) {
        read_system_cpu()
    }

    fn processes(&self) -> HashMap<u32, ProcessStat> {
        read_processes(parse_process_stat)
    }

    fn process_memory(&self, pid: u32) -> MemoryUsage {
        read_process_memory(pid)
    }

    fn process_io(&self, pid: u32) -> Option<ProcessIo> {
        read_process_io(pid)
    }

    fn process_files(&self, pid: u32) -> ProcessFiles {
        read_process_file_sets(pid)
    }

    fn process_sockets(&self, pid: u32) -> Option<HashSet<u64>> {
        read_process_sockets(pid)
    }

    fn network_counters(&self, inodes: &HashSet<u64>) -> Option<HashMap<u64, NetworkCounters>> {
        read_network_counters(inodes)
    }

    fn gpu_processes(&self, pids: &HashSet<u32>) -> HashMap<u32, GpuProcessStat> {
        read_gpu_processes(pids)
    }

    fn process_cgroup(&self, pid: u32) -> Option<String> {
        process_cgroup(pid)
    }

    fn owned_process_cgroups(&self, processes: &HashMap<u32, ProcessStat>) -> HashMap<u32, String> {
        owned_process_cgroups(processes)
    }

    fn cgroup_counters(&self, path: &str) -> Option<CgroupCounters> {
        read_cgroup_counters(path)
    }

    fn cgroup_members(&self, path: &str) -> HashSet<u32> {
        read_cgroup_members(path)
    }

    fn application_disk_usage(&self, target_id: &str) -> Option<DiskBreakdown> {
        application_disk_usage(target_id)
    }
}

fn application_disk_usage(target_id: &str) -> Option<DiskBreakdown> {
    let target = target_id.trim_end_matches(".desktop");
    if target.is_empty() || target.starts_with("window-group:") {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let names = application_directory_names(target);
    let mut permanent_roots = named_roots(
        &names,
        xdg_roots(
            &home,
            [
                ("XDG_DATA_HOME", ".local/share"),
                ("XDG_CONFIG_HOME", ".config"),
                ("XDG_STATE_HOME", ".local/state"),
            ],
        ),
    );
    let mut temporary_roots = named_roots(
        &names,
        xdg_roots(&home, [("XDG_CACHE_HOME", ".cache")])
            .into_iter()
            .chain(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)),
    );
    append_flatpak_roots(&mut permanent_roots, &mut temporary_roots, home, target);
    let mut budget = DiskScanBudget::default();
    let permanent = allocated_directory_bytes(&permanent_roots, &mut budget)?;
    let temporary = allocated_directory_bytes(&temporary_roots, &mut budget)?;
    Some(DiskBreakdown {
        total_bytes: permanent.saturating_add(temporary),
        temporary_bytes: temporary,
        permanent_bytes: permanent,
    })
}

fn application_directory_names(target: &str) -> HashSet<String> {
    let lowercase = target.to_ascii_lowercase();
    let mut names = HashSet::from([target.to_owned(), lowercase.clone()]);
    for candidate in [target, &lowercase] {
        if let Some(short) = candidate.rsplit('.').next().filter(|name| name.len() >= 4) {
            names.insert(short.to_owned());
        }
    }
    names
}

fn xdg_roots<const N: usize>(home: &Option<PathBuf>, roots: [(&str, &str); N]) -> Vec<PathBuf> {
    roots
        .into_iter()
        .filter_map(|(variable, fallback)| {
            xdg_directory(variable, home.as_ref().map(|path| path.join(fallback)))
        })
        .collect()
}

fn named_roots(names: &HashSet<String>, roots: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    roots
        .into_iter()
        .flat_map(|root| names.iter().map(move |name| root.join(name)))
        .collect()
}

fn append_flatpak_roots(
    permanent: &mut Vec<PathBuf>,
    temporary: &mut Vec<PathBuf>,
    home: Option<PathBuf>,
    target: &str,
) {
    let Some(home) = home else { return };
    let lowercase = target.to_ascii_lowercase();
    for name in [target, &lowercase] {
        let root = home.join(".var/app").join(name);
        permanent.extend([root.join("config"), root.join("data")]);
        temporary.push(root.join("cache"));
    }
}

fn xdg_directory(variable: &str, fallback: Option<PathBuf>) -> Option<PathBuf> {
    std::env::var_os(variable).map(PathBuf::from).or(fallback)
}

struct DiskScanBudget {
    remaining_entries: usize,
    deadline: std::time::Instant,
}

impl Default for DiskScanBudget {
    fn default() -> Self {
        Self {
            remaining_entries: 100_000,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(2),
        }
    }
}

impl DiskScanBudget {
    fn visit(&mut self) -> Option<()> {
        self.remaining_entries = self.remaining_entries.checked_sub(1)?;
        (std::time::Instant::now() < self.deadline).then_some(())
    }
}

fn is_directory(root: &Path) -> Option<bool> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => Some(metadata.is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

fn allocated_directory_bytes(roots: &[PathBuf], budget: &mut DiskScanBudget) -> Option<u64> {
    let mut files = HashMap::<DiskFileId, u64>::new();
    for root in roots {
        budget.visit()?;
        if !is_directory(root)? {
            continue;
        }
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .follow_root_links(false)
            .same_file_system(true)
            .max_open(8)
        {
            budget.visit()?;
            let entry = entry.ok()?;
            if !entry.file_type().is_file() {
                continue;
            }
            let metadata = entry.metadata().ok()?;
            files
                .entry(DiskFileId {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                })
                .or_insert_with(|| metadata.blocks().saturating_mul(512));
        }
    }
    (std::time::Instant::now() < budget.deadline).then(|| files.values().copied().sum())
}

pub(super) fn shared_target_pids(
    targets: &HashMap<String, Vec<u32>>,
    children: &HashMap<u32, Vec<u32>>,
    cgroups: &HashMap<u32, HashSet<u32>>,
) -> HashSet<u32> {
    let mut owners = HashMap::<u32, u32>::new();
    for roots in targets.values() {
        for pid in target_processes(roots, children, cgroups) {
            *owners.entry(pid).or_default() += 1;
        }
    }
    owners
        .into_iter()
        .filter_map(|(pid, owners)| (owners > 1).then_some(pid))
        .collect()
}

fn target_processes(
    roots: &[u32],
    children: &HashMap<u32, Vec<u32>>,
    cgroups: &HashMap<u32, HashSet<u32>>,
) -> HashSet<u32> {
    let mut pids = HashSet::new();
    for root in roots {
        pids.extend(descendants([*root], children));
        pids.extend(cgroups.get(root).into_iter().flatten());
    }
    pids
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
                .and_then(|path| application_cgroup_path(&path).map(str::to_owned))
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

fn owned_process_cgroups(processes: &HashMap<u32, ProcessStat>) -> HashMap<u32, String> {
    let Ok(current) = fs::metadata("/proc/self") else {
        return HashMap::new();
    };
    let uid = current.uid();
    processes
        .keys()
        .filter_map(|&pid| {
            let metadata = fs::metadata(format!("/proc/{pid}")).ok()?;
            (metadata.uid() == uid)
                .then(|| process_cgroup(pid))
                .flatten()
                .map(|path| (pid, path))
        })
        .collect()
}

fn application_cgroup_path(mut path: &str) -> Option<&str> {
    loop {
        if specific_application_cgroup(path) {
            return Some(path);
        }
        path = path.rsplit_once('/')?.0;
    }
}

fn specific_application_cgroup(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    name != "app-daemon.service"
        && (name.ends_with(".scope") || name.ends_with(".service"))
        && (name.starts_with("app-") || name.contains("flatpak") || name.contains("snap."))
}

fn read_cgroup_counters(path: &str) -> Option<CgroupCounters> {
    let root = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    read_cgroup_counters_at(&root)
}

fn read_cgroup_counters_at(root: &Path) -> Option<CgroupCounters> {
    let cpu_usage_usec = fs::read_to_string(root.join("cpu.stat"))
        .ok()
        .and_then(|cpu| whitespace_key_values(&cpu).get("usage_usec").copied());
    let memory_bytes = fs::read_to_string(root.join("memory.current"))
        .ok()
        .and_then(|value| value.trim().parse().ok());
    let io = fs::read_to_string(root.join("io.stat"))
        .ok()
        .and_then(|io| parse_cgroup_io(&io));
    (cpu_usage_usec.is_some() || memory_bytes.is_some() || io.is_some()).then_some(CgroupCounters {
        cpu_usage_usec,
        memory_bytes,
        io,
    })
}

fn parse_cgroup_io(value: &str) -> Option<CgroupIo> {
    let mut counters = CgroupIo::default();
    // A readable empty io.stat is supported idle accounting; malformed records
    // are unavailable, not fabricated zeros.
    for line in value.lines().filter(|line| !line.trim().is_empty()) {
        let values = equals_key_values(line);
        counters.read_bytes = counters.read_bytes.saturating_add(*values.get("rbytes")?);
        counters.write_bytes = counters.write_bytes.saturating_add(*values.get("wbytes")?);
        counters.read_operations = counters
            .read_operations
            .saturating_add(*values.get("rios")?);
        counters.write_operations = counters
            .write_operations
            .saturating_add(*values.get("wios")?);
    }
    Some(counters)
}

fn whitespace_key_values(value: &str) -> HashMap<&str, u64> {
    value
        .lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .filter_map(|(key, value)| Some((key, value.trim().parse().ok()?)))
        .collect()
}

fn equals_key_values(value: &str) -> HashMap<&str, u64> {
    value
        .split_whitespace()
        .filter_map(|field| field.split_once('='))
        .filter_map(|(key, value)| Some((key, value.parse().ok()?)))
        .collect()
}

fn read_cgroup_members(path: &str) -> HashSet<u32> {
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

fn read_process_file_sets(pid: u32) -> ProcessFiles {
    let fd_directory = format!("/proc/{pid}/fd");
    let Some(open) = read_open_files(&fd_directory) else {
        return ProcessFiles::default();
    };
    let mut referenced = open.clone();
    merge_disk_files(
        &mut referenced,
        &read_open_files(&format!("/proc/{pid}/map_files")).unwrap_or_default(),
    );
    ProcessFiles {
        open,
        referenced,
        fd_available: true,
    }
}

fn read_process_sockets(pid: u32) -> Option<HashSet<u64>> {
    let entries = fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    Some(
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let link = fs::read_link(entry.path()).ok()?;
                link.to_str()?
                    .strip_prefix("socket:[")?
                    .strip_suffix(']')?
                    .parse()
                    .ok()
            })
            .collect(),
    )
}

fn read_open_files(directory: &str) -> Option<HashMap<DiskFileId, DiskFile>> {
    let entries = fs::read_dir(directory).ok()?;
    Some(
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
            .collect(),
    )
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

fn temporary_path(path: &Path) -> bool {
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

fn parse_field<T: std::str::FromStr>(fields: &[&str], index: usize) -> Option<T> {
    fields.get(index)?.parse().ok()
}

fn read_process_io(pid: u32) -> Option<ProcessIo> {
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

fn numeric_key_values(value: &str) -> HashMap<&str, u64> {
    value
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter_map(|(key, value)| Some((key, value.trim().parse().ok()?)))
        .collect()
}

fn read_system_cpu() -> (u64, usize) {
    let Ok(stat) = fs::read_to_string("/proc/stat") else {
        return (0, 1);
    };
    let mut total = 0_u64;
    let mut logical_cpus = 0_usize;
    for line in stat.lines() {
        if let Some(values) = line.strip_prefix("cpu ") {
            // Exclude guest and guest_nice, already represented in user and nice.
            total = values
                .split_whitespace()
                .filter_map(|value| value.parse::<u64>().ok())
                .take(8)
                .sum();
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

fn read_process_memory(pid: u32) -> MemoryUsage {
    if let Ok(rollup) = fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) {
        let values = memory_key_values(&rollup);
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
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let values = memory_key_values(&status);
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

#[cfg(test)]
mod cgroup_tests;
#[cfg(test)]
mod disk_tests;

fn memory_key_values(value: &str) -> HashMap<&str, u64> {
    value
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter_map(|(key, value)| Some((key, value.split_whitespace().next()?.parse().ok()?)))
        .collect()
}
