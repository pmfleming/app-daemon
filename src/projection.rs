//! Stable per-metric availability for consumers. Raw acquisition metadata remains
//! available for diagnostics, but clients must not reconstruct capability rules.
use crate::model::ResourceAvailability;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MetricAvailability(pub BTreeMap<String, bool>);
impl Default for MetricAvailability {
    fn default() -> Self {
        ResourceAvailability::default().project()
    }
}
impl ResourceAvailability {
    pub(crate) fn project(&self) -> MetricAvailability {
        let groups: &[(&[&str], bool)] = &[
            (
                &[
                    "cpu_percent",
                    "cpu_percent_of_machine",
                    "process_count",
                    "thread_count",
                    "major_faults_per_second",
                ],
                self.cpu,
            ),
            (
                &[
                    "memory_bytes",
                    "memory_rss_bytes",
                    "memory_pss_bytes",
                    "memory_private_bytes",
                    "memory_swap_bytes",
                    "memory_cgroup_bytes",
                ],
                self.memory,
            ),
            (
                &[
                    "gpu_percent",
                    "gpu_busy_percent",
                    "gpu_memory_bytes",
                    "gpu_memory_resident_bytes",
                    "gpu_memory_allocated_bytes",
                ],
                self.gpu,
            ),
            (
                &[
                    "disk_read_bytes",
                    "disk_write_bytes",
                    "disk_read_bytes_per_second",
                    "disk_write_bytes_per_second",
                    "logical_read_bytes",
                    "logical_write_bytes",
                    "logical_read_bytes_per_second",
                    "logical_write_bytes_per_second",
                    "read_operations",
                    "write_operations",
                    "read_operations_per_second",
                    "write_operations_per_second",
                    "cancelled_write_bytes",
                ],
                self.storage,
            ),
            (
                &[
                    "open_file_disk_bytes",
                    "referenced_file_disk_bytes",
                    "referenced_file_temporary_bytes",
                    "referenced_file_permanent_bytes",
                ],
                self.referenced_files,
            ),
            (
                &[
                    "disk_space_total_bytes",
                    "disk_space_temporary_bytes",
                    "disk_space_permanent_bytes",
                ],
                self.disk_space,
            ),
            (
                &[
                    "network_receive_bytes",
                    "network_transmit_bytes",
                    "network_receive_bytes_per_second",
                    "network_transmit_bytes_per_second",
                ],
                self.network_bytes,
            ),
            (&["network_connection_count"], self.network_connections),
            (
                &[
                    "average_power_watts",
                    "estimated_app_power_watts",
                    "power_watts",
                    "energy_mwh",
                    "battery_percent",
                    "battery_percent_per_hour",
                    "attributed_fraction",
                ],
                self.energy,
            ),
        ];
        MetricAvailability(
            groups
                .iter()
                .flat_map(|(names, available)| {
                    names.iter().map(move |name| ((*name).into(), *available))
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capabilities_are_independent_and_unknown_metrics_are_absent() {
        let flags = ResourceAvailability {
            network_connections: true,
            memory: true,
            ..Default::default()
        }
        .project()
        .0;
        assert!(flags["network_connection_count"]);
        assert!(!flags["network_receive_bytes_per_second"]);
        assert!(flags["memory_bytes"]);
        assert!(!flags["average_power_watts"]);
        assert!(!flags.contains_key("invented_metric"));
    }
}
