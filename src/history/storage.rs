use super::{
    Database, ENERGY_RETENTION_MILLISECONDS, FinishedBuckets, HistoryPage, PendingEnergy,
    RETENTION_MILLISECONDS, ResourceHistoryPoint, aggregate, decode_cursor, encode_cursor, invalid,
    new_cursor_epoch, summary,
};
use anyhow::{Context, Result, ensure};
use std::collections::HashMap;
use turso::{Value, params};

pub(super) fn timestamp(value: u64) -> Result<i64> {
    value
        .try_into()
        .context("history timestamp exceeds signed 64-bit milliseconds")
}

impl Database {
    pub(super) async fn apply(
        &mut self,
        finished: FinishedBuckets,
        now: u64,
        force_prune: bool,
    ) -> Result<()> {
        self.healthy()?;
        let prune = force_prune
            || self
                .last_prune
                .is_none_or(|last| now < last || now - last >= 60_000);
        if finished.resources.is_empty() && finished.energy.is_empty() && !prune {
            return Ok(());
        }
        self.begin().await?;
        let result = async {
            for (id, point) in finished.resources { self.insert_resource(&id, point).await?; }
            for (id, point) in finished.energy {
                ensure!(point.energy_mwh.is_finite() && point.energy_mwh >= 0.0, "invalid energy history value");
                self.connection.execute(
                    "INSERT INTO energy_points(target_id,timestamp_ms,energy_mwh,energy_source,energy_confidence) VALUES (?1,?2,?3,?4,?5) \
                     ON CONFLICT(target_id,timestamp_ms) DO UPDATE SET energy_mwh=energy_points.energy_mwh+excluded.energy_mwh, \
                     energy_source=CASE WHEN energy_points.energy_source IN ('','unavailable') THEN excluded.energy_source WHEN excluded.energy_source IN ('','unavailable') OR energy_points.energy_source=excluded.energy_source THEN energy_points.energy_source ELSE 'mixed' END, \
                     energy_confidence=CASE WHEN energy_points.energy_confidence IN ('','unavailable') THEN excluded.energy_confidence WHEN excluded.energy_confidence IN ('','unavailable') OR energy_points.energy_confidence=excluded.energy_confidence THEN energy_points.energy_confidence ELSE 'mixed' END",
                    params![id, timestamp(point.timestamp_ms)?, point.energy_mwh, point.energy_source, point.energy_confidence]
                ).await?;
            }
            if prune {
                self.connection.execute("DELETE FROM resource_points WHERE timestamp_ms < ?", [timestamp(now.saturating_sub(RETENTION_MILLISECONDS))?]).await?;
                self.connection.execute("DELETE FROM energy_points WHERE timestamp_ms < ?", [timestamp(now.saturating_sub(ENERGY_RETENTION_MILLISECONDS))?]).await?;
                self.connection.execute("DELETE FROM cursor_epochs WHERE NOT EXISTS (SELECT 1 FROM resource_points WHERE resource_points.target_id=cursor_epochs.target_id)", ()).await?;
            }
            #[cfg(test)]
            if matches!(self.fault, Some(super::database::Fault::Write | super::database::Fault::Rollback)) {
                if self.fault == Some(super::database::Fault::Write) { self.fault = None; }
                anyhow::bail!("injected write failure");
            }
            Ok(())
        }.await;
        self.finish(result).await?;
        if prune {
            self.last_prune = Some(now);
        }
        Ok(())
    }

    async fn point(&self, id: &str, timestamp_ms: u64) -> Result<Option<ResourceHistoryPoint>> {
        self.connection
            .query(
                "SELECT payload,timestamp_ms,start_ms FROM resource_points WHERE target_id=? AND timestamp_ms=?",
                params![id, timestamp(timestamp_ms)?],
            )
            .await?
            .next()
            .await?
            .map(|row| decode_point(&row))
            .transpose()
    }

