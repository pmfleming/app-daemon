use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use shelllist_daemon_core::{
    AtomicWritePolicy, XdgRoot, read_json, resolve_xdg_path, write_json_atomic,
};

use crate::{
    metrics::{available_label, merge_label, rounded},
    model::{ResourceHistoryPoint, ResourceUsage},
};

mod aggregate;
pub mod summary;
use aggregate::PendingPoint;

const FILE_VERSION: u8 = 1;
const CURSOR_VERSION: u8 = 2;
const BUCKET_MILLISECONDS: u64 = 15_000;
const RETENTION_MILLISECONDS: u64 = 24 * 60 * 60 * 1000;
const ENERGY_BUCKET_MILLISECONDS: u64 = 60_000;
const ENERGY_RETENTION_MILLISECONDS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct HistoryFile {
    version: u8,
    applications: HashMap<String, VecDeque<Arc<ResourceHistoryPoint>>>,
    energy_applications: HashMap<String, VecDeque<Arc<EnergyHistoryPoint>>>,
    /// Rewrites/backfills invalidate timestamp cursors rather than silently hiding data.
    cursor_epochs: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct EnergyHistoryPoint {
    timestamp_ms: u64,
    energy_mwh: f64,
    energy_source: String,
    energy_confidence: String,
}

#[derive(Debug, Default)]
struct PendingEnergy {
    timestamp_ms: u64,
    energy_mwh: f64,
    energy_source: String,
    energy_confidence: String,
}

#[derive(Debug, Clone)]
pub struct EnergyTotal {
    pub target_id: String,
    pub energy_mwh: f64,
    pub energy_source: String,
    pub energy_confidence: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct HistoryCursor<Text = String> {
    version: u8,
    target_id: Text,
    after_timestamp_ms: u64,
    epoch: Text,
}

#[derive(Debug)]
pub struct HistoryPage {
    pub points: Vec<ResourceHistoryPoint>,
    pub summary: summary::HistorySummary,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

pub struct HistorySnapshot {
    path: Option<PathBuf>,
    file: HistoryFile,
    generation: u64,
    saved_generation: Arc<AtomicU64>,
}

#[derive(Debug)]
pub struct HistoryStore {
    path: Option<PathBuf>,
    generation: u64,
    saved_generation: Arc<AtomicU64>,
    points: HashMap<String, VecDeque<Arc<ResourceHistoryPoint>>>,
    pending: HashMap<String, PendingPoint>,
    energy_points: HashMap<String, VecDeque<Arc<EnergyHistoryPoint>>>,
    pending_energy: HashMap<String, PendingEnergy>,
    cursor_epochs: HashMap<String, String>,
}

impl HistoryStore {
    pub fn load_default() -> Self {
        Self::load(resolve_xdg_path(
            XdgRoot::State,
            "app-daemon",
            Path::new("resource-history-v1.json"),
        ))
    }

    pub(crate) fn load(path: Option<PathBuf>) -> Self {
        let file = path
            .as_ref()
            .and_then(|path| read_json::<HistoryFile>(path).ok().flatten())
            .filter(|file| file.version == FILE_VERSION)
            .unwrap_or_default();
        let mut store = Self {
            path,
            // Rewrite once after load to persist repairs and compact legacy data.
            generation: 1,
            saved_generation: Arc::new(AtomicU64::new(0)),
            points: HashMap::new(),
            pending: HashMap::new(),
            energy_points: HashMap::new(),
            pending_energy: HashMap::new(),
            cursor_epochs: file.cursor_epochs,
        };
        // Repair old files with duplicate or out-of-order buckets too.
        for (id, points) in file.applications {
            store.insert_points(id, points.into_iter().map(Arc::unwrap_or_clone));
        }
        for (id, points) in file.energy_applications {
            store.insert_energy_points(id, points.into_iter().map(Arc::unwrap_or_clone));
        }
        store.prune(now_milliseconds());
        store
    }

    pub fn record(
        &mut self,
        target_id: &str,
        timestamp_ms: u64,
        duration_seconds: f64,
        usage: &ResourceUsage,
    ) {
        if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
            return;
        }
        self.flush_expired(timestamp_ms);
        self.record_energy(target_id, timestamp_ms, usage);
        let duration_ms = (duration_seconds * 1000.0).round().max(1.0) as u64;
        let sample_start = timestamp_ms.saturating_sub(duration_ms);
        // Only retain intervals that can still be queried. This also bounds the
        // work for a very long sample after suspend or a clock discontinuity.
        let mut start = sample_start.max(timestamp_ms.saturating_sub(RETENTION_MILLISECONDS));
        while start < timestamp_ms {
            let bucket_start = start - start % BUCKET_MILLISECONDS;
            let end = timestamp_ms.min(bucket_start.saturating_add(BUCKET_MILLISECONDS));
            if self.pending.get(target_id).is_some_and(|pending| {
                pending.timestamp_ms != bucket_start || pending.end_ms != start
            }) && let Some(pending) = self.pending.remove(target_id)
            {
                self.finish_pending(target_id.to_owned(), pending);
            }
            let pending = self.pending.entry(target_id.to_owned()).or_default();
            pending.timestamp_ms = bucket_start;
            pending.add(usage, start - sample_start..end - sample_start, duration_ms);
            pending.end_ms = end;
            start = end;
        }
        self.prune(timestamp_ms);
    }

    pub fn query_window(
        &mut self,
        target_id: &str,
        since_ms: Option<u64>,
        until_ms: Option<u64>,
        cursor: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<HistoryPage> {
        anyhow::ensure!(
            since_ms
                .zip(until_ms)
                .is_none_or(|(start, end)| start <= end),
            "history window start exceeds its end"
        );
        self.flush_expired(now_milliseconds());
        self.prune(now_milliseconds());
        let epoch = self.cursor_epochs.get(target_id).map_or("", String::as_str);
        let after = cursor
            .map(|value| decode_cursor(value, target_id, epoch))
            .transpose()?
            .map(|cursor| cursor.after_timestamp_ms);
        if let Some(after) = after {
            // Rewrite epochs stay stable during ordinary retention pruning.
            // Validate the cursor's anchor against all retained points (not
            // just the requested window), so expired positions require resync
            // without invalidating newer cursors for the same application.
            anyhow::ensure!(
                self.points.get(target_id).is_some_and(|points| {
                    points
                        .binary_search_by_key(&after, |point| point.timestamp_ms)
                        .is_ok()
                }),
                "history cursor expired; restart pagination without a cursor"
            );
        }
        let limit = limit.clamp(1, 10_000);
        // Recreate borrowed iterators instead of collecting the entire selected window.
        let window = || window_points(self.points.get(target_id), since_ms, until_ms);
        let end = until_ms.unwrap_or_else(|| {
            now_milliseconds().max(window().next_back().map_or(0, |p| p.timestamp_ms))
        });
        let start = since_ms.unwrap_or_else(|| {
            window()
                .next()
                .map_or(end, |p| p.timestamp_ms.saturating_sub(p.duration_ms))
        });
        let summary = summary::summarize(window(), start, end, epoch);
        let mut matching = window()
            // Keep endpoint-based pagination: a summary-only overlapping
            // bucket must not advance the cursor past the selected window.
            .filter(|point| until_ms.is_none_or(|until| point.timestamp_ms <= until))
            .filter(|point| after.is_none_or(|timestamp| point.timestamp_ms > timestamp));
        let points = matching
            .by_ref()
            .take(limit)
            .cloned()
            .map(summary::normalize)
            .collect::<Vec<_>>();
        let has_more = matching.next().is_some();
        let next_cursor = points
            .last()
            .map(|point| encode_cursor(target_id, point.timestamp_ms, epoch))
            .transpose()?
            .or_else(|| cursor.map(str::to_owned));
        Ok(HistoryPage {
            points,
            summary,
            has_more,
            next_cursor,
        })
    }

    pub fn energy_totals(&mut self, since_ms: u64, until_ms: u64) -> Vec<EnergyTotal> {
        self.flush_expired(until_ms);
        self.energy_points
            .keys()
            .chain(
                self.pending_energy
                    .keys()
                    .filter(|id| !self.energy_points.contains_key(*id)),
            )
            .filter_map(|id| {
                let total = self.energy_total(id, since_ms, until_ms);
                (total.energy_mwh > 0.0).then(|| EnergyTotal {
                    target_id: id.clone(),
                    energy_mwh: rounded(total.energy_mwh, 4),
                    energy_source: available_label(total.energy_source),
                    energy_confidence: available_label(total.energy_confidence),
                })
            })
            .collect()
    }

    fn energy_total(&self, target_id: &str, since_ms: u64, until_ms: u64) -> PendingEnergy {
        let mut total = PendingEnergy::default();
        for point in self
            .energy_points
            .get(target_id)
            .into_iter()
            .flatten()
            .filter(|point| point.timestamp_ms >= since_ms && point.timestamp_ms <= until_ms)
        {
            total.add(
                point.energy_mwh,
                &point.energy_source,
                &point.energy_confidence,
            );
        }
        // Pending buckets use their interval; finished buckets use their endpoint.
        if let Some(point) = self.pending_energy.get(target_id).filter(|point| {
            point
                .timestamp_ms
                .saturating_add(ENERGY_BUCKET_MILLISECONDS)
                >= since_ms
                && point.timestamp_ms <= until_ms
        }) {
            total.add(
                point.energy_mwh,
                &point.energy_source,
                &point.energy_confidence,
            );
        }
        total
    }

    pub fn snapshot(&mut self, final_save: bool) -> Option<HistorySnapshot> {
        if final_save {
            self.flush_pending();
        } else {
            self.flush_expired(now_milliseconds());
        }
        self.prune(now_milliseconds());
        if self.generation == self.saved_generation.load(Ordering::Acquire) {
            return None;
        }
        Some(HistorySnapshot {
            path: self.path.clone(),
            generation: self.generation,
            saved_generation: Arc::clone(&self.saved_generation),
            file: HistoryFile {
                version: FILE_VERSION,
                applications: self.points.clone(),
                energy_applications: self.energy_points.clone(),
                cursor_epochs: self.cursor_epochs.clone(),
            },
        })
    }

    fn flush_pending(&mut self) {
        for (id, pending) in std::mem::take(&mut self.pending) {
            self.finish_pending(id, pending);
        }
        for (id, pending) in std::mem::take(&mut self.pending_energy) {
            self.insert_energy_points(id, [pending.finish()]);
        }
    }

    fn flush_expired(&mut self, timestamp_ms: u64) {
        let expired = self
            .pending
            .extract_if(|_, pending| {
                pending.timestamp_ms.saturating_add(BUCKET_MILLISECONDS) <= timestamp_ms
            })
            .collect::<Vec<_>>();
        for (id, pending) in expired {
            self.finish_pending(id, pending);
        }
        let expired_energy = self
            .pending_energy
            .extract_if(|_, pending| {
                pending
                    .timestamp_ms
                    .saturating_add(ENERGY_BUCKET_MILLISECONDS)
                    <= timestamp_ms
            })
            .collect::<Vec<_>>();
        for (id, pending) in expired_energy {
            self.insert_energy_points(id, [pending.finish()]);
        }
    }

    fn record_energy(&mut self, target_id: &str, timestamp_ms: u64, usage: &ResourceUsage) {
        let bucket_start = timestamp_ms - timestamp_ms % ENERGY_BUCKET_MILLISECONDS;
        if self
            .pending_energy
            .get(target_id)
            .is_some_and(|pending| pending.timestamp_ms != bucket_start)
            && let Some(pending) = self.pending_energy.remove(target_id)
        {
            self.insert_energy_points(target_id.to_owned(), [pending.finish()]);
        }
        let pending = self.pending_energy.entry(target_id.to_owned()).or_default();
        pending.timestamp_ms = bucket_start;
        pending.add(
            usage.energy.energy_mwh,
            &usage.energy.energy_source,
            &usage.energy.energy_confidence,
        );
    }

    fn insert_energy_points(
        &mut self,
        id: String,
        incoming: impl IntoIterator<Item = EnergyHistoryPoint>,
    ) {
        let points = self.energy_points.entry(id).or_default();
        for point in incoming {
            self.generation += 1;
            match points.binary_search_by_key(&point.timestamp_ms, |point| point.timestamp_ms) {
                Ok(index) => {
                    let previous = Arc::make_mut(&mut points[index]);
                    previous.energy_mwh += point.energy_mwh;
                    merge_label(&mut previous.energy_source, &point.energy_source);
                    merge_label(&mut previous.energy_confidence, &point.energy_confidence);
                }
                Err(index) => points.insert(index, Arc::new(point)),
            }
        }
    }

    fn finish_pending(&mut self, id: String, pending: PendingPoint) {
        if let Some(mut point) = pending.finish() {
            if let Some(points) = self.points.get_mut(&id)
                && let Ok(index) = points.binary_search_by_key(
                    &point.timestamp_ms.saturating_sub(point.duration_ms),
                    |p| p.timestamp_ms,
                )
                && aggregate::adjacent_in_bucket(&points[index], &point)
                && let Some(previous) = points.remove(index)
            {
                point = PendingPoint::merge_finished(&previous, &point);
                self.cursor_epochs.insert(id.clone(), new_cursor_epoch());
            }
            self.insert_points(id, [point]);
        }
    }

    fn insert_points(
        &mut self,
        id: String,
        incoming: impl IntoIterator<Item = ResourceHistoryPoint>,
    ) {
        let points = self.points.entry(id.clone()).or_default();
        let epoch = self
            .cursor_epochs
            .entry(id)
            .or_insert_with(new_cursor_epoch);
        for mut point in incoming {
            self.generation += 1;
            point.resources.metric_availability = crate::projection::MetricAvailability::empty();
            if points
                .back()
                .is_some_and(|last| point.timestamp_ms <= last.timestamp_ms)
            {
                // Backfills and rewrites invalidate cursors, including during recovery.
                *epoch = new_cursor_epoch();
            }
            match points.binary_search_by_key(&point.timestamp_ms, |point| point.timestamp_ms) {
                Ok(index) => {
                    points[index] = Arc::new(PendingPoint::merge_finished(&points[index], &point))
                }
                Err(index) => points.insert(index, Arc::new(point)),
            }
        }
    }

    fn prune(&mut self, timestamp_ms: u64) {
        let mut changed = false;
        let cutoff = timestamp_ms.saturating_sub(RETENTION_MILLISECONDS);
        self.points.retain(|_, points| {
            let expired = points.partition_point(|point| point.timestamp_ms < cutoff);
            changed |= expired > 0 || points.is_empty();
            points.drain(..expired);
            !points.is_empty()
        });
        let energy_cutoff = timestamp_ms.saturating_sub(ENERGY_RETENTION_MILLISECONDS);
        self.cursor_epochs.retain(|id, _| {
            let keep = self.points.contains_key(id) || self.pending.contains_key(id);
            changed |= !keep;
            keep
        });
        self.energy_points.retain(|_, points| {
            let expired = points.partition_point(|point| point.timestamp_ms < energy_cutoff);
            changed |= expired > 0 || points.is_empty();
            points.drain(..expired);
            !points.is_empty()
        });
        if changed {
            self.generation += 1;
        }
    }
}

impl PendingEnergy {
    fn add(&mut self, energy_mwh: f64, source: &str, confidence: &str) {
        if energy_mwh.is_finite() && energy_mwh > 0.0 {
            self.energy_mwh += energy_mwh;
        }
        merge_label(&mut self.energy_source, source);
        merge_label(&mut self.energy_confidence, confidence);
    }

    fn finish(self) -> EnergyHistoryPoint {
        EnergyHistoryPoint {
            timestamp_ms: self.timestamp_ms.saturating_add(ENERGY_BUCKET_MILLISECONDS),
            energy_mwh: rounded(self.energy_mwh, 4),
            energy_source: available_label(self.energy_source),
            energy_confidence: available_label(self.energy_confidence),
        }
    }
}

fn window_points(
    points: Option<&VecDeque<Arc<ResourceHistoryPoint>>>,
    since: Option<u64>,
    until: Option<u64>,
) -> impl DoubleEndedIterator<Item = &ResourceHistoryPoint> {
    points
        .into_iter()
        .flatten()
        .map(Arc::as_ref)
        .filter(move |point| since.is_none_or(|since| point.timestamp_ms >= since))
        // Include summary-only overlaps; pagination separately checks endpoints.
        .filter(move |point| {
            until.is_none_or(|until| {
                point.timestamp_ms <= until
                    || point.timestamp_ms.saturating_sub(point.duration_ms) < until
            })
        })
}

fn new_cursor_epoch() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn encode_cursor(target_id: &str, timestamp_ms: u64, epoch: &str) -> anyhow::Result<String> {
    let cursor = HistoryCursor {
        version: CURSOR_VERSION,
        target_id,
        after_timestamp_ms: timestamp_ms,
        epoch,
    };
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor)?))
}

