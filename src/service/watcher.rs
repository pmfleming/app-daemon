//! Watchers are an optimization, never a prerequisite for reconciliation.
use super::{
    ApplicationService,
    catalog_watch::{self, CatalogEvent},
};
use crate::hyprland;
use shelllist_daemon_core::watch::WatchBackoff;
use std::time::{Duration, Instant};
use tokio::{
    sync::{mpsc, watch},
    time,
};

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

    fn invalidate(&mut self, event: CatalogEvent, now: Instant) {
        if event != CatalogEvent::Changed {
            self.rebuild(now);
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

    fn repair<E: std::fmt::Display>(
        &mut self,
        now: Instant,
        create: impl FnOnce() -> Result<W, E>,
    ) {
        // Repair before the periodic snapshot, without postponing pending retries.
        if self.watcher.is_some() {
            self.rebuild(now);
            let _ = self.attempt(now, create);
        }
    }

    fn retry_deadline(&self) -> Option<Instant> {
        self.watcher.is_none().then_some(self.recovery.deadline())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refresh {
    Windows,
    Catalog,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Connection {
    Disconnected,
    Connected,
    Closed,
}

struct WindowState {
    connection: Connection,
    available: bool,
    refreshed: Instant,
}

impl WindowState {
    fn observe(&mut self, event: Option<shelllist_hyprland::Event>) -> bool {
        use shelllist_hyprland::Event;
        match event {
            None => {
                self.connection = Connection::Closed;
                tracing::warn!("window event stream ended; continuing reconciliation");
                false
            }
            Some(Event::Connected) => {
                self.connection = Connection::Connected;
                true
            }
            Some(Event::Disconnected(cause)) => {
                self.connection = Connection::Disconnected;
                if let Some(cause) = cause {
                    tracing::warn!(%cause, "window event stream failed; continuing reconciliation");
                }
                true
            }
            Some(Event::ConnectFailed(cause)) => {
                self.connection = Connection::Disconnected;
                tracing::warn!(%cause, "window event connection failed; continuing reconciliation");
                true
            }
            Some(Event::Message(line)) => hyprland::window_event_relevant(&line),
        }
    }

    fn deadline(&self) -> Instant {
        self.refreshed
            + if self.connection == Connection::Connected && self.available {
                HEALTHY_WINDOW_INTERVAL
            } else {
                WINDOW_RECOVERY_INTERVAL
            }
    }

    async fn batch(
        &mut self,
        event: Option<shelllist_hyprland::Event>,
        events: &mut mpsc::Receiver<shelllist_hyprland::Event>,
    ) -> Option<Refresh> {
        if !self.observe(event) {
            return None;
        }
        drain_events(events, |event| {
            self.observe(Some(event));
        })
        .await;
        Some(Refresh::Windows)
    }
}

struct WatchState {
    windows: WindowState,
    catalog: WatchRecovery<notify::RecommendedWatcher>,
    catalog_refreshed: Instant,
}

impl WatchState {
    fn catalog_deadline(&self) -> Instant {
        self.catalog_refreshed
            + if self.catalog.retry_deadline().is_none() {
                HEALTHY_CATALOG_INTERVAL
            } else {
                CATALOG_RECOVERY_INTERVAL
            }
    }

    async fn react(&mut self, signal: Signal, sources: &mut EventSources) -> Option<Refresh> {
        let create = || catalog_watch::create(sources.catalog_sender.clone());
        match signal {
            Signal::Resume => {
                self.catalog.rebuild(Instant::now());
                Some(Refresh::Both)
            }
            Signal::Retry => {
                // Close the gap between the last snapshot and installing a watch.
                self.catalog
                    .attempt(Instant::now(), create)
                    .ok()
                    .map(|()| Refresh::Catalog)
            }
            Signal::Window(event) => self.windows.batch(event, &mut sources.windows).await,
            Signal::Catalog(mut event) => {
                drain_events(&mut sources.catalog, |next| event = event.max(next)).await;
                self.catalog.invalidate(event, Instant::now());
                Some(Refresh::Catalog)
            }
            Signal::WindowPoll => Some(Refresh::Windows),
            Signal::CatalogPoll => {
                self.catalog.repair(Instant::now(), create);
                Some(Refresh::Catalog)
            }
        }
    }
}

enum Signal {
    Resume,
    Retry,
    Window(Option<shelllist_hyprland::Event>),
    Catalog(CatalogEvent),
    WindowPoll,
    CatalogPoll,
}

struct EventSources {
    windows: mpsc::Receiver<shelllist_hyprland::Event>,
    catalog: mpsc::Receiver<CatalogEvent>,
    catalog_sender: mpsc::Sender<CatalogEvent>,
    resumes: watch::Receiver<u64>,
}

impl EventSources {
    // Only cancellation-safe waits belong in this select. Once an event wins,
    // its debounce/reduction completes before timers or other streams compete.
    async fn next(&mut self, state: &WatchState) -> Signal {
        tokio::select! {
            _ = self.resumes.changed(), if self.resumes.has_changed().is_ok() => Signal::Resume,
            _ = wait_for_watcher_retry(state.catalog.retry_deadline()) => Signal::Retry,
            event = self.windows.recv(), if state.windows.connection != Connection::Closed => Signal::Window(event),
            Some(event) = self.catalog.recv() => Signal::Catalog(event),
            _ = time::sleep_until(state.windows.deadline().into()) => Signal::WindowPoll,
            _ = time::sleep_until(state.catalog_deadline().into()) => Signal::CatalogPoll,
        }
    }
}

pub(super) async fn track_state(service: std::sync::Weak<ApplicationService>) {
    let (window_sender, windows) = mpsc::channel(64);
    let events_task = tokio::spawn(shelllist_hyprland::watch_events_detailed(window_sender));
    let _events = shelllist_daemon_tokio::AbortOnDrop(events_task.abort_handle());
    let (catalog_sender, catalog) = mpsc::channel(64);
    let Some(initial) = service.upgrade() else {
        return;
    };
    let mut sources = EventSources {
        windows,
        catalog,
        catalog_sender,
        resumes: initial.resume_events.clone(),
    };
    let mut state = WatchState {
        catalog: WatchRecovery::new(Instant::now()),
        windows: WindowState {
            connection: Connection::Disconnected,
            available: false,
            refreshed: Instant::now(),
        },
        catalog_refreshed: Instant::now(),
    };
    state.windows.available = Refresh::Both
        .apply(
            &initial,
            &mut state.catalog_refreshed,
            &mut state.windows.refreshed,
        )
        .await
        .unwrap_or(false);
    drop(initial);
    while let Some(service) = service.upgrade() {
        let signal = sources.next(&state).await;
        if let Some(refresh) = state.react(signal, &mut sources).await {
            state.windows.available = refresh
                .apply(
                    &service,
                    &mut state.catalog_refreshed,
                    &mut state.windows.refreshed,
                )
                .await
                .unwrap_or(state.windows.available);
        }
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

async fn wait_for_watcher_retry(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests;
