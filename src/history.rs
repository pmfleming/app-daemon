use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use shelllist_daemon_core::{AtomicWritePolicy, XdgRoot, resolve_xdg_path, write_json_atomic};

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
    applications: HashMap<String, VecDeque<ResourceHistoryPoint>>,
    energy_applications: HashMap<String, VecDeque<EnergyHistoryPoint>>,
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

#[derive(Debug, Clone, Default)]
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
struct HistoryCursor {
    version: u8,
    target_id: String,
    after_timestamp_ms: u64,
    epoch: String,
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
}

#[derive(Debug)]
pub struct HistoryStore {
    path: Option<PathBuf>,
    points: HashMap<String, VecDeque<ResourceHistoryPoint>>,
    pending: HashMap<String, PendingPoint>,
    energy_points: HashMap<String, VecDeque<EnergyHistoryPoint>>,
    pending_energy: HashMap<String, PendingEnergy>,
    cursor_epochs: HashMap<String, String>,
}

impl HistoryStore {
    pub fn load_default() -> Self {
        Self::load(history_path())
    }

    pub(crate) fn load(path: Option<PathBuf>) -> Self {
        let file = path
            .as_ref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<HistoryFile>(&bytes).ok())
            .filter(|file| file.version == FILE_VERSION)
            .unwrap_or_default();
        let mut store = Self {
            path,
            points: HashMap::new(),
            pending: HashMap::new(),
            energy_points: HashMap::new(),
            pending_energy: HashMap::new(),
            cursor_epochs: file.cursor_epochs,
        };
        // Repair old files with duplicate or out-of-order buckets too.
        for (id, points) in file.applications {
            for point in points {
                store.insert_point(id.clone(), point);
            }
        }
        for (id, points) in file.energy_applications {
            for point in points {
                store.insert_energy_point(id.clone(), point);
            }
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
        let bucket_start = timestamp_ms - timestamp_ms % BUCKET_MILLISECONDS;
        if self
            .pending
            .get(target_id)
            .is_some_and(|pending| pending.timestamp_ms != bucket_start)
            && let Some(pending) = self.pending.remove(target_id)
        {
            self.finish_pending(target_id.to_owned(), pending);
        }
        let pending = self.pending.entry(target_id.to_owned()).or_default();
        pending.timestamp_ms = bucket_start;
        pending.add(duration_ms, usage);
        self.prune(timestamp_ms);
    }

