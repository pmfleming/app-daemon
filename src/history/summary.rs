//! Canonical statistics over a selected window, independent of response pagination.
use crate::model::{HistoricalResourceUsage, ResourceAvailability, ResourceHistoryPoint};
pub use crate::model::{HistorySummary, MetricSummary};

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
    let mut available = availability(&point);
    available.energy &= point.resources.energy_source == "rapl";
    point.resources.metric_availability = available.project();
    point.resources.availability = Some(available);
    point
}
// Keep response keys and their observation sources together: no duplicate
// registration list or fallible per-sample map lookup is needed.
type MetricReader = fn(&HistoricalResourceUsage, &ResourceAvailability) -> (bool, f64, f64);
const METRICS: &[(&str, MetricReader)] = &[
    ("cpu_percent_of_machine", |r, a| {
        (
            a.cpu,
            r.compute.cpu_percent_of_machine,
            r.peaks.cpu_percent_of_machine,
        )
    }),
    ("memory_bytes", |r, a| {
        (
            a.memory,
            r.compute.memory_bytes as f64,
            r.peaks.memory_bytes as f64,
        )
    }),
    ("gpu_busy_percent", |r, a| {
        (a.gpu, r.compute.gpu_busy_percent, r.peaks.gpu_busy_percent)
    }),
    ("disk_read_bytes_per_second", |r, a| {
        (
            a.storage,
            r.storage.disk_read_bytes_per_second,
            r.peaks.disk_read_bytes_per_second,
        )
    }),
    ("disk_write_bytes_per_second", |r, a| {
        (
            a.storage,
            r.storage.disk_write_bytes_per_second,
            r.peaks.disk_write_bytes_per_second,
        )
    }),
    ("network_receive_bytes_per_second", |r, a| {
        (
            a.network_bytes,
            r.network.network_receive_bytes_per_second,
            r.peaks.network_receive_bytes_per_second,
        )
    }),
    ("network_transmit_bytes_per_second", |r, a| {
        (
            a.network_bytes,
            r.network.network_transmit_bytes_per_second,
            r.peaks.network_transmit_bytes_per_second,
        )
    }),
    ("average_power_watts", |r, a| {
        (
            a.energy && r.energy_source == "rapl",
            r.average_power_watts,
            r.peaks.estimated_app_power_watts,
        )
    }),
];

pub(super) fn summarize(
    points: &[&ResourceHistoryPoint],
    start: u64,
    end: u64,
    epoch: &str,
) -> HistorySummary {
    let mut metrics: [MetricSummary; METRICS.len()] =
        std::array::from_fn(|_| MetricSummary::default());
    let window_ms = end.saturating_sub(start).max(1);
    let mut confidence = None;
    for point in points {
        let weight = point.timestamp_ms.min(end).saturating_sub(
            point
                .timestamp_ms
                .saturating_sub(point.duration_ms)
                .max(start),
        );
        let available = availability(point);
        let power = point.resources.average_power_watts;
        if weight > 0
            && available.energy
            && point.resources.energy_source == "rapl"
            && power.is_finite()
            && power >= 0.0
        {
            let next = match point.resources.energy_confidence.as_str() {
                "low" => 0,
                "high" => 3,
                "medium" => 2,
                _ => 1,
            };
            confidence = Some(confidence.map_or(next, |previous: u8| previous.min(next)));
        }
        for ((_, read), stats) in METRICS.iter().zip(&mut metrics) {
            stats.observe(read(&point.resources, &available), weight, window_ms);
        }
    }
    for ((name, _), stats) in METRICS.iter().zip(&mut metrics) {
        let unit = if *name == "average_power_watts" {
            Some((3600.0, "mWh"))
        } else if name.ends_with("_bytes_per_second") {
            Some((1000.0, "bytes"))
        } else {
            None
        };
        if let (Some(mean), Some((divisor, unit))) = (stats.mean, unit) {
            let total = mean * stats.observed_ms as f64 / divisor;
            if total.is_finite() && total >= 0.0 {
                stats.observed_total = Some(total);
                stats.total_unit = Some(unit.into());
            }
        }
    }
    HistorySummary {
        energy_confidence: match confidence {
            Some(0) => "low",
            Some(2) => "medium",
            Some(3) => "high",
            _ => "unknown",
        }
        .into(),
        window_start_ms: start,
        window_end_ms: end,
        revision: format!(
            "{epoch}:{}:{}",
            points.last().map_or(0, |p| p.timestamp_ms),
            points.len()
        ),
        weighting: "observed-duration".into(),
        metrics: METRICS
            .iter()
            .zip(metrics)
            .map(|((name, _), stats)| ((*name).into(), stats))
            .collect(),
    }
}

