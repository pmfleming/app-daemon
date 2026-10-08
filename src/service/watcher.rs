//! Watchers are an optimization, never a prerequisite for reconciliation.
use super::{
    ApplicationService,
    catalog_watch::{self, CatalogEvent},
};
use crate::hyprland;
use std::time::{Duration, Instant};
use tokio::{sync::mpsc, time};

const WINDOW_RECOVERY_INTERVAL: Duration = Duration::from_secs(5);
const CATALOG_RECOVERY_INTERVAL: Duration = Duration::from_secs(30);
const HEALTHY_WINDOW_INTERVAL: Duration = Duration::from_secs(30);
const HEALTHY_CATALOG_INTERVAL: Duration = Duration::from_secs(300);
const EVENT_DEBOUNCE: Duration = Duration::from_millis(75);

const MIN_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);

struct WatchRecovery<W> {
    watcher: Option<W>,
    retry_at: Instant,
    retry_delay: Duration,
}

impl<W> WatchRecovery<W> {
    fn new(now: Instant) -> Self {
        Self {
            watcher: None,
            retry_at: now,
            retry_delay: MIN_RETRY,
        }
    }

    fn rebuild(&mut self, now: Instant) {
        self.watcher = None;
        self.retry_at = now;
        self.retry_delay = MIN_RETRY;
    }

    fn failed(&mut self, now: Instant) {
        self.watcher = None;
        self.retry_at = now + self.retry_delay;
        self.retry_delay = (self.retry_delay * 2).min(MAX_RETRY);
    }

    fn attempt<E>(&mut self, now: Instant, create: impl FnOnce() -> Result<W, E>) -> Result<(), E> {
        if self.watcher.is_some() || now < self.retry_at {
            return Ok(());
        }
        match create() {
            Ok(watcher) => {
                self.watcher = Some(watcher);
                self.retry_delay = MIN_RETRY;
                Ok(())
            }
            Err(error) => {
                self.failed(now);
                Err(error)
            }
        }
    }

    fn retry_deadline(&self) -> Option<Instant> {
        self.watcher.is_none().then_some(self.retry_at)
    }
}

pub(super) async fn track_state(service: std::sync::Weak<ApplicationService>) {
    let (window_sender, mut window_events) = mpsc::channel(64);
    let events_task = tokio::spawn(hyprland::watch_window_events(window_sender));
    let _events = shelllist_daemon_tokio::AbortOnDrop(events_task.abort_handle());
    let (catalog_sender, mut catalog_events) = mpsc::channel(64);
    let mut catalog_watch = WatchRecovery::new(Instant::now());
    let mut window_events_open = true;
    let mut catalog_events_open = true;
    let mut window_connected = false;
    let Some(initial) = service.upgrade() else {
        return;
    };
    let mut resumes = initial.resume_events.clone();
    let (_, mut window_available) =
        tokio::join!(initial.refresh_catalog(), initial.refresh_windows());
    drop(initial);
    let mut window_refreshed = Instant::now();
    let mut catalog_refreshed = Instant::now();

    loop {
        let Some(service) = service.upgrade() else {
            return;
        };
        let (window_interval, catalog_interval) = reconciliation_intervals(
            window_connected && window_available,
            catalog_watch.retry_deadline().is_none(),
        );
        tokio::select! {
            _ = resumes.changed(), if resumes.has_changed().is_ok() => {
                (_, window_available) = tokio::join!(service.refresh_catalog(), service.refresh_windows());
                window_refreshed = Instant::now();
                catalog_refreshed = window_refreshed;
                catalog_watch.rebuild(catalog_refreshed);
            }
            _ = wait_for_watcher_retry(catalog_watch.retry_deadline()) => {
                if let Err(error) = catalog_watch.attempt(Instant::now(), || catalog_watch::create(catalog_sender.clone())) {
                    tracing::warn!(%error, "catalog watcher unavailable; polling continues, retry scheduled");
                } else {
                    // Close the gap between the last snapshot and installing a watch.
                    service.refresh_catalog().await;
                    catalog_refreshed = Instant::now();
                }
            }
            event = window_events.recv(), if window_events_open => {
                let Some(event) = event else {
                    window_events_open = false;
                    window_connected = false;
                    tracing::warn!("window event stream ended; continuing reconciliation");
                    continue;
                };
                if !observe_window_event(event, &mut window_connected) { continue; }
                time::sleep(EVENT_DEBOUNCE).await;
                while let Ok(event) = window_events.try_recv() {
                    observe_window_event(event, &mut window_connected);
                }
                window_available = service.refresh_windows().await;
                window_refreshed = Instant::now();
            }
            event = catalog_events.recv(), if catalog_events_open => {
                let Some(event) = event else { catalog_events_open = false; continue; };
                let mut failed = matches!(event, CatalogEvent::Failed);
                let mut rewatch = matches!(event, CatalogEvent::Rewatch);
                time::sleep(EVENT_DEBOUNCE).await;
                while let Ok(event) = catalog_events.try_recv() {
                    failed |= matches!(event, CatalogEvent::Failed);
                    rewatch |= matches!(event, CatalogEvent::Rewatch);
                }
                if failed { catalog_watch.failed(Instant::now()); }
                else if rewatch { catalog_watch.rebuild(Instant::now()); }
                service.refresh_catalog().await;
                catalog_refreshed = Instant::now();
            }
            _ = time::sleep_until(time::Instant::from_std(window_refreshed + window_interval)) => {
                window_available = service.refresh_windows().await;
                window_refreshed = Instant::now();
            }
            _ = time::sleep_until(time::Instant::from_std(catalog_refreshed + catalog_interval)) => {
                // Install the repaired watch before taking a single snapshot,
                // avoiding a second full catalog scan on every healthy timer.
                if catalog_watch.retry_deadline().is_none() {
                    catalog_watch.rebuild(Instant::now());
                    if let Err(error) = catalog_watch.attempt(Instant::now(), || catalog_watch::create(catalog_sender.clone())) {
                        tracing::warn!(%error, "catalog watch repair failed; polling continues");
                    }
                }
                service.refresh_catalog().await;
                catalog_refreshed = Instant::now();
            }
        }
    }
}

