use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use crate::process::{process_cgroup, process_stat_fields, read_processes};

use super::{
    energy,
    gpu::read_gpu_processes,
    network::read_network_counters,
    provider::{
        BatterySample, CgroupCounters, DiskBreakdown, DiskFile, DiskFileId, EnergyProvider,
        GpuProcessStat, IoCounters, MemoryUsage, NetworkCounters, ProcessFiles, ProcessIo,
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
    [target.to_owned(), target.to_ascii_lowercase()]
        .into_iter()
        .flat_map(|name| {
            let short = name
                .rsplit('.')
                .next()
                .filter(|name| name.len() >= 4)
                .map(str::to_owned);
            std::iter::once(name).chain(short)
        })
        .collect()
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

#[derive(Default)]
struct AllocatedFiles {
    seen: HashSet<DiskFileId>,
    bytes: u64,
}

impl AllocatedFiles {
    fn include(&mut self, id: DiskFileId, bytes: u64) {
        if self.seen.insert(id) {
            self.bytes = self.bytes.saturating_add(bytes);
        }
    }

    fn scan_directory(&mut self, root: &Path, budget: &mut DiskScanBudget) -> Option<()> {
        budget.visit()?;
        if !is_directory(root)? {
            return Some(());
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
            self.include(
                DiskFileId {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                },
                metadata.blocks().saturating_mul(512),
            );
        }
        Some(())
    }
}

fn allocated_directory_bytes(roots: &[PathBuf], budget: &mut DiskScanBudget) -> Option<u64> {
    let mut files = AllocatedFiles::default();
    roots
        .iter()
        .try_for_each(|root| files.scan_directory(root, budget))?;
    (std::time::Instant::now() < budget.deadline).then_some(files.bytes)
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

fn read_cgroup_counters(path: &str) -> Option<CgroupCounters> {
    let root = Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'));
    read_cgroup_counters_at(&root)
}

fn read_cgroup_counters_at(root: &Path) -> Option<CgroupCounters> {
    let cpu_usage_usec = fs::read_to_string(root.join("cpu.stat"))
        .ok()
        .and_then(|cpu| {
            selected_values(key_values(cpu.lines(), char::is_whitespace), ["usage_usec"])[0]
        });
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

fn parse_cgroup_io(value: &str) -> Option<IoCounters> {
    let mut counters = IoCounters::default();
    // A readable empty io.stat is supported idle accounting; malformed records
    // are unavailable, not fabricated zeros.
    for line in value.lines().filter(|line| !line.trim().is_empty()) {
        let [read, write, reads, writes] = selected_values(
            key_values(line.split_whitespace(), |ch| ch == '='),
            ["rbytes", "wbytes", "rios", "wios"],
        );
        counters.read_bytes = counters.read_bytes.saturating_add(read?);
        counters.write_bytes = counters.write_bytes.saturating_add(write?);
        counters.read_operations = counters.read_operations.saturating_add(reads?);
        counters.write_operations = counters.write_operations.saturating_add(writes?);
    }
    Some(counters)
}

fn key_values<'a>(
    fields: impl Iterator<Item = &'a str>,
    separator: impl Fn(char) -> bool + Copy,
) -> impl Iterator<Item = (&'a str, u64)> {
    fields
        .filter_map(move |field| field.split_once(separator))
        .filter_map(|(key, value)| Some((key, value.trim().parse().ok()?)))
}

// Fixed kernel schemas need neither owned keys nor a heap-allocated lookup table.
// Preserve the old map semantics: the last successfully parsed occurrence wins.
fn selected_values<'a, const N: usize>(
    fields: impl Iterator<Item = (&'a str, u64)>,
    keys: [&str; N],
) -> [Option<u64>; N] {
    let mut values = [None; N];
    for (key, value) in fields {
        if let Some(index) = keys.iter().position(|candidate| *candidate == key) {
            values[index] = Some(value);
        }
    }
    values
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
    ProcessFiles {
        open,
        mapped: read_open_files(&format!("/proc/{pid}/map_files")).unwrap_or_default(),
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
    let mut fields = process_stat_fields(value)?;
    // Read in proc_pid_stat(5) field order; skip counts exclude the consumed field.
    Some(ProcessStat {
        parent_pid: parse_field(&mut fields, 1)?,   // 4: ppid
        major_faults: parse_field(&mut fields, 7)?, // 12: majflt
        total_ticks: parse_field::<u64>(&mut fields, 1)? // 14: utime
            .saturating_add(parse_field(&mut fields, 0)?), // 15: stime
        thread_count: parse_field(&mut fields, 4)?, // 20: num_threads
        start_ticks: parse_field(&mut fields, 1)?,  // 22: starttime
    })
}

fn parse_field<T: std::str::FromStr>(
    fields: &mut std::str::SplitWhitespace<'_>,
    skip: usize,
) -> Option<T> {
    fields.nth(skip)?.parse().ok()
}

fn read_process_io(pid: u32) -> Option<ProcessIo> {
    let value = fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    Some(parse_process_io(&value))
}

fn parse_process_io(value: &str) -> ProcessIo {
    let [
        read_bytes,
        write_bytes,
        read_operations,
        write_operations,
        logical_read_bytes,
        logical_write_bytes,
        cancelled_write_bytes,
    ] = selected_values(
        key_values(value.lines(), |ch| ch == ':'),
        [
            "read_bytes",
            "write_bytes",
            "syscr",
            "syscw",
            "rchar",
            "wchar",
            "cancelled_write_bytes",
        ],
    )
    .map(Option::unwrap_or_default);
    ProcessIo {
        storage: IoCounters {
            read_bytes,
            write_bytes,
            read_operations,
            write_operations,
        },
        logical_read_bytes,
        logical_write_bytes,
        cancelled_write_bytes,
    }
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
    match fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) {
        Ok(rollup) => parse_memory_rollup(&rollup),
        Err(_) => parse_memory_status(
            &fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default(),
        ),
    }
}

fn parse_memory_rollup(rollup: &str) -> MemoryUsage {
    let [rss, pss, clean, dirty, huge, swap_pss, swap] = selected_values(
        memory_bytes(rollup),
        [
            "Rss",
            "Pss",
            "Private_Clean",
            "Private_Dirty",
            "Private_Hugetlb",
            "SwapPss",
            "Swap",
        ],
    );
    MemoryUsage {
        rss_bytes: rss.unwrap_or(0),
        pss_bytes: pss.unwrap_or(0),
        private_bytes: [clean, dirty, huge]
            .into_iter()
            .flatten()
            .fold(0, u64::saturating_add),
        swap_bytes: swap_pss.or(swap).unwrap_or(0),
        rss_available: rss.is_some(),
        pss_available: pss.is_some(),
    }
}

fn parse_memory_status(status: &str) -> MemoryUsage {
    let [rss, swap] = selected_values(memory_bytes(status), ["VmRSS", "VmSwap"]);
    MemoryUsage {
        rss_bytes: rss.unwrap_or(0),
        swap_bytes: swap.unwrap_or(0),
        rss_available: rss.is_some(),
        ..MemoryUsage::default()
    }
}

#[cfg(feature = "benchmarks")]
pub(super) fn benchmark_parsers() {
    use std::hint::black_box;
    black_box(parse_memory_rollup(black_box(
        "Rss: 4096 kB\nPss: 2048 kB\nPrivate_Clean: 16 kB\nPrivate_Dirty: 1024 kB\nSwapPss: 0 kB\n",
    )));
    black_box(parse_memory_status(black_box(
        "Name: app\nVmRSS: 4096 kB\nVmSwap: 0 kB\nThreads: 4\n",
    )));
    black_box(parse_process_io(black_box(
        "rchar: 4096\nwchar: 1024\nsyscr: 8\nsyscw: 2\nread_bytes: 8192\nwrite_bytes: 2048\ncancelled_write_bytes: 0\n",
    )));
    black_box(parse_cgroup_io(black_box(
        "8:0 rbytes=8192 wbytes=2048 rios=8 wios=2 dbytes=0\n",
    )));
}

// Both smaps_rollup and status report memory in KiB. Normalize once at the edge.
fn memory_bytes(value: &str) -> impl Iterator<Item = (&str, u64)> {
    value
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter_map(|(key, value)| {
            let kib: u64 = value.split_whitespace().next()?.parse().ok()?;
            Some((key, kib.saturating_mul(1024)))
        })
}

#[cfg(test)]
mod tests {
    use super::{parse_cgroup_io, parse_memory_rollup, parse_memory_status, parse_process_io};

    #[test]
    fn fixed_fields_preserve_missing_duplicate_and_saturating_semantics() {
        let memory = parse_memory_rollup(
            "Rss: 2 kB\nRss: 4 kB\nRss: bad\nPss: 0 kB\nSwap: 3 kB\nSwapPss: 0 kB\nPrivate_Clean: 18446744073709551615 kB\nPrivate_Dirty: 1 kB\n",
        );
        assert_eq!(
            (memory.rss_bytes, memory.pss_bytes, memory.swap_bytes),
            (4096, 0, 0)
        );
        assert!(memory.rss_available && memory.pss_available);
        assert_eq!(memory.private_bytes, u64::MAX);
        let missing = parse_memory_status("Name: app\nVmRSS: invalid\nVmSwap: 3 kB\n");
        assert!(!missing.rss_available);
        assert_eq!(missing.swap_bytes, 3072);
        let io = parse_process_io(
            "read_bytes: 1\nread_bytes: 7\nread_bytes: bad\nrchar: 9\nunknown: 8\n",
        );
        assert_eq!(
            (
                io.storage.read_bytes,
                io.storage.write_bytes,
                io.logical_read_bytes
            ),
            (7, 0, 9)
        );
        assert_eq!(parse_cgroup_io("").unwrap().read_bytes, 0);
        assert!(parse_cgroup_io("8:0 rbytes=1 wbytes=2 rios=3").is_none());
        let io = parse_cgroup_io("8:0 rbytes=1 wbytes=2 rios=3 wios=4\n8:1 rbytes=5 rbytes=6 rbytes=bad wbytes=7 rios=8 wios=9").unwrap();
        assert_eq!(
            (
                io.read_bytes,
                io.write_bytes,
                io.read_operations,
                io.write_operations
            ),
            (7, 9, 11, 13)
        );
        #[cfg(feature = "benchmarks")]
        assert_eq!(
            allocation_counter::measure(super::benchmark_parsers).count_total,
            0
        );
    }
}