impl crate::model::MetricSummary {
    fn observe(&mut self, (valid, value, peak): (bool, f64, f64), weight: u64, window_ms: u64) {
        if !valid || weight == 0 || !value.is_finite() || value < 0.0 {
            return;
        }
        self.observed_ms = self.observed_ms.saturating_add(weight);
        let mean = self.mean.unwrap_or(value);
        self.mean = Some(mean + (value - mean) * (weight as f64 / self.observed_ms as f64));
        let peak = if peak.is_finite() && peak >= 0.0 {
            peak
        } else {
            value
        };
        self.peak = Some(self.peak.unwrap_or(value).max(value).max(peak));
        self.available = true;
        self.coverage = (self.observed_ms as f64 / window_ms as f64).min(1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::{ResourceAvailability, ResourceHistoryPoint};
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
    fn resource_projection_fixture_is_current() {
        let mut fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../test_support/app-resource-v1.json")).unwrap();
        let original = fixture.clone();
        let current: crate::model::ResourceUsage =
            serde_json::from_value(fixture["current"].clone()).unwrap();
        fixture["current"]["metric_availability"] =
            serde_json::to_value(ResourceAvailability::for_usage(&current).project()).unwrap();
        let point =
            super::normalize(serde_json::from_value(fixture["history_point"].clone()).unwrap());
        fixture["history_point"]["metric_availability"] =
            serde_json::to_value(&point.resources.metric_availability).unwrap();
        fixture["summary"] = serde_json::to_value(super::summarize(
            &[&point],
            point.timestamp_ms - point.duration_ms,
            point.timestamp_ms,
            "fixture",
        ))
        .unwrap();
        if std::env::var_os("APP_DAEMON_UPDATE_RESOURCE_FIXTURE").is_some() {
            std::fs::write(
                "test_support/app-resource-v1.json",
                format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap()),
            )
            .unwrap();
        } else {
            assert_eq!(fixture, original);
        }
    }

    #[test]
    fn totals_and_confidence_use_only_available_clipped_intervals() {
        let mut first = point(1000, 1000, 0.0);
        first.resources.availability.as_mut().unwrap().energy = true;
        first.resources.energy_source = "rapl".into();
        first.resources.energy_confidence = "high".into();
        first.resources.average_power_watts = 3.6;
        let mut second = first.clone();
        second.timestamp_ms = 3000;
        second.resources.energy_confidence = "low".into();
        let summary = super::summarize(&[&first, &second], 500, 2500, "test");
        assert_eq!(
            summary.metrics["average_power_watts"].observed_total,
            Some(1.0)
        );
        assert_eq!(
            summary.metrics["average_power_watts"].total_unit.as_deref(),
            Some("mWh")
        );
        assert_eq!(summary.energy_confidence, "low");
        second.resources.energy_source = "battery".into();
        let summary = super::summarize(&[&first, &second], 500, 2500, "test");
        assert_eq!(
            summary.metrics["average_power_watts"].observed_total,
            Some(0.5)
        );
        assert_eq!(summary.energy_confidence, "high");
        first.resources.average_power_watts = 0.0;
        assert_eq!(
            super::summarize(&[&first], 0, 1000, "test").metrics["average_power_watts"]
                .observed_total,
            Some(0.0)
        );
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
        let cpu = &second.summary.metrics["cpu_percent_of_machine"];
        assert_eq!(cpu.mean, Some(25.0));
        assert_eq!(cpu.peak, Some(40.0));
        assert_eq!(cpu.observed_ms, 4000);
        assert_eq!(cpu.coverage, 0.8);
        assert_eq!(second.summary.metrics["gpu_busy_percent"].mean, None);
        assert!(!second.summary.metrics["gpu_busy_percent"].available);
        assert_eq!(second.summary.metrics.len(), 8);
        assert!(second.points[0].timestamp_ms > first.points[0].timestamp_ms);
        assert!(
            store
                .query("another-app", None, first.next_cursor.as_deref(), 1)
                .is_err()
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
    fn clips_both_boundaries_without_advancing_cursor_over_summary_only_bucket() {
        let base = super::super::now_milliseconds() - 60_000;
        let mut store = super::super::HistoryStore::load(None);
        store.insert_point("app".into(), point(base + 15_000, 15_000, 10.0));
        store.insert_point("app".into(), point(base + 30_000, 15_000, 30.0));
        // This point starts exactly at the selected end and contributes nothing.
        store.insert_point("app".into(), point(base + 40_000, 17_500, 90.0));
        // A right-clipped first bucket contributes even when there is no page
        // endpoint yet, with either an explicit or inferred window start.
        for since in [Some(base), None] {
            let page = store
                .query_window("app", since, Some(base + 7_500), None, 1)
                .unwrap();
            assert_eq!(page.summary.window_start_ms, base);
            assert_eq!(
                page.summary.metrics["cpu_percent_of_machine"].observed_ms,
                7_500
            );
            assert_eq!(
                page.summary.metrics["cpu_percent_of_machine"].mean,
                Some(10.0)
            );
            assert!(page.points.is_empty() && !page.has_more && page.next_cursor.is_none());
        }
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
}
