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

#[cfg(test)]
pub(super) fn summarize<'a>(
    points: impl IntoIterator<Item = &'a ResourceHistoryPoint>,
    start: u64,
    end: u64,
    epoch: &str,
) -> HistorySummary {
    let mut metrics: [MetricSummary; METRICS.len()] =
        std::array::from_fn(|_| MetricSummary::default());
    let window_ms = end.saturating_sub(start).max(1);
    let mut confidence = None;
    let (mut count, mut last) = (0, 0);
    for point in points {
        count += 1;
        last = point.timestamp_ms;
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
        revision: format!("{epoch}:{last}:{count}"),
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

    #[cfg(test)]
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

// Materialize only the eight summary metrics, alongside the opaque full point.
// SQL computes statistics without decoding every resource payload on each page.
pub(super) fn sql_column_names() -> Vec<String> {
    (0..METRICS.len())
        .flat_map(|i| [format!("m{i}_value"), format!("m{i}_peak")])
        .collect()
}

pub(super) fn sql_columns() -> String {
    sql_column_names()
        .into_iter()
        .map(|name| format!("{name} REAL"))
        .collect::<Vec<_>>()
        .join(",")
}

pub(super) fn sql_values(point: &ResourceHistoryPoint) -> Vec<turso::Value> {
    let available = availability(point);
    let mut values = vec![
        energy_confidence(&point.resources, &available, 1).map_or(turso::Value::Null, |value| {
            turso::Value::Integer(i64::from(value))
        }),
    ];
    for (_, read) in METRICS {
        let (valid, value, peak) = read(&point.resources, &available);
        if observable(valid, value, 1) {
            values.push(turso::Value::Real(value));
            values.push(turso::Value::Real(if peak.is_finite() && peak >= 0.0 {
                peak.max(value)
            } else {
                value
            }));
        } else {
            values.extend([turso::Value::Null, turso::Value::Null]);
        }
    }
    values
}

pub(super) async fn sql_summary(
    connection: &turso::Connection,
    target: &str,
    lower: u64,
    start: u64,
    end: u64,
    epoch: &str,
) -> anyhow::Result<HistorySummary> {
    use anyhow::Context;
    let aggregates = (0..METRICS.len())
        .map(|i| {
            format!(
                "COALESCE(SUM(CASE WHEN m{i}_value IS NOT NULL THEN weight ELSE 0 END),0), \
         SUM(m{i}_value * weight), MAX(CASE WHEN weight > 0 THEN m{i}_peak END)"
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let row = connection.query(format!(
        "SELECT COUNT(*), COALESCE(MAX(timestamp_ms),0), MIN(CASE WHEN weight > 0 THEN energy_confidence END), {aggregates} \
         FROM (SELECT *, MAX(0, MIN(timestamp_ms,?3) - MAX(start_ms,?2)) AS weight \
         FROM resource_points WHERE target_id=?1 AND timestamp_ms>=?4 AND (timestamp_ms<=?3 OR start_ms<?3))"
    ), turso::params![target, super::storage::timestamp(start)?, super::storage::timestamp(end)?, super::storage::timestamp(lower)?])
        .await?.next().await?.context("missing history summary")?;
    let count: i64 = row.get(0)?;
    let last: i64 = row.get(1)?;
    let confidence: Option<i64> = row.get(2)?;
    let window_ms = end.saturating_sub(start).max(1);
    let mut metrics = std::collections::HashMap::new();
    for (i, (name, _)) in METRICS.iter().enumerate() {
        let observed: i64 = row.get(3 + i * 3)?;
        let total: Option<f64> = row.get(4 + i * 3)?;
        let peak: Option<f64> = row.get(5 + i * 3)?;
        let stats = MetricSummary {
            available: observed > 0,
            mean: if observed > 0 {
                total.map(|sum| sum / observed as f64)
            } else {
                None
            },
            peak,
            observed_ms: observed.try_into()?,
            coverage: (observed as f64 / window_ms as f64).min(1.0),
            ..Default::default()
        };
        metrics.insert((*name).to_owned(), stats.with_total(name));
    }
    Ok(HistorySummary {
        window_start_ms: start,
        window_end_ms: end,
        revision: format!("{epoch}:{last}:{count}"),
        weighting: "observed-duration".into(),
        energy_confidence: match confidence {
            Some(0) => "low",
            Some(2) => "medium",
            Some(3) => "high",
            _ => "unknown",
        }
        .into(),
        metrics: metrics.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::{ResourceAvailability, ResourceHistoryPoint};
    use crate::model::{HistoricalResourceUsage, ResourcePeaks};
    #[test]
    fn totals_and_confidence_use_only_available_clipped_intervals() {
        let mut first = ResourceHistoryPoint {
            timestamp_ms: 1000,
            duration_ms: 1000,
            resources: HistoricalResourceUsage {
                availability: Some(ResourceAvailability {
                    cpu: true,
                    energy: true,
                    ..Default::default()
                }),
                peaks: ResourcePeaks {
                    cpu_percent_of_machine: 10.0,
                    ..Default::default()
                },
                energy_source: "rapl".into(),
                energy_confidence: "high".into(),
                average_power_watts: 3.6,
                ..Default::default()
            },
        };
        let mut second = first.clone();
        second.timestamp_ms = 3000;
        second.resources.energy_confidence = "low".into();
        let summary = super::summarize([&first, &second], 500, 2500, "test");
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
        let summary = super::summarize([&first, &second], 500, 2500, "test");
        assert_eq!(
            summary.metrics["average_power_watts"].observed_total,
            Some(0.5)
        );
        assert_eq!(summary.energy_confidence, "high");
        for power in [f64::NAN, f64::INFINITY, -1.0] {
            first.resources.average_power_watts = power;
            let summary = super::summarize([&first], 0, 1000, "test");
            assert_eq!(summary.energy_confidence, "unknown");
            assert_eq!(summary.metrics["average_power_watts"].observed_total, None);
        }
        first.resources.average_power_watts = 0.0;
        assert_eq!(
            super::summarize([&first], 0, 1000, "test").metrics["average_power_watts"]
                .observed_total,
            Some(0.0)
        );
    }
}
