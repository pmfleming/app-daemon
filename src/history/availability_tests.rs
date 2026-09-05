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

#[test]
fn legacy_history_has_unknown_capabilities() {
    let point: ResourceHistoryPoint = serde_json::from_str(
        r#"{"timestamp_ms":15000,"duration_ms":2000,"gpu_percent":0}"#
    ).unwrap();
    assert!(point.resources.availability.is_none());
}
