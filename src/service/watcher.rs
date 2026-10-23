//! Watchers are an optimization, never a prerequisite for reconciliation.
use super::{
    ApplicationService,
    catalog_watch::{self, CatalogEvent},
};
use crate::hyprland;
use shelllist_daemon_core::watch::WatchBackoff;
use std::time::{Duration, Instant};
use tokio::{sync::mpsc, time};

const WINDOW_RECOVERY_INTERVAL: Duration = Duration::from_secs(5);
const CATALOG_RECOVERY_INTERVAL: Duration = Duration::from_secs(30);
const HEALTHY_WINDOW_INTERVAL: Duration = Duration::from_secs(30);
const HEALTHY_CATALOG_INTERVAL: Duration = Duration::from_secs(300);
const EVENT_DEBOUNCE: Duration = Duration::from_millis(75);

struct WatchRecovery<W> {
    watcher: Option<W>,
    recovery: WatchBackoff,
}

impl<W> WatchRecovery<W> {
    fn new(now: Instant) -> Self {
        Self {
            watcher: None,
            recovery: WatchBackoff::new(now),
        }
    }

    fn rebuild(&mut self, now: Instant) {
        // Rewatch/rescan storms cannot reset or postpone an existing deadline.
        if self.watcher.take().is_some() {
            self.recovery.failed(now);
        }
    }

    fn attempt<E: std::fmt::Display>(
        &mut self,
        now: Instant,
        create: impl FnOnce() -> Result<W, E>,
    ) -> Result<(), E> {
        if self.watcher.is_some() || !self.recovery.ready(now) {
            return Ok(());
        }
        self.watcher = Some(create().inspect_err(|error| {
            self.recovery.failed(now);
            tracing::warn!(%error, "catalog watcher unavailable; polling continues, retry scheduled");
        })?);
        self.recovery.started(now);
        Ok(())
    }

    fn retry_deadline(&self) -> Option<Instant> {
        self.watcher.is_none().then_some(self.recovery.deadline())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Refresh {
    Windows,
    Catalog,
    Both,
}

pub(super) async fn track_state(service: std::sync::Weak<ApplicationService>) {
    let (window_sender, mut window_events) = mpsc::channel(64);
    let events_task = tokio::spawn(shelllist_hyprland::watch_events_detailed(window_sender));
    let _events = shelllist_daemon_tokio::AbortOnDrop(events_task.abort_handle());
    let (catalog_sender, mut catalog_events) = mpsc::channel(64);
    let mut catalog_watch = WatchRecovery::new(Instant::now());
    let mut window_events_open = true;
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
        let refresh = tokio::select! {
            _ = resumes.changed(), if resumes.has_changed().is_ok() => {
                catalog_watch.rebuild(Instant::now());
                Refresh::Both
            }
            _ = wait_for_watcher_retry(catalog_watch.retry_deadline()) => {
                if catalog_watch.attempt(Instant::now(), || catalog_watch::create(catalog_sender.clone())).is_err() {
                    continue;
                }
                // Close the gap between the last snapshot and installing a watch.
                Refresh::Catalog
            }
            event = window_events.recv(), if window_events_open => {
                if !observe_window_event(event, &mut window_connected, &mut window_events_open) { continue; }
                drain_events(&mut window_events, |event| {
                    observe_window_event(Some(event), &mut window_connected, &mut window_events_open);
                }).await;
                Refresh::Windows
            }
            Some(mut event) = catalog_events.recv() => {
                drain_events(&mut catalog_events, |next| event = event.max(next)).await;
                match event {
                    CatalogEvent::Failed | CatalogEvent::Rewatch => catalog_watch.rebuild(Instant::now()),
                    CatalogEvent::Changed => {},
                }
                Refresh::Catalog
            }
            _ = time::sleep_until(time::Instant::from_std(window_refreshed + window_interval)) => Refresh::Windows,
            _ = time::sleep_until(time::Instant::from_std(catalog_refreshed + catalog_interval)) => {
                // Repair before taking the snapshot, avoiding a second healthy-timer scan.
                if catalog_watch.retry_deadline().is_none() {
                    catalog_watch.rebuild(Instant::now());
                    let _ = catalog_watch.attempt(Instant::now(), || catalog_watch::create(catalog_sender.clone()));
                }
                Refresh::Catalog
            }
        };
        window_available = refresh
            .apply(&service, &mut catalog_refreshed, &mut window_refreshed)
            .await
            .unwrap_or(window_available);
    }
}

impl Refresh {
    /// Apply the selected invalidation concurrently and timestamp only completed reads.
    async fn apply(
        self,
        service: &ApplicationService,
        catalog: &mut Instant,
        windows: &mut Instant,
    ) -> Option<bool> {
        let (_, available) = tokio::join!(
            refresh_if(self != Self::Windows, service.refresh_catalog(), catalog),
            refresh_if(self != Self::Catalog, service.refresh_windows(), windows),
        );
        available
    }
}

async fn refresh_if<T>(
    requested: bool,
    refresh: impl std::future::Future<Output = T>,
    updated: &mut Instant,
) -> Option<T> {
    if !requested {
        return None;
    }
    let value = refresh.await;
    *updated = Instant::now();
    Some(value)
}

async fn drain_events<T>(events: &mut mpsc::Receiver<T>, mut observe: impl FnMut(T)) {
    time::sleep(EVENT_DEBOUNCE).await;
    while let Ok(event) = events.try_recv() {
        observe(event);
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

fn observe_window_event(
    event: Option<shelllist_hyprland::Event>,
    connected: &mut bool,
    open: &mut bool,
) -> bool {
    match event {
        None => {
            *open = false;
            *connected = false;
            tracing::warn!("window event stream ended; continuing reconciliation");
            false
        }
        Some(shelllist_hyprland::Event::Connected) => {
            *connected = true;
            true
        }
        Some(shelllist_hyprland::Event::Disconnected(cause)) => {
            *connected = false;
            if let Some(cause) = cause {
                tracing::warn!(%cause, "window event stream failed; continuing reconciliation");
            }
            true
        }
        Some(shelllist_hyprland::Event::ConnectFailed(cause)) => {
            *connected = false;
            tracing::warn!(%cause, "window event connection failed; continuing reconciliation");
            true
        }
        Some(shelllist_hyprland::Event::Message(line)) => hyprland::window_event_relevant(&line),
    }
}

async fn wait_for_watcher_retry(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(time::Instant::from_std(deadline)).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests;