fn decode_cursor(value: &str, target_id: &str, epoch: &str) -> anyhow::Result<HistoryCursor> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .context("history cursor is not valid base64url")?;
    let cursor: HistoryCursor =
        serde_json::from_slice(&bytes).context("history cursor is not valid JSON")?;
    anyhow::ensure!(
        cursor.version == CURSOR_VERSION,
        "history cursor version is unsupported"
    );
    anyhow::ensure!(
        cursor.target_id == target_id,
        "history cursor belongs to another target"
    );
    anyhow::ensure!(
        cursor.epoch == epoch,
        "history changed or expired; restart pagination without a cursor"
    );
    Ok(cursor)
}

pub(crate) fn merged_labels<'a>(values: impl Iterator<Item = &'a str>) -> String {
    let mut merged = String::new();
    for value in values {
        merge_label(&mut merged, value);
    }
    available_label(merged)
}

pub fn now_milliseconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub fn persist_snapshot(snapshot: HistorySnapshot) -> std::io::Result<()> {
    if let Some(path) = snapshot.path {
        write_json_atomic(
            &path,
            &snapshot.file,
            AtomicWritePolicy {
                pretty: false,
                ..AtomicWritePolicy::PRIVATE
            },
        )
        .map_err(std::io::Error::other)?;
    }
    // Writers are serialized by the service. A cancelled await must still mark
    // this exact generation saved, never changes recorded during the write.
    snapshot
        .saved_generation
        .fetch_max(snapshot.generation, Ordering::Release);
    Ok(())
}

#[cfg(test)]
mod persistence_tests;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod tests;
