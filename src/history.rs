//! Turso-backed history. Only unfinished buckets live in memory; all database
//! work runs on blocking workers, serialized by the service's owned mutex guard.
use std::{
    borrow::Cow,
    collections::HashMap,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::executor::block_on;
use serde::{Deserialize, Serialize};
use shelllist_daemon_core::{XdgRoot, resolve_xdg_path};

use crate::{
    metrics::{available_label, merge_label, rounded},
    model::{ResourceHistoryPoint, ResourceUsage},
};

mod aggregate;
mod database;
mod storage;
pub mod summary;
use aggregate::PendingPoint;
use database::Database;

const CURSOR_VERSION: u8 = 3;
const BUCKET_MILLISECONDS: u64 = 15_000;
const RETENTION_MILLISECONDS: u64 = 24 * 60 * 60 * 1000;
const ENERGY_BUCKET_MILLISECONDS: u64 = 60_000;
const ENERGY_RETENTION_MILLISECONDS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Default)]
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
struct HistoryCursor<'a> {
    version: u8,
    #[serde(borrow)]
    target_id: Cow<'a, str>,
    after_timestamp_ms: u64,
    #[serde(borrow)]
    epoch: Cow<'a, str>,
}

#[derive(Debug)]
pub struct HistoryPage {
    pub points: Vec<ResourceHistoryPoint>,
    pub summary: summary::HistorySummary,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

#[derive(Debug)]
pub(crate) struct InvalidHistoryQuery(String);
impl std::fmt::Display for InvalidHistoryQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for InvalidHistoryQuery {}
fn invalid(message: impl ToString) -> anyhow::Error {
    InvalidHistoryQuery(message.to_string()).into()
}

#[derive(Clone, Default)]
struct PendingBuckets {
    resources: HashMap<String, PendingPoint>,
    energy: HashMap<String, PendingEnergy>,
}

#[derive(Default)]
struct FinishedBuckets {
    resources: Vec<(String, ResourceHistoryPoint)>,
    energy: Vec<(String, EnergyHistoryPoint)>,
}

pub struct HistoryStore {
    database: Database,
    pending: PendingBuckets,
}

impl HistoryStore {
    pub fn open_default() -> anyhow::Result<Self> {
        let path = resolve_xdg_path(
            XdgRoot::State,
            "app-daemon",
            Path::new("resource-history-turso-v1.db"),
        )
        .context("cannot resolve application history state directory")?;
        Self::open(&path)
    }

    pub(crate) fn open(path: &Path) -> anyhow::Result<Self> {
        let mut store = Self {
            database: block_on(Database::open(path))?,
            pending: PendingBuckets::default(),
        };
        store.flush(false)?;
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn load(path: Option<std::path::PathBuf>) -> Self {
        match path {
            Some(path) => Self::open(&path).unwrap(),
            None => Self {
                database: block_on(Database::temporary()).unwrap(),
                pending: PendingBuckets::default(),
            },
        }
    }

    /// Commit all completed buckets from a sampling pass in one transaction.
    /// A failed transaction leaves the previous pending aggregates intact.
    pub fn record_batch<'a>(
        &mut self,
        timestamp_ms: u64,
        duration_seconds: f64,
        samples: impl IntoIterator<Item = (&'a str, &'a ResourceUsage)>,
    ) -> anyhow::Result<()> {
        self.database.healthy()?;
        if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
            return Ok(());
        }
        storage::timestamp(timestamp_ms)?;
        let mut pending = self.pending.clone();
        let mut finished = FinishedBuckets::default();
        pending.flush_expired(timestamp_ms, &mut finished);
        let duration_ms = (duration_seconds * 1000.0).round().max(1.0) as u64;
        for (id, usage) in samples {
            pending.record(id, timestamp_ms, duration_ms, usage, &mut finished);
        }
        self.commit(pending, finished, timestamp_ms)
    }

    #[cfg(test)]
    pub fn record(
        &mut self,
        id: &str,
        timestamp_ms: u64,
        duration_seconds: f64,
        usage: &ResourceUsage,
    ) -> anyhow::Result<()> {
        self.record_batch(timestamp_ms, duration_seconds, [(id, usage)])
    }

    fn commit(
        &mut self,
        pending: PendingBuckets,
        finished: FinishedBuckets,
        now: u64,
    ) -> anyhow::Result<()> {
        block_on(self.database.apply(finished, now, false))?;
        self.pending = pending;
        Ok(())
    }

    fn flush_expired(&mut self, timestamp_ms: u64) -> anyhow::Result<()> {
        self.database.healthy()?;
        let mut pending = self.pending.clone();
        let mut finished = FinishedBuckets::default();
        pending.flush_expired(timestamp_ms, &mut finished);
        self.commit(pending, finished, now_milliseconds())
    }

    /// Periodic maintenance finalizes idle applications and prunes by indexed
    /// timestamp ranges. Shutdown also commits partial resource/energy buckets.
    pub fn flush(&mut self, final_save: bool) -> anyhow::Result<()> {
        self.flush_expired(if final_save {
            u64::MAX
        } else {
            now_milliseconds()
        })?;
        if final_save {
            block_on(self.database.checkpoint())?;
        }
        Ok(())
    }