    pub fn query(
        &mut self,
        target_id: &str,
        since_ms: Option<u64>,
        cursor: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<HistoryPage> {
        self.query_window(target_id, since_ms, None, cursor, limit)
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
        let window: Vec<_> = self
            .points
            .get(target_id)
            .into_iter()
            .flatten()
            .filter(|point| since_ms.is_none_or(|since| point.timestamp_ms >= since))
            // Summaries also need observations whose interval overlaps the
            // right boundary, even when their bucket endpoint is outside it.
            .filter(|point| {
                until_ms.is_none_or(|until| {
                    point.timestamp_ms <= until
                        || point.timestamp_ms.saturating_sub(point.duration_ms) < until
                })
            })
            .collect();
        let end = until_ms
            .unwrap_or_else(|| now_milliseconds().max(window.last().map_or(0, |p| p.timestamp_ms)));
        let start = since_ms.unwrap_or_else(|| {
            window
                .first()
                .map_or(end, |p| p.timestamp_ms.saturating_sub(p.duration_ms))
        });
        let summary = summary::summarize(&window, start, end, epoch);
        let mut matching = window
            .into_iter()
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
        let mut totals = HashMap::<String, PendingEnergy>::new();
        add_recorded_energy(&mut totals, &self.energy_points, since_ms, until_ms);
        add_pending_energy(&mut totals, &self.pending_energy, since_ms, until_ms);
        totals
            .into_iter()
            .filter(|(_, total)| total.energy_mwh > 0.0)
            .map(|(target_id, total)| EnergyTotal {
                target_id,
                energy_mwh: rounded(total.energy_mwh, 4),
                energy_source: available_label(total.energy_source),
                energy_confidence: available_label(total.energy_confidence),
            })
            .collect()
    }

    pub fn snapshot(&mut self, final_save: bool) -> HistorySnapshot {
        if final_save {
            self.flush_pending();
        } else {
            self.flush_expired(now_milliseconds());
        }
        self.prune(now_milliseconds());
        HistorySnapshot {
            path: self.path.clone(),
            file: HistoryFile {
                version: FILE_VERSION,
                applications: self.points.clone(),
                energy_applications: self.energy_points.clone(),
                cursor_epochs: self.cursor_epochs.clone(),
            },
        }
    }

    fn flush_pending(&mut self) {
        for (id, pending) in std::mem::take(&mut self.pending) {
            self.finish_pending(id, pending);
        }
        for (id, pending) in std::mem::take(&mut self.pending_energy) {
            self.finish_pending_energy(id, pending);
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
            self.finish_pending_energy(id, pending);
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
            self.finish_pending_energy(target_id.to_owned(), pending);
        }
        let pending = self.pending_energy.entry(target_id.to_owned()).or_default();
        pending.timestamp_ms = bucket_start;
        pending.add(
            usage.energy.energy_mwh,
            &usage.energy.energy_source,
            &usage.energy.energy_confidence,
        );
    }

    fn finish_pending_energy(&mut self, id: String, pending: PendingEnergy) {
        self.insert_energy_point(id, pending.finish());
    }

    fn insert_energy_point(&mut self, id: String, point: EnergyHistoryPoint) {
        let points = self.energy_points.entry(id).or_default();
        match points.binary_search_by_key(&point.timestamp_ms, |point| point.timestamp_ms) {
            Ok(index) => {
                let previous = &mut points[index];
                previous.energy_mwh += point.energy_mwh;
                merge_label(&mut previous.energy_source, &point.energy_source);
                merge_label(&mut previous.energy_confidence, &point.energy_confidence);
            }
            Err(index) => points.insert(index, point),
        }
    }

    fn finish_pending(&mut self, id: String, pending: PendingPoint) {
        if let Some(point) = pending.finish() {
            self.insert_point(id, point);
        }
    }

    fn insert_point(&mut self, id: String, point: ResourceHistoryPoint) {
        let points = self.points.entry(id.clone()).or_default();
        let epoch = self
            .cursor_epochs
            .entry(id)
            .or_insert_with(new_cursor_epoch);
        if points
            .back()
            .is_some_and(|last| point.timestamp_ms <= last.timestamp_ms)
        {
            // A previously returned bucket changed, or the wall clock went back.
            // Existing callers must resync; `timestamp > cursor` would lose data.
            *epoch = new_cursor_epoch();
        }
        match points.binary_search_by_key(&point.timestamp_ms, |point| point.timestamp_ms) {
            Ok(index) => points[index] = PendingPoint::merge_finished(&points[index], &point),
            Err(index) => points.insert(index, point),
        }
    }

    fn prune(&mut self, timestamp_ms: u64) {
        let cutoff = timestamp_ms.saturating_sub(RETENTION_MILLISECONDS);
        self.points.retain(|_, points| {
            while points
                .front()
                .is_some_and(|point| point.timestamp_ms < cutoff)
            {
                points.pop_front();
            }
            !points.is_empty()
        });
        let energy_cutoff = timestamp_ms.saturating_sub(ENERGY_RETENTION_MILLISECONDS);
        self.cursor_epochs
            .retain(|id, _| self.points.contains_key(id) || self.pending.contains_key(id));
        self.energy_points.retain(|_, points| {
            while points
                .front()
                .is_some_and(|point| point.timestamp_ms < energy_cutoff)
            {
                points.pop_front();
            }
            !points.is_empty()
        });
    }
}

fn add_recorded_energy(
    totals: &mut HashMap<String, PendingEnergy>,
    points: &HashMap<String, VecDeque<EnergyHistoryPoint>>,
    since_ms: u64,
    until_ms: u64,
) {
    for (target_id, points) in points {
        let total = totals.entry(target_id.clone()).or_default();
        for point in points
            .iter()
            .filter(|point| point.timestamp_ms >= since_ms && point.timestamp_ms <= until_ms)
        {
            total.add(
                point.energy_mwh,
                &point.energy_source,
                &point.energy_confidence,
            );
        }
    }
}

fn add_pending_energy(
    totals: &mut HashMap<String, PendingEnergy>,
    points: &HashMap<String, PendingEnergy>,
    since_ms: u64,
    until_ms: u64,
) {
    for (target_id, point) in points.iter().filter(|(_, point)| {
        point
            .timestamp_ms
            .saturating_add(ENERGY_BUCKET_MILLISECONDS)
            >= since_ms
            && point.timestamp_ms <= until_ms
    }) {
        totals.entry(target_id.clone()).or_default().add(
            point.energy_mwh,
            &point.energy_source,
            &point.energy_confidence,
        );
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

fn new_cursor_epoch() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn encode_cursor(target_id: &str, timestamp_ms: u64, epoch: &str) -> anyhow::Result<String> {
    let cursor = HistoryCursor {
        version: CURSOR_VERSION,
        target_id: target_id.to_owned(),
        after_timestamp_ms: timestamp_ms,
        epoch: epoch.to_owned(),
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

fn history_path() -> Option<PathBuf> {
    resolve_xdg_path(
        XdgRoot::State,
        "app-daemon",
        Path::new("resource-history-v1.json"),
    )
}

pub fn persist_snapshot(snapshot: HistorySnapshot) -> std::io::Result<()> {
    let Some(path) = snapshot.path else {
        return Ok(());
    };
    write_json_atomic(
        &path,
        &snapshot.file,
        AtomicWritePolicy {
            pretty: false,
            ..AtomicWritePolicy::PRIVATE
        },
    )
    .map_err(std::io::Error::other)
}

#[cfg(test)]
mod availability_tests;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod tests;
