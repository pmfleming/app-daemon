use std::{
    collections::{HashMap, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

use crate::model::{ResourceHistoryPoint, ResourceUsage};

mod aggregate;
use aggregate::PendingPoint;

const FILE_VERSION: u8 = 1;
const BUCKET_MILLISECONDS: u64 = 15_000;
const RETENTION_MILLISECONDS: u64 = 24 * 60 * 60 * 1000;
const ENERGY_BUCKET_MILLISECONDS: u64 = 60_000;
const ENERGY_RETENTION_MILLISECONDS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct HistoryFile {
    version: u8,
    applications: HashMap<String, Vec<ResourceHistoryPoint>>,
    energy_applications: HashMap<String, Vec<EnergyHistoryPoint>>,
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
}

#[derive(Debug)]
pub struct HistoryPage {
    pub points: Vec<ResourceHistoryPoint>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

#[derive(Debug)]
pub struct HistoryStore {
    path: Option<PathBuf>,
    points: HashMap<String, VecDeque<ResourceHistoryPoint>>,
    pending: HashMap<String, PendingPoint>,
    energy_points: HashMap<String, VecDeque<EnergyHistoryPoint>>,
    pending_energy: HashMap<String, PendingEnergy>,
}

impl HistoryStore {
    pub fn load_default() -> Self {
        Self::load(history_path())
    }

    fn load(path: Option<PathBuf>) -> Self {
        let points = path
            .as_ref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<HistoryFile>(&bytes).ok())
            .filter(|file| file.version == FILE_VERSION)
            .map(|file| {
                let points = file
                    .applications
                    .into_iter()
                    .map(|(id, points)| (id, points.into()))
                    .collect();
                let energy_points = file
                    .energy_applications
                    .into_iter()
                    .map(|(id, points)| (id, points.into()))
                    .collect();
                (points, energy_points)
            })
            .unwrap_or_default();
        let mut store = Self {
            path,
            points: points.0,
            pending: HashMap::new(),
            energy_points: points.1,
            pending_energy: HashMap::new(),
        };
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
        self.flush_expired(now_milliseconds());
        let after = cursor
            .map(|value| decode_cursor(value, target_id))
            .transpose()?
            .map(|cursor| cursor.after_timestamp_ms);
        let limit = limit.clamp(1, 10_000);
        let mut matching = self
            .points
            .get(target_id)
            .into_iter()
            .flatten()
            .filter(|point| since_ms.is_none_or(|since| point.timestamp_ms >= since))
            .filter(|point| after.is_none_or(|timestamp| point.timestamp_ms > timestamp));
        let points = matching.by_ref().take(limit).cloned().collect::<Vec<_>>();
        let has_more = matching.next().is_some();
        let next_cursor = points
            .last()
            .map(|point| encode_cursor(target_id, point.timestamp_ms))
            .transpose()?;
        Ok(HistoryPage {
            points,
            has_more,
            next_cursor,
        })
    }

    pub fn energy_totals(&mut self, since_ms: u64, until_ms: u64) -> Vec<EnergyTotal> {
        self.flush_expired(until_ms);
        let mut totals = HashMap::<String, PendingEnergy>::new();
        for (target_id, points) in &self.energy_points {
            for point in points
                .iter()
                .filter(|point| point.timestamp_ms >= since_ms && point.timestamp_ms <= until_ms)
            {
                totals.entry(target_id.clone()).or_default().add(
                    point.energy_mwh,
                    &point.energy_source,
                    &point.energy_confidence,
                );
            }
        }
        for (target_id, pending) in &self.pending_energy {
            if pending
                .timestamp_ms
                .saturating_add(ENERGY_BUCKET_MILLISECONDS)
                >= since_ms
                && pending.timestamp_ms <= until_ms
            {
                totals.entry(target_id.clone()).or_default().add(
                    pending.energy_mwh,
                    &pending.energy_source,
                    &pending.energy_confidence,
                );
            }
        }
        totals
            .into_iter()
            .filter(|(_, total)| total.energy_mwh > 0.0)
            .map(|(target_id, total)| EnergyTotal {
                target_id,
                energy_mwh: rounded_energy(total.energy_mwh),
                energy_source: available_label(total.energy_source),
                energy_confidence: available_label(total.energy_confidence),
            })
            .collect()
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        self.flush_expired(now_milliseconds());
        self.persist()
    }

    pub fn save_final(&mut self) -> std::io::Result<()> {
        self.flush_pending();
        self.persist()
    }

    fn persist(&mut self) -> std::io::Result<()> {
        self.prune(now_milliseconds());
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = HistoryFile {
            version: FILE_VERSION,
            applications: self
                .points
                .iter()
                .map(|(id, points)| (id.clone(), points.iter().cloned().collect()))
                .collect(),
            energy_applications: self
                .energy_points
                .iter()
                .map(|(id, points)| (id.clone(), points.iter().cloned().collect()))
                .collect(),
        };
        let bytes = serde_json::to_vec(&file)?;
        let temporary = temporary_path(path);
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, path)
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
            .iter()
            .filter_map(|(id, pending)| {
                (pending.timestamp_ms.saturating_add(BUCKET_MILLISECONDS) <= timestamp_ms)
                    .then_some(id.clone())
            })
            .collect::<Vec<_>>();
        for id in expired {
            if let Some(pending) = self.pending.remove(&id) {
                self.finish_pending(id, pending);
            }
        }
        let expired_energy = self
            .pending_energy
            .iter()
            .filter_map(|(id, pending)| {
                (pending
                    .timestamp_ms
                    .saturating_add(ENERGY_BUCKET_MILLISECONDS)
                    <= timestamp_ms)
                    .then_some(id.clone())
            })
            .collect::<Vec<_>>();
        for id in expired_energy {
            if let Some(pending) = self.pending_energy.remove(&id) {
                self.finish_pending_energy(id, pending);
            }
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
        self.energy_points
            .entry(id)
            .or_default()
            .push_back(pending.finish());
    }

    fn finish_pending(&mut self, id: String, pending: PendingPoint) {
        if let Some(point) = pending.finish() {
            self.points.entry(id).or_default().push_back(point);
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
            energy_mwh: rounded_energy(self.energy_mwh),
            energy_source: available_label(self.energy_source),
            energy_confidence: available_label(self.energy_confidence),
        }
    }
}

fn merge_label(current: &mut String, next: &str) {
    if next.is_empty() {
        return;
    }
    if current.is_empty() {
        *current = next.to_owned();
    } else if current != next {
        *current = "mixed".into();
    }
}

fn available_label(value: String) -> String {
    if value.is_empty() {
        "unavailable".into()
    } else {
        value
    }
}

fn rounded_energy(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn encode_cursor(target_id: &str, timestamp_ms: u64) -> anyhow::Result<String> {
    let cursor = HistoryCursor {
        version: FILE_VERSION,
        target_id: target_id.to_owned(),
        after_timestamp_ms: timestamp_ms,
    };
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor)?))
}

fn decode_cursor(value: &str, target_id: &str) -> anyhow::Result<HistoryCursor> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .context("history cursor is not valid base64url")?;
    let cursor: HistoryCursor =
        serde_json::from_slice(&bytes).context("history cursor is not valid JSON")?;
    anyhow::ensure!(
        cursor.version == FILE_VERSION,
        "history cursor version is unsupported"
    );
    anyhow::ensure!(
        cursor.target_id == target_id,
        "history cursor belongs to another target"
    );
    Ok(cursor)
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
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|root| root.join("app-daemon/resource-history-v1.json"))
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".tmp");
    PathBuf::from(value)
}

#[cfg(test)]
mod tests;
