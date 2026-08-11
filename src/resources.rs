use std::{
    collections::{HashMap, HashSet},
    fs,
    time::Instant,
};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ResourceUsage {
    pub cpu_percent: f64,
    pub memory_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct ProcessStat {
    parent_pid: u32,
    total_ticks: u64,
    memory_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct ProcessUsage {
    parent_pid: u32,
    cpu_percent: f64,
    memory_bytes: u64,
}

#[derive(Debug, Default)]
pub struct ResourceSnapshot {
    processes: HashMap<u32, ProcessUsage>,
    children: HashMap<u32, Vec<u32>>,
}

impl ResourceSnapshot {
    pub fn usage_for_roots(&self, roots: impl IntoIterator<Item = u32>) -> ResourceUsage {
        let mut pending = roots.into_iter().filter(|pid| *pid > 0).collect::<Vec<_>>();
        let mut included = HashSet::new();
        while let Some(pid) = pending.pop() {
            if !included.insert(pid) {
                continue;
            }
            if let Some(children) = self.children.get(&pid) {
                pending.extend(children);
            }
        }

        let mut usage = ResourceUsage::default();
        for pid in included {
            if let Some(process) = self.processes.get(&pid) {
                usage.cpu_percent += process.cpu_percent;
                usage.memory_bytes = usage.memory_bytes.saturating_add(process.memory_bytes);
            }
        }
        usage.cpu_percent = rounded_cpu(usage.cpu_percent);
        usage
    }
}

#[derive(Debug)]
pub struct ResourceSampler {
    previous_ticks: HashMap<u32, u64>,
    previous_sample: Option<Instant>,
    clock_ticks_per_second: f64,
}

impl Default for ResourceSampler {
    fn default() -> Self {
        Self {
            previous_ticks: HashMap::new(),
            previous_sample: None,
            clock_ticks_per_second: clock_ticks_per_second(),
        }
    }
}

impl ResourceSampler {
    pub fn sample(&mut self) -> ResourceSnapshot {
        let now = Instant::now();
        let elapsed = self
            .previous_sample
            .map(|previous| now.duration_since(previous).as_secs_f64())
            .filter(|seconds| *seconds > 0.0);
        let current = read_processes();
        let mut snapshot = ResourceSnapshot::default();

        for (&pid, process) in &current {
            let cpu_percent = elapsed
                .and_then(|seconds| {
                    self.previous_ticks.get(&pid).map(|previous| {
                        process.total_ticks.saturating_sub(*previous) as f64
                            / self.clock_ticks_per_second
                            / seconds
                            * 100.0
                    })
                })
                .unwrap_or(0.0);
            snapshot.processes.insert(
                pid,
                ProcessUsage {
                    parent_pid: process.parent_pid,
                    cpu_percent: rounded_cpu(cpu_percent),
                    memory_bytes: process.memory_bytes,
                },
            );
        }
        for (&pid, process) in &snapshot.processes {
            snapshot
                .children
                .entry(process.parent_pid)
                .or_default()
                .push(pid);
        }

        self.previous_ticks = current
            .into_iter()
            .map(|(pid, process)| (pid, process.total_ticks))
            .collect();
        self.previous_sample = Some(now);
        snapshot
    }
}

fn read_processes() -> HashMap<u32, ProcessStat> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return HashMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let memory_bytes = resident_memory_bytes(&entry.path().join("status"));
            parse_process_stat(&stat, memory_bytes).map(|process| (pid, process))
        })
        .collect()
}

fn parse_process_stat(value: &str, memory_bytes: u64) -> Option<ProcessStat> {
    let command_end = value.rfind(')')?;
    let fields = value
        .get(command_end + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    Some(ProcessStat {
        parent_pid: fields.get(1)?.parse().ok()?,
        total_ticks: fields
            .get(11)?
            .parse::<u64>()
            .ok()?
            .saturating_add(fields.get(12)?.parse::<u64>().ok()?),
        memory_bytes,
    })
}

fn resident_memory_bytes(path: &std::path::Path) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("VmRSS:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
        })
        .unwrap_or(0)
        .saturating_mul(1024)
}

fn clock_ticks_per_second() -> f64 {
    // SAFETY: sysconf only reads the process-independent clock tick setting.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 { ticks as f64 } else { 100.0 }
}

fn rounded_cpu(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        (value * 10.0).round() / 10.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::{ProcessUsage, ResourceSnapshot, parse_process_stat};
    use std::collections::HashMap;

    #[test]
    fn parses_proc_stat_with_spaces_in_command() {
        let stat = "42 (application helper) S 7 0 0 0 0 0 0 0 0 0 120 30 0 0 0 0 0 0 0 0 0";
        let process = parse_process_stat(stat, 4096).expect("valid stat");
        assert_eq!(process.parent_pid, 7);
        assert_eq!(process.total_ticks, 150);
        assert_eq!(process.memory_bytes, 4096);
    }

    #[test]
    fn totals_process_trees_without_double_counting_shared_roots() {
        let processes = HashMap::from([
            (
                10,
                ProcessUsage {
                    parent_pid: 1,
                    cpu_percent: 2.0,
                    memory_bytes: 100,
                },
            ),
            (
                11,
                ProcessUsage {
                    parent_pid: 10,
                    cpu_percent: 3.5,
                    memory_bytes: 200,
                },
            ),
            (
                20,
                ProcessUsage {
                    parent_pid: 1,
                    cpu_percent: 1.0,
                    memory_bytes: 50,
                },
            ),
        ]);
        let children = HashMap::from([(1, vec![10, 20]), (10, vec![11])]);
        let snapshot = ResourceSnapshot {
            processes,
            children,
        };
        let usage = snapshot.usage_for_roots([10, 10, 11]);
        assert_eq!(usage.cpu_percent, 5.5);
        assert_eq!(usage.memory_bytes, 300);
    }
}
