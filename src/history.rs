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

#[derive(Debug, Serialize, Deserialize)]
struct HistoryFile {
    version: u8,
    applications: HashMap<String, Vec<ResourceHistoryPoint>>,
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
                file.applications
                    .into_iter()
                    .map(|(id, points)| (id, points.into()))
                    .collect()
            })
            .unwrap_or_default();
        let mut store = Self {
            path,
            points,
            pending: HashMap::new(),
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
    }
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
