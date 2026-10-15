//! Canonical statistics over a selected window, independent of response pagination.
use crate::model::{HistoricalResourceUsage, ResourceAvailability, ResourceHistoryPoint};
pub use crate::model::{HistorySummary, MetricSummary};

pub(super) fn availability(point: &ResourceHistoryPoint) -> ResourceAvailability {
    point
        .resources
        .availability
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
        if let Some(next) = energy_confidence(&point.resources, &available, weight) {
            confidence = Some(confidence.map_or(next, |previous: u8| previous.min(next)));
        }
        for ((_, read), stats) in METRICS.iter().zip(&mut metrics) {
            stats.observe(read(&point.resources, &available), weight, window_ms);
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
            .map(|((name, _), stats)| ((*name).into(), stats.with_total(name)))
            .collect(),
    }
}

fn observable(available: bool, value: f64, weight: u64) -> bool {
    available && weight > 0 && value.is_finite() && value >= 0.0
}

fn energy_confidence(
    r: &HistoricalResourceUsage,
    a: &ResourceAvailability,
    weight: u64,
) -> Option<u8> {
    observable(
        a.energy && r.energy_source == "rapl",
        r.average_power_watts,
        weight,
    )
    .then_some(match r.energy_confidence.as_str() {
        "low" => 0,
        "medium" => 2,
        "high" => 3,
        _ => 1,
    })
}

impl crate::model::MetricSummary {
    fn with_total(mut self, name: &str) -> Self {
        let (divisor, unit) = match name {
            "average_power_watts" => (3600.0, "mWh"),
            name if name.ends_with("_bytes_per_second") => (1000.0, "bytes"),
            _ => return self,
        };
        if let Some(total) = self
            .mean
            .map(|mean| mean * self.observed_ms as f64 / divisor)
            .filter(|total| total.is_finite() && *total >= 0.0)
        {
            self.observed_total = Some(total);
            self.total_unit = Some(unit.into());
        }
        self
    }

    fn observe(&mut self, (valid, value, peak): (bool, f64, f64), weight: u64, window_ms: u64) {
        if !observable(valid, value, weight) {
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
        fn shape(value: serde_json::Value) -> serde_json::Value {
            match value {
                serde_json::Value::Object(fields) => fields
                    .into_iter()
                    .map(|(key, value)| (key, shape(value)))
                    .collect(),
                _ => serde_json::Value::Null,
            }
        }
        let mut fixture = crate::protocol::resource_contract_fixture().unwrap();
        let original = fixture.clone();
        let current: crate::model::ResourceUsage =
            serde_json::from_value(fixture["current"].clone()).unwrap();
        // JSON's integer/float spellings are interchangeable; field paths are not.
        assert_eq!(
            shape(serde_json::to_value(&current).unwrap()),
            shape(fixture["current"].clone())
        );
        fixture["current"]["metric_availability"] =
            serde_json::to_value(ResourceAvailability::for_usage(&current).project()).unwrap();
        let point =
            super::normalize(serde_json::from_value(fixture["history_point"].clone()).unwrap());
        assert_eq!(
            shape(serde_json::to_value(&point).unwrap()),
            shape(fixture["history_point"].clone())
        );
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
        for power in [f64::NAN, f64::INFINITY, -1.0] {
            first.resources.average_power_watts = power;
            let summary = super::summarize(&[&first], 0, 1000, "test");
            assert_eq!(summary.energy_confidence, "unknown");
            assert_eq!(summary.metrics["average_power_watts"].observed_total, None);
        }
        first.resources.average_power_watts = 0.0;
        assert_eq!(
            super::summarize(&[&first], 0, 1000, "test").metrics["average_power_watts"]
                .observed_total,
            Some(0.0)
        );
    }

    #[test]
    fn window_statistics_ignore_pagination_and_clip_without_skipping_summary_only_buckets() {
        let base = super::super::now_milliseconds() - 60_000;
        let mut store = super::super::HistoryStore::load(None);
        store.insert_point("app".into(), point(base + 15_000, 15_000, 10.0));
        store.insert_point("app".into(), point(base + 30_000, 15_000, 30.0));
        // This point starts at the full window's end and must contribute nothing.
        store.insert_point("app".into(), point(base + 40_000, 10_000, 90.0));
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
        let full = store
            .query_window("app", Some(base + 7_500), Some(base + 30_000), None, 1)
            .unwrap();
        assert!(full.has_more);
        assert_eq!(full.summary, extended.summary);
        assert!(!extended.has_more);
        let cpu = &full.summary.metrics["cpu_percent_of_machine"];
        assert!((cpu.mean.unwrap() - 70.0 / 3.0).abs() < 1e-10);
        assert_eq!(cpu.peak, Some(40.0));
        assert_eq!(cpu.observed_ms, 22_500);
        assert_eq!(full.summary.metrics["gpu_busy_percent"].mean, None);
        assert!(!full.summary.metrics["gpu_busy_percent"].available);
        assert!(store.query("another-app", None, Some(cursor), 1).is_err());
        assert!(
            store
                .query_window("app", Some(base + 30_000), Some(base), None, 1)
                .is_err()
        );

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
