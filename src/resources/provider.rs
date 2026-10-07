//! Raw observations and the sampling boundary; no procfs, timing, or attribution state.
use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    path::PathBuf,
};

pub(super) trait EnergyProvider {
    fn rapl_zones(&self) -> HashMap<PathBuf, (u64, u64)>;
    fn batteries(&self) -> BatterySample;
}

#[derive(Debug, Default)]
pub(super) struct BatterySample {
    pub full_mwh: f64,
    pub discharge_watts: f64,
}

#[derive(Debug, Default)]
pub(super) struct GpuProcessStat {
    pub engine_nanoseconds: HashMap<String, u64>,
    pub resident_memory_bytes: u64,
    pub allocated_memory_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct NetworkCounters {
    pub received_bytes: u64,
    pub transmitted_bytes: u64,
}

pub(super) trait ResourceProvider: Debug + EnergyProvider + Send + Sync {
    fn system_cpu(&self) -> (u64, usize);
    fn processes(&self) -> HashMap<u32, ProcessStat>;
    fn process_memory(&self, pid: u32) -> MemoryUsage;
    fn process_io(&self, pid: u32) -> Option<ProcessIo>;
    fn process_files(&self, pid: u32) -> ProcessFiles;
    fn process_sockets(&self, pid: u32) -> Option<HashSet<u64>>;
    fn network_counters(&self, inodes: &HashSet<u64>) -> Option<HashMap<u64, NetworkCounters>>;
    fn gpu_processes(&self, pids: &HashSet<u32>) -> HashMap<u32, GpuProcessStat>;
    fn process_cgroup(&self, pid: u32) -> Option<String>;
    fn owned_process_cgroups(&self, processes: &HashMap<u32, ProcessStat>) -> HashMap<u32, String>;
    fn cgroup_counters(&self, path: &str) -> Option<CgroupCounters>;
    fn cgroup_members(&self, path: &str) -> HashSet<u32>;
    fn application_disk_usage(&self, target_id: &str) -> Option<DiskBreakdown>;
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ProcessStat {
    pub parent_pid: u32,
    pub total_ticks: u64,
    pub start_ticks: u64,
    pub major_faults: u64,
    pub thread_count: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ProcessIo {
    pub physical_read_bytes: u64,
    pub physical_write_bytes: u64,
    pub logical_read_bytes: u64,
    pub logical_write_bytes: u64,
    pub read_operations: u64,
    pub write_operations: u64,
    pub cancelled_write_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CgroupCounters {
    pub cpu_usage_usec: Option<u64>,
    pub io: Option<CgroupIo>,
    pub memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CgroupIo {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_operations: u64,
    pub write_operations: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct MemoryUsage {
    pub rss_bytes: u64,
    pub pss_bytes: u64,
    pub private_bytes: u64,
    pub swap_bytes: u64,
    pub rss_available: bool,
    pub pss_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct DiskFileId {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct DiskFile {
    pub bytes: u64,
    pub temporary: bool,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ProcessFiles {
    pub open: HashMap<DiskFileId, DiskFile>,
    pub referenced: HashMap<DiskFileId, DiskFile>,
    pub fd_available: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct DiskBreakdown {
    pub total_bytes: u64,
    pub temporary_bytes: u64,
    pub permanent_bytes: u64,
}
