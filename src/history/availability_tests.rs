use super::aggregate::PendingPoint;
use crate::model::{ResourceHistoryPoint, ResourceMeasurement, ResourceUsage};

#[test]
fn persists_idle_capabilities_and_marks_mixed_availability_unavailable() {
    let mut usage = ResourceUsage {
        measurement: ResourceMeasurement {
            coverage: 1.0,
            memory_source: "pss".into(),
            gpu_available: true,
            storage_available: true,
            referenced_files_available: true,
            network_bytes_available: true,
            network_connections_available: true,
            disk_space_scope: "identified-app-directories".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    usage.energy.energy_source = "rapl".into();
    let mut pending = PendingPoint::default();
    pending.add(2000, &usage);
    let point = pending.finish().unwrap();
    let bytes = serde_json::to_vec(&point).unwrap();
    let restored: ResourceHistoryPoint = serde_json::from_slice(&bytes).unwrap();
    let summary = super::summary::summarize(&[&restored], 0, restored.timestamp_ms, "");
    assert_eq!(summary.metrics["cpu_percent_of_machine"].mean, Some(0.0));
    assert_eq!(summary.metrics["gpu_busy_percent"].mean, Some(0.0));
    let available = restored.resources.availability.unwrap();
    assert!(available.gpu && available.storage && available.network_bytes && available.energy);
    assert_eq!(restored.resources.compute.gpu_busy_percent, 0.0);

    let mut pending = PendingPoint::default();
    pending.add(2000, &usage);
    usage.measurement.gpu_available = false;
    usage.measurement.network_bytes_available = false;
    pending.add(2000, &usage);
    let available = pending.finish().unwrap().resources.availability.unwrap();
    assert!(!available.gpu && !available.network_bytes);
    assert!(available.storage && available.memory);
}