    pub fn query_window(
        &mut self,
        target_id: &str,
        since_ms: Option<u64>,
        until_ms: Option<u64>,
        cursor: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<HistoryPage> {
        if since_ms
            .zip(until_ms)
            .is_some_and(|(start, end)| start > end)
        {
            return Err(invalid("history window start exceeds its end"));
        }
        for value in [since_ms, until_ms].into_iter().flatten() {
            storage::timestamp(value).map_err(invalid)?;
        }
        let now = now_milliseconds();
        self.flush_expired(now)?;
        block_on(self.database.query_window(
            target_id,
            since_ms,
            until_ms,
            cursor,
            limit.clamp(1, 10_000),
            now,
        ))
    }

    pub fn energy_totals(
        &mut self,
        since_ms: u64,
        until_ms: u64,
    ) -> anyhow::Result<Vec<EnergyTotal>> {
        storage::timestamp(since_ms).map_err(invalid)?;
        storage::timestamp(until_ms).map_err(invalid)?;
        self.flush_expired(until_ms)?;
        let mut totals = block_on(self.database.energy_totals(
            since_ms,
            until_ms,
            now_milliseconds(),
        ))?;
        for (id, point) in &self.pending.energy {
            if point
                .timestamp_ms
                .saturating_add(ENERGY_BUCKET_MILLISECONDS)
                >= since_ms
                && point.timestamp_ms <= until_ms
            {
                totals.entry(id.clone()).or_default().add(
                    point.energy_mwh,
                    &point.energy_source,
                    &point.energy_confidence,
                );
            }
        }
        Ok(totals
            .into_iter()
            .filter_map(|(target_id, total)| {
                (total.energy_mwh > 0.0).then(|| EnergyTotal {
                    target_id,
                    energy_mwh: rounded(total.energy_mwh, 4),
                    energy_source: available_label(total.energy_source),
                    energy_confidence: available_label(total.energy_confidence),
                })
            })
            .collect())
    }

    #[cfg(test)]
    fn insert_points(
        &mut self,
        id: String,
        points: impl IntoIterator<Item = ResourceHistoryPoint>,
    ) -> anyhow::Result<()> {
        block_on(self.database.apply(
            FinishedBuckets {
                resources: points.into_iter().map(|p| (id.clone(), p)).collect(),
                ..Default::default()
            },
            now_milliseconds(),
            false,
        ))
    }

    #[cfg(test)]
    fn prune(&mut self, now: u64) -> anyhow::Result<()> {
        block_on(self.database.apply(FinishedBuckets::default(), now, true))
    }
}

impl PendingBuckets {
    fn flush_expired(&mut self, now: u64, finished: &mut FinishedBuckets) {
        for (id, pending) in self
            .resources
            .extract_if(|_, p| p.timestamp_ms.saturating_add(BUCKET_MILLISECONDS) <= now)
        {
            if let Some(point) = pending.finish() {
                finished.resources.push((id, point));
            }
        }
        for (id, pending) in self
            .energy
            .extract_if(|_, p| p.timestamp_ms.saturating_add(ENERGY_BUCKET_MILLISECONDS) <= now)
        {
            finished.energy.push((id, pending.finish()));
        }
    }

    fn record(
        &mut self,
        id: &str,
        timestamp_ms: u64,
        duration_ms: u64,
        usage: &ResourceUsage,
        finished: &mut FinishedBuckets,
    ) {
        let energy_start = timestamp_ms - timestamp_ms % ENERGY_BUCKET_MILLISECONDS;
        if self
            .energy
            .get(id)
            .is_some_and(|p| p.timestamp_ms != energy_start)
            && let Some(pending) = self.energy.remove(id)
        {
            finished.energy.push((id.to_owned(), pending.finish()));
        }
        let energy = self.energy.entry(id.to_owned()).or_default();
        energy.timestamp_ms = energy_start;
        energy.add(
            usage.energy.energy_mwh,
            &usage.energy.energy_source,
            &usage.energy.energy_confidence,
        );
        let sample_start = timestamp_ms.saturating_sub(duration_ms);
        let mut start = sample_start.max(timestamp_ms.saturating_sub(RETENTION_MILLISECONDS));
        while start < timestamp_ms {
            let bucket_start = start - start % BUCKET_MILLISECONDS;
            let end = timestamp_ms.min(bucket_start.saturating_add(BUCKET_MILLISECONDS));
            if self
                .resources
                .get(id)
                .is_some_and(|p| p.timestamp_ms != bucket_start || p.end_ms != start)
                && let Some(pending) = self.resources.remove(id)
                && let Some(point) = pending.finish()
            {
                finished.resources.push((id.to_owned(), point));
            }
            let pending = self.resources.entry(id.to_owned()).or_default();
            pending.timestamp_ms = bucket_start;
            pending.add(usage, start - sample_start..end - sample_start, duration_ms);
            pending.end_ms = end;
            start = end;
        }
        // Commit this target's bucket when it ends exactly at the sample.
        // Other expired targets were drained once at the start of the batch.
        if self.resources.get(id).is_some_and(|pending| {
            pending.timestamp_ms.saturating_add(BUCKET_MILLISECONDS) <= timestamp_ms
        }) && let Some(pending) = self.resources.remove(id)
            && let Some(point) = pending.finish()
        {
            finished.resources.push((id.to_owned(), point));
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

fn new_cursor_epoch() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn encode_cursor(target_id: &str, timestamp_ms: u64, epoch: &str) -> anyhow::Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&HistoryCursor {
        version: CURSOR_VERSION,
        target_id: target_id.into(),
        after_timestamp_ms: timestamp_ms,
        epoch: epoch.into(),
    })?))
}
fn decode_cursor(value: &str, target_id: &str, epoch: &str) -> anyhow::Result<u64> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .context("history cursor is not valid base64url")?;
    let cursor: HistoryCursor<'_> =
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
    Ok(cursor.after_timestamp_ms)
}

pub fn now_milliseconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod persistence_tests;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod tests;