fn reconciliation_intervals(windows_healthy: bool, catalog_healthy: bool) -> (Duration, Duration) {
    (
        if windows_healthy {
            HEALTHY_WINDOW_INTERVAL
        } else {
            WINDOW_RECOVERY_INTERVAL
        },
        if catalog_healthy {
            HEALTHY_CATALOG_INTERVAL
        } else {
            CATALOG_RECOVERY_INTERVAL
        },
    )
}

fn observe_window_event(event: shelllist_hyprland::Event, connected: &mut bool) -> bool {
    match event {
        shelllist_hyprland::Event::Connected => {
            *connected = true;
            true
        }
        shelllist_hyprland::Event::Disconnected => {
            *connected = false;
            true
        }
        shelllist_hyprland::Event::Message(line) => hyprland::window_event_relevant(&line),
    }
}

async fn wait_for_watcher_retry(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_RETRY, MIN_RETRY, WatchRecovery};
    use std::time::Instant;
    #[test]
    fn failed_initialization_retries_with_bounded_backoff_and_recovers() {
        let start = Instant::now();
        let mut recovery = WatchRecovery::<()>::new(start);
        assert!(
            recovery
                .attempt(start, || Err("no inotify descriptors"))
                .is_err()
        );
        assert_eq!(recovery.retry_deadline(), Some(start + MIN_RETRY));
        recovery
            .attempt(start, || -> Result<(), ()> {
                panic!("must back off");
            })
            .unwrap();
        let later = start + MIN_RETRY;
        recovery.attempt(later, || Ok::<_, ()>(())).unwrap();
        assert_eq!(recovery.retry_deadline(), None);
        recovery.failed(later); // backend runtime failure drops and recreates it
        assert_eq!(recovery.retry_deadline(), Some(later + MIN_RETRY));
        let mut now = later;
        for _ in 0..20 {
            now = recovery.retry_deadline().unwrap();
            assert!(recovery.attempt(now, || Err("still unavailable")).is_err());
            assert!(recovery.retry_deadline().unwrap() - now <= MAX_RETRY);
        }
        recovery
            .attempt(now + MAX_RETRY, || Ok::<_, ()>(()))
            .unwrap();
        assert_eq!(recovery.retry_deadline(), None);
    }
}