    async fn insert_resource(&self, id: &str, mut point: ResourceHistoryPoint) -> Result<()> {
        let latest = self.connection.query("SELECT timestamp_ms FROM resource_points WHERE target_id=? ORDER BY timestamp_ms DESC LIMIT 1", [id])
            .await?.next().await?.map(|row| row.get::<i64>(0)).transpose()?;
        let mut rewrite = latest.is_some_and(|latest| point.timestamp_ms <= latest as u64);
        if let Some(previous) = self
            .point(id, point.timestamp_ms.saturating_sub(point.duration_ms))
            .await?
            && aggregate::adjacent_in_bucket(&previous, &point)
        {
            self.connection
                .execute(
                    "DELETE FROM resource_points WHERE target_id=? AND timestamp_ms=?",
                    params![id, timestamp(previous.timestamp_ms)?],
                )
                .await?;
            point = aggregate::PendingPoint::merge_finished(&previous, point);
            rewrite = true;
        }
        if let Some(previous) = self.point(id, point.timestamp_ms).await? {
            point = aggregate::PendingPoint::merge_finished(&previous, point);
        }
        point.resources.metric_availability = crate::projection::MetricAvailability::empty();
        let names = summary::sql_column_names();
        let mut values = vec![
            Value::Text(id.to_owned()),
            Value::Integer(timestamp(point.timestamp_ms)?),
            Value::Integer(timestamp(
                point.timestamp_ms.saturating_sub(point.duration_ms),
            )?),
            Value::Text(serde_json::to_string(&point)?),
        ];
        values.extend(summary::sql_values(&point));
        let placeholders = vec!["?"; values.len()].join(",");
        let columns = format!(
            "target_id,timestamp_ms,start_ms,payload,energy_confidence,{}",
            names.join(",")
        );
        let updates = ["start_ms", "payload", "energy_confidence"]
            .into_iter()
            .map(str::to_owned)
            .chain(names)
            .map(|name| format!("{name}=excluded.{name}"))
            .collect::<Vec<_>>()
            .join(",");
        self.connection.execute(format!("INSERT INTO resource_points({columns}) VALUES ({placeholders}) ON CONFLICT(target_id,timestamp_ms) DO UPDATE SET {updates}"), values).await?;
        if rewrite {
            self.connection.execute("INSERT INTO cursor_epochs(target_id,epoch) VALUES (?,?) ON CONFLICT(target_id) DO UPDATE SET epoch=excluded.epoch", params![id, new_cursor_epoch()]).await?;
        } else {
            self.connection.execute("INSERT INTO cursor_epochs(target_id,epoch) VALUES (?,?) ON CONFLICT(target_id) DO NOTHING", params![id, new_cursor_epoch()]).await?;
        }
        Ok(())
    }

    pub(super) async fn epoch(&self, id: &str) -> Result<String> {
        Ok(self
            .connection
            .query("SELECT epoch FROM cursor_epochs WHERE target_id=?", [id])
            .await?
            .next()
            .await?
            .map(|row| row.get::<String>(0))
            .transpose()?
            .unwrap_or_default())
    }

