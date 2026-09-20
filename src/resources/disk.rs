use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender},
    },
    time::Instant,
};

use super::{APP_DISK_REFRESH_INTERVAL, DiskBreakdown, ResourceProvider};

pub(super) const WORKERS: usize = 2;

#[derive(Debug)]
struct DiskResult {
    target: String,
    usage: Option<DiskBreakdown>,
}

#[derive(Debug)]
struct DiskWorkers {
    requests: SyncSender<String>,
    results: Receiver<DiskResult>,
}

impl DiskWorkers {
    fn start(provider: &Arc<dyn ResourceProvider>) -> std::io::Result<Self> {
        let (requests, receiver) = mpsc::sync_channel::<String>(WORKERS);
        let receiver = Arc::new(Mutex::new(receiver));
        let (sender, results) = mpsc::sync_channel(WORKERS);
        for index in 0..WORKERS {
            let receiver = Arc::clone(&receiver);
            let sender = sender.clone();
            let provider = Arc::clone(provider);
            std::thread::Builder::new()
                .name(format!("app-disk-{index}"))
                .spawn(move || {
                    loop {
                        let request = receiver
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .recv();
                        let Ok(target) = request else { break };
                        let usage = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            provider.application_disk_usage(&target)
                        }))
                        .unwrap_or(None);
                        if sender.send(DiskResult { target, usage }).is_err() {
                            break;
                        }
                    }
                })?;
        }
        Ok(Self { requests, results })
    }

    fn request_due(
        &self,
        targets: &HashSet<&String>,
        samples: &HashMap<String, CachedDisk>,
        in_flight: &mut HashSet<String>,
        now: Instant,
    ) {
        let mut due = targets
            .iter()
            .copied()
            .filter(|target| !in_flight.contains(*target))
            .filter(|target| {
                samples
                    .get(*target)
                    .is_none_or(|sample| now >= sample.next_refresh)
            })
            .collect::<Vec<_>>();
        due.sort_unstable();
        for target in due
            .into_iter()
            .take(WORKERS.saturating_sub(in_flight.len()))
        {
            if self.requests.try_send(target.clone()).is_ok() {
                in_flight.insert(target.clone());
            }
        }
    }
}

#[derive(Debug)]
struct CachedDisk {
    usage: Option<DiskBreakdown>,
    next_refresh: Instant,
}

/// Disk walks never run on the resource sampler. At most two requests are
/// outstanding, including running work, and both channels have bounded capacity.
/// Dropping the cache closes the channels; workers finish their current walk
/// without blocking sampler/service shutdown.
#[derive(Debug, Default)]
pub(super) struct AppDiskCache {
    samples: HashMap<String, CachedDisk>,
    in_flight: HashSet<String>,
    workers: Option<DiskWorkers>,
}

impl AppDiskCache {
    pub(super) fn read<'a>(
        &mut self,
        provider: &Arc<dyn ResourceProvider>,
        targets: impl IntoIterator<Item = &'a String>,
        now: Instant,
    ) -> HashMap<String, DiskBreakdown> {
        let targets = targets.into_iter().collect::<HashSet<_>>();
        self.samples.retain(|target, _| targets.contains(target));
        if !targets.is_empty() && self.workers.is_none() {
            self.workers = DiskWorkers::start(provider)
                .inspect_err(
                    |error| tracing::warn!(%error, "application disk workers could not start"),
                )
                .ok();
        }
        if let Some(workers) = &self.workers {
            for result in workers.results.try_iter() {
                self.in_flight.remove(&result.target);
                if !targets.contains(&result.target) {
                    continue;
                }
                let cached = self.samples.entry(result.target).or_insert(CachedDisk {
                    usage: None,
                    next_refresh: now,
                });
                // An incomplete/failed refresh must not replace a completed
                // footprint with a partial total or invented zero.
                cached.usage = result.usage.or(cached.usage);
                cached.next_refresh = now + APP_DISK_REFRESH_INTERVAL;
            }
            workers.request_due(&targets, &self.samples, &mut self.in_flight, now);
        }
        self.samples
            .iter()
            .filter_map(|(target, sample)| Some((target.clone(), sample.usage?)))
            .collect()
    }
}

#[cfg(test)]
mod tests;
