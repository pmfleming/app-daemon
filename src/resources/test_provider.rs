use super::provider::{
    BatterySample, CgroupCounters, DiskBreakdown, EnergyProvider, GpuProcessStat, MemoryUsage,
    NetworkCounters, ProcessFiles, ProcessIo, ProcessStat, ResourceProvider,
};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

#[derive(Debug, Default)]
pub(super) struct TestProvider {
    pub state: Mutex<TestState>,
    pub disk_reads: AtomicU64,
    pub disk_gate: Mutex<Option<Arc<std::sync::Barrier>>>,
    pub disk_started: Mutex<Option<std::sync::mpsc::Sender<String>>>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct TestState {
    pub system_ticks: u64,
    pub rapl: HashMap<PathBuf, (u64, u64)>,
    pub processes: HashMap<u32, ProcessStat>,
    pub io: HashMap<u32, ProcessIo>,
    pub memory: HashMap<u32, MemoryUsage>,
    pub files: HashMap<u32, ProcessFiles>,
    pub sockets: HashMap<u32, HashSet<u64>>,
    pub network: Option<HashMap<u64, NetworkCounters>>,
    pub disk_usage: Option<DiskBreakdown>,
    pub cgroups: HashMap<u32, String>,
}

impl EnergyProvider for TestProvider {
    fn rapl_zones(&self) -> HashMap<PathBuf, (u64, u64)> {
        self.state.lock().unwrap().rapl.clone()
    }
    fn batteries(&self) -> BatterySample {
        BatterySample::default()
    }
}

impl ResourceProvider for TestProvider {
    fn system_cpu(&self) -> (u64, usize) {
        (self.state.lock().unwrap().system_ticks, 1)
    }
    fn processes(&self) -> HashMap<u32, ProcessStat> {
        self.state.lock().unwrap().processes.clone()
    }
    fn process_memory(&self, pid: u32) -> MemoryUsage {
        self.state
            .lock()
            .unwrap()
            .memory
            .get(&pid)
            .copied()
            .unwrap_or_default()
    }
    fn process_io(&self, pid: u32) -> Option<ProcessIo> {
        self.state.lock().unwrap().io.get(&pid).copied()
    }
    fn process_files(&self, pid: u32) -> ProcessFiles {
        self.state
            .lock()
            .unwrap()
            .files
            .get(&pid)
            .cloned()
            .unwrap_or_default()
    }
    fn process_sockets(&self, pid: u32) -> Option<HashSet<u64>> {
        self.state.lock().unwrap().sockets.get(&pid).cloned()
    }
    fn network_counters(&self, inodes: &HashSet<u64>) -> Option<HashMap<u64, NetworkCounters>> {
        Some(
            self.state
                .lock()
                .unwrap()
                .network
                .as_ref()?
                .iter()
                .filter(|(inode, _)| inodes.contains(inode))
                .map(|(&inode, &counters)| (inode, counters))
                .collect(),
        )
    }
    fn gpu_processes(&self, _: &HashSet<u32>) -> HashMap<u32, GpuProcessStat> {
        HashMap::new()
    }
    fn process_cgroup(&self, pid: u32) -> Option<String> {
        self.state.lock().unwrap().cgroups.get(&pid).cloned()
    }
    fn owned_process_cgroups(&self, processes: &HashMap<u32, ProcessStat>) -> HashMap<u32, String> {
        self.state
            .lock()
            .unwrap()
            .cgroups
            .iter()
            .filter(|(pid, _)| processes.contains_key(pid))
            .map(|(&pid, path)| (pid, path.clone()))
            .collect()
    }
    fn cgroup_counters(&self, _: &str) -> Option<CgroupCounters> {
        None
    }
    fn cgroup_members(&self, path: &str) -> HashSet<u32> {
        self.state
            .lock()
            .unwrap()
            .cgroups
            .iter()
            .filter(|(_, group)| group.as_str() == path)
            .map(|(&pid, _)| pid)
            .collect()
    }
    fn application_disk_usage(&self, target: &str) -> Option<DiskBreakdown> {
        self.disk_reads.fetch_add(1, Ordering::Relaxed);
        let gate = self.disk_gate.lock().unwrap().clone();
        if let Some(sender) = &*self.disk_started.lock().unwrap() {
            let _ = sender.send(target.to_owned());
        }
        if let Some(gate) = gate {
            gate.wait();
        }
        self.state.lock().unwrap().disk_usage
    }
}