    pub(super) async fn query_window(
        &self,
        id: &str,
        since: Option<u64>,
        until: Option<u64>,
        cursor: Option<&str>,
        limit: usize,
        now: u64,
    ) -> Result<HistoryPage> {
        self.healthy()?;
        let cutoff = now.saturating_sub(RETENTION_MILLISECONDS);
        let epoch = self.epoch(id).await?;
        let after = cursor
            .map(|cursor| decode_cursor(cursor, id, &epoch).map_err(invalid))
            .transpose()?;
        if let Some(after) = after {
            let exists = after >= cutoff
                && self
                    .connection
                    .query(
                        "SELECT 1 FROM resource_points WHERE target_id=? AND timestamp_ms=?",
                        params![id, timestamp(after).map_err(invalid)?],
                    )
                    .await?
                    .next()
                    .await?
                    .is_some();
            if !exists {
                return Err(invalid(
                    "history cursor expired; restart pagination without a cursor",
                ));
            }
        }
        let lower = since.unwrap_or(0).max(cutoff);
        // Bounds and summaries are independent of the page/cursor. The primary
        // key seeks by target and endpoint; no other application's rows decode.
        let upper = until.unwrap_or(i64::MAX as u64);
        let bounds = "target_id=?1 AND timestamp_ms>=?2 AND (timestamp_ms<=?3 OR start_ms<?3)";
        let first = self.connection.query(format!("SELECT start_ms FROM resource_points WHERE {bounds} ORDER BY timestamp_ms LIMIT 1"), params![id, timestamp(lower)?, timestamp(upper)?]).await?.next().await?.map(|row| row.get::<i64>(0)).transpose()?;
        let last = self.connection.query(format!("SELECT timestamp_ms FROM resource_points WHERE {bounds} ORDER BY timestamp_ms DESC LIMIT 1"), params![id, timestamp(lower)?, timestamp(upper)?]).await?.next().await?.map(|row| row.get::<i64>(0)).transpose()?;
        let end = until.unwrap_or_else(|| now.max(last.unwrap_or(0) as u64));
        let start = since.unwrap_or_else(|| first.map_or(end, |value| value as u64));
        let summary = summary::sql_summary(&self.connection, id, lower, start, end, &epoch).await?;
        // Use a range constraint rather than an OR around the cursor so the
        // composite index can seek directly, including deep pagination.
        if after.is_some_and(|after| after >= upper) {
            return Ok(HistoryPage {
                points: Vec::new(),
                summary,
                has_more: false,
                next_cursor: cursor.map(str::to_owned),
            });
        }
        let page_lower = lower.max(after.map_or(0, |after| after.saturating_add(1)));
        let mut rows = self.connection.query("SELECT payload,timestamp_ms,start_ms FROM resource_points WHERE target_id=? AND timestamp_ms>=? AND timestamp_ms<=? ORDER BY timestamp_ms LIMIT ?", params![id, timestamp(page_lower)?, timestamp(upper)?, (limit + 1) as i64]).await?;
        let mut points = Vec::with_capacity(limit.min(256));
        let mut has_more = false;
        while let Some(row) = rows.next().await? {
            if points.len() == limit {
                has_more = true;
                break;
            }
            points.push(summary::normalize(decode_point(&row)?));
        }
        let next_cursor = points
            .last()
            .map(|p| encode_cursor(id, p.timestamp_ms, &epoch))
            .transpose()?
            .or_else(|| cursor.map(str::to_owned));
        Ok(HistoryPage {
            points,
            summary,
            has_more,
            next_cursor,
        })
    }

    pub(super) async fn energy_totals(
        &self,
        since: u64,
        until: u64,
        now: u64,
    ) -> Result<HashMap<String, PendingEnergy>> {
        self.healthy()?;
        let label = |column: &str| {
            format!(
                "CASE COUNT(DISTINCT CASE WHEN {column} NOT IN ('','unavailable') THEN {column} END) WHEN 0 THEN '' WHEN 1 THEN MIN(CASE WHEN {column} NOT IN ('','unavailable') THEN {column} END) ELSE 'mixed' END"
            )
        };
        let mut rows = self.connection.query(format!("SELECT target_id,SUM(energy_mwh),{},{} FROM energy_points WHERE timestamp_ms>=? AND timestamp_ms<=? GROUP BY target_id", label("energy_source"), label("energy_confidence")), params![timestamp(since.max(now.saturating_sub(ENERGY_RETENTION_MILLISECONDS)))?, timestamp(until)?]).await?;
        let mut totals = HashMap::new();
        while let Some(row) = rows.next().await? {
            totals.insert(
                row.get(0)?,
                PendingEnergy {
                    energy_mwh: row.get(1)?,
                    energy_source: row.get(2)?,
                    energy_confidence: row.get(3)?,
                    ..Default::default()
                },
            );
        }
        Ok(totals)
    }
}

fn decode_point(row: &turso::Row) -> Result<ResourceHistoryPoint> {
    let point: ResourceHistoryPoint = serde_json::from_str(&row.get::<String>(0)?)
        .context("invalid resource history row; database was not reset")?;
    ensure!(
        timestamp(point.timestamp_ms)? == row.get::<i64>(1)?
            && timestamp(point.timestamp_ms.saturating_sub(point.duration_ms))?
                == row.get::<i64>(2)?,
        "resource history payload disagrees with its indexed interval; database was not reset"
    );
    Ok(point)
}
