//! Canonical statistics over a selected window, independent of response pagination.
use crate::model::{ResourceAvailability, ResourceHistoryPoint};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricSummary {
    pub available: bool,
    pub mean: Option<f64>,
    pub peak: Option<f64>,
    pub observed_ms: u64,
    pub coverage: f64,
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HistorySummary {
    pub window_start_ms: u64,
    pub window_end_ms: u64,
    pub revision: String,
    pub weighting: String,
    pub metrics: BTreeMap<String, MetricSummary>,
}
pub(super) fn availability(point: &ResourceHistoryPoint) -> ResourceAvailability {
    point
        .resources
        .availability
        .clone()
        .unwrap_or_else(|| ResourceAvailability {
            cpu: point.resources.coverage.is_finite() && point.resources.coverage > 0.0,
            memory: point.resources.coverage.is_finite() && point.resources.coverage > 0.0,
            energy: point.resources.energy_source == "rapl",
            ..Default::default()
        })
}
pub(super) fn normalize(mut point: ResourceHistoryPoint) -> ResourceHistoryPoint {
    point.resources.availability = Some(availability(&point));
    point
}
pub(super) fn summarize(
    points: &[&ResourceHistoryPoint],
    start: u64,
    end: u64,
    epoch: &str,
) -> HistorySummary {
    let mut result = HistorySummary {
        window_start_ms: start,
        window_end_ms: end,
        revision: format!(
            "{epoch}:{}:{}",
            points.last().map_or(0, |p| p.timestamp_ms),
            points.len()
        ),
        weighting: "observed-duration".into(),
        ..Default::default()
    };
    for metric in [
        "cpu_percent_of_machine",
        "memory_bytes",
        "gpu_busy_percent",
        "disk_read_bytes_per_second",
        "disk_write_bytes_per_second",
        "network_receive_bytes_per_second",
        "network_transmit_bytes_per_second",
        "average_power_watts",
    ] {
        result
            .metrics
            .insert(metric.into(), MetricSummary::default());
    }
    for point in points {
        let weight = point.timestamp_ms.min(end).saturating_sub(
            point
                .timestamp_ms
                .saturating_sub(point.duration_ms)
                .max(start),
        );
        if weight == 0 {
            continue;
        }
        let r = &point.resources;
        let a = availability(point);
        for (metric, valid, value, peak) in [
            (
                "cpu_percent_of_machine",
                a.cpu,
                r.compute.cpu_percent_of_machine,
                r.peaks.cpu_percent_of_machine,
            ),
            (
                "memory_bytes",
                a.memory,
                r.compute.memory_bytes as f64,
                r.peaks.memory_bytes as f64,
            ),
            (
                "gpu_busy_percent",
                a.gpu,
                r.compute.gpu_busy_percent,
                r.peaks.gpu_busy_percent,
            ),
            (
                "disk_read_bytes_per_second",
                a.storage,
                r.storage.disk_read_bytes_per_second,
                r.peaks.disk_read_bytes_per_second,
            ),
            (
                "disk_write_bytes_per_second",
                a.storage,
                r.storage.disk_write_bytes_per_second,
                r.peaks.disk_write_bytes_per_second,
            ),
            (
                "network_receive_bytes_per_second",
                a.network_bytes,
                r.network.network_receive_bytes_per_second,
                r.peaks.network_receive_bytes_per_second,
            ),
            (
                "network_transmit_bytes_per_second",
                a.network_bytes,
                r.network.network_transmit_bytes_per_second,
                r.peaks.network_transmit_bytes_per_second,
            ),
            (
                "average_power_watts",
                a.energy,
                r.average_power_watts,
                r.peaks.estimated_app_power_watts,
            ),
        ] {
            if !valid || !value.is_finite() || value < 0.0 {
                continue;
            }
            let stats = result.metrics.get_mut(metric).expect("registered metric");
            let observed = stats.observed_ms.saturating_add(weight);
            let mean = stats.mean.unwrap_or(value);
            stats.mean = Some(mean + (value - mean) * (weight as f64 / observed as f64));
            stats.peak = Some(stats.peak.unwrap_or(value).max(value).max(
                if peak.is_finite() && peak >= 0.0 {
                    peak
                } else {
                    value
                },
            ));
            stats.observed_ms = observed;
            stats.available = true;
            stats.coverage = (observed as f64 / end.saturating_sub(start).max(1) as f64).min(1.0);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HistoricalResourceUsage, ResourcePeaks};
    fn point(timestamp: u64, duration: u64, cpu: f64) -> ResourceHistoryPoint {
        let mut resources = HistoricalResourceUsage {
            availability: Some(ResourceAvailability {
                cpu: true,
                ..Default::default()
            }),
            peaks: ResourcePeaks {
                cpu_percent_of_machine: cpu + 10.0,
                ..Default::default()
            },
            ..Default::default()
        };
        resources.compute.cpu_percent_of_machine = cpu;
        ResourceHistoryPoint {
            timestamp_ms: timestamp,
            duration_ms: duration,
            resources,
        }
    }
    #[test]
    fn weights_observation_time_and_preserves_peaks_and_missing_capabilities() {
        let first = point(1000, 1000, 10.0);
        let second = point(4000, 3000, 30.0);
        let summary = summarize(&[&first, &second], 0, 5000, "epoch");
        let cpu = &summary.metrics["cpu_percent_of_machine"];
        assert_eq!(cpu.mean, Some(25.0));
        assert_eq!(cpu.peak, Some(40.0));
        assert_eq!(cpu.observed_ms, 4000);
        assert_eq!(cpu.coverage, 0.8);
        assert_eq!(summary.metrics["gpu_busy_percent"].mean, None);
        assert!(!summary.metrics["gpu_busy_percent"].available);
        let clipped = summarize(&[&first, &second], 500, 4000, "epoch");
        assert_eq!(clipped.metrics["cpu_percent_of_machine"].observed_ms, 3500);
    }
    #[test]
    fn pagination_and_incremental_reads_share_full_window_statistics() {
        let base = super::super::now_milliseconds() - 60_000;
        let mut store = super::super::HistoryStore::load(None);
        store.insert_point("app".into(), point(base + 1000, 1000, 10.0));
        store.insert_point("app".into(), point(base + 4000, 3000, 30.0));
        store.insert_point("app".into(), point(base + 6000, 1000, 99.0));
        let first = store
            .query_window("app", Some(base), Some(base + 5000), None, 1)
            .unwrap();
        let second = store
            .query_window(
                "app",
                Some(base),
                Some(base + 5000),
                first.next_cursor.as_deref(),
                1,
            )
            .unwrap();
        assert!(first.has_more);
        assert!(!second.has_more);
        assert_eq!(first.summary, second.summary);
        assert_eq!(
            second.summary.metrics["cpu_percent_of_machine"].mean,
            Some(25.0)
        );
        assert!(second.points[0].resources.availability.is_some());
        let empty_page = store
            .query_window(
                "app",
                Some(base),
                Some(base + 5000),
                second.next_cursor.as_deref(),
                1,
            )
            .unwrap();
        assert!(empty_page.points.is_empty());
        assert_eq!(empty_page.summary, second.summary);
        assert!(
            store
                .query_window("app", Some(base + 5000), Some(base), None, 1)
                .is_err()
        );
    }

    #[test]
    fn window_ending_inside_first_bucket_still_has_observed_statistics() {
        let base = super::super::now_milliseconds() - 60_000;
        let mut store = super::super::HistoryStore::load(None);
        store.insert_point("app".into(), point(base + 15_000, 15_000, 10.0));
        for since in [Some(base), None] {
            let page = store
                .query_window("app", since, Some(base + 7_500), None, 1)
                .unwrap();
            let cpu = &page.summary.metrics["cpu_percent_of_machine"];
            assert_eq!(page.summary.window_start_ms, base);
            assert_eq!(cpu.observed_ms, 7_500);
            assert_eq!(cpu.mean, Some(10.0));
            assert_eq!(cpu.peak, Some(20.0));
            assert_eq!(cpu.coverage, 1.0);
            assert!(
                page.points.is_empty(),
                "no bucket endpoint is in the window"
            );
            assert!(!page.has_more);
            assert!(page.next_cursor.is_none());
        }
    }

    #[test]
    fn clips_both_boundaries_without_advancing_cursor_over_summary_only_bucket() {
        let base = super::super::now_milliseconds() - 60_000;
        let mut store = super::super::HistoryStore::load(None);
        store.insert_point("app".into(), point(base + 15_000, 15_000, 10.0));
        store.insert_point("app".into(), point(base + 30_000, 15_000, 30.0));
        // This point starts exactly at the selected end and contributes nothing.
        store.insert_point("app".into(), point(base + 40_000, 17_500, 90.0));
        let first = store
            .query_window("app", Some(base + 7_500), Some(base + 22_500), None, 1)
            .unwrap();
        let cpu = &first.summary.metrics["cpu_percent_of_machine"];
        assert_eq!(cpu.observed_ms, 15_000);
        assert_eq!(cpu.mean, Some(20.0));
        assert_eq!(cpu.peak, Some(40.0));
        assert_eq!(cpu.coverage, 1.0);
        assert_eq!(first.points.len(), 1);
        assert_eq!(first.points[0].timestamp_ms, base + 15_000);
        assert!(!first.has_more);
        let cursor = first.next_cursor.as_deref().unwrap();
        let empty = store
            .query_window(
                "app",
                Some(base + 7_500),
                Some(base + 22_500),
                Some(cursor),
                1,
            )
            .unwrap();
        assert!(empty.points.is_empty());
        assert_eq!(empty.next_cursor, first.next_cursor);
        assert_eq!(empty.summary, first.summary);
        let extended = store
            .query_window(
                "app",
                Some(base + 7_500),
                Some(base + 30_000),
                Some(cursor),
                1,
            )
            .unwrap();
        assert_eq!(extended.points.len(), 1);
        assert_eq!(extended.points[0].timestamp_ms, base + 30_000);

        let zero_width = store
            .query_window("app", Some(base + 7_500), Some(base + 7_500), None, 1)
            .unwrap();
        assert_eq!(
            zero_width.summary.metrics["cpu_percent_of_machine"].mean,
            None
        );
        assert_eq!(
            zero_width.summary.metrics["cpu_percent_of_machine"].observed_ms,
            0
        );
    }

    #[test]
    fn measured_zero_legacy_normalization_and_invalid_values() {
        let mut zero = point(1000, 1000, 0.0);
        zero.resources.peaks.cpu_percent_of_machine = 0.0;
        assert_eq!(
            summarize(&[&zero], 0, 1000, "").metrics["cpu_percent_of_machine"].mean,
            Some(0.0)
        );
        zero.resources.availability = None;
        zero.resources.coverage = 1.0;
        zero.resources.energy_source = "rapl".into();
        let normalized = normalize(zero.clone());
        assert!(normalized.resources.availability.unwrap().energy);
        zero.resources.compute.cpu_percent_of_machine = f64::NAN;
        assert_eq!(
            summarize(&[&zero], 0, 1000, "").metrics["cpu_percent_of_machine"].mean,
            None
        );
    }
}
