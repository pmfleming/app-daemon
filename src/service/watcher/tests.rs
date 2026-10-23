use super::{Connection, EventSources, Refresh, Signal, WatchRecovery, WatchState, WindowState};
use crate::service::catalog_watch::CatalogEvent;
use shelllist_hyprland::Event;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

#[test]
fn retry_backoff_survives_construction_success_and_rewatch_storms() {
    let mut now = Instant::now();
    let mut watch = WatchRecovery::<u8>::new(now);
    for seconds in [1, 2, 4, 8, 16, 30, 30] {
        assert!(watch.attempt(now, || Err("unavailable")).is_err());
        assert_eq!(
            watch.retry_deadline(),
            Some(now + Duration::from_secs(seconds))
        );
        assert!(
            watch
                .attempt::<&str>(now, || panic!("retry must not run early"))
                .is_ok()
        );
        now += Duration::from_secs(seconds);
    }
    watch.attempt(now, || Ok::<_, &str>(1)).unwrap();
    assert_eq!(watch.retry_deadline(), None);
    watch
        .attempt::<&str>(now, || panic!("healthy watcher must not be replaced"))
        .unwrap();
    watch.rebuild(now);
    assert_eq!(watch.retry_deadline(), Some(now + Duration::from_secs(30)));
    watch.rebuild(now + Duration::from_secs(1));
    assert_eq!(watch.retry_deadline(), Some(now + Duration::from_secs(30)));
    assert!(watch.watcher.is_none());
    now += Duration::from_secs(30);
    watch.attempt(now, || Ok::<_, &str>(1)).unwrap();
    now += Duration::from_secs(60);
    watch.rebuild(now);
    assert_eq!(watch.retry_deadline(), Some(now + Duration::from_secs(1)));
    watch.invalidate(CatalogEvent::Changed, now);
    watch.invalidate(CatalogEvent::Failed, now + Duration::from_millis(100));
    watch.repair::<&str>(now, || panic!("polling cannot bypass a pending retry"));
    assert_eq!(watch.retry_deadline(), Some(now + Duration::from_secs(1)));
}

fn state(now: Instant) -> WatchState {
    WatchState {
        windows: WindowState {
            connection: Connection::Disconnected,
            available: false,
            refreshed: now,
        },
        catalog: WatchRecovery::new(now),
        catalog_refreshed: now,
    }
}

#[test]
fn connection_transitions_require_both_events_and_snapshot_health() {
    let now = Instant::now();
    let mut state = state(now);
    assert_eq!(state.catalog_deadline(), now + Duration::from_secs(30));
    let windows = &mut state.windows;
    assert!(windows.observe(Some(Event::Connected)));
    assert_eq!(windows.deadline(), now + Duration::from_secs(5));
    windows.available = true;
    assert_eq!(windows.deadline(), now + Duration::from_secs(30));
    assert!(!windows.observe(Some(Event::Message("unrelated>>ignored".into()))));
    assert!(windows.observe(Some(Event::Message("openwindow>>abc".into()))));
    assert!(windows.observe(Some(Event::Disconnected(None))));
    assert_eq!(windows.deadline(), now + Duration::from_secs(5));
    assert!(windows.observe(Some(Event::ConnectFailed("unavailable".into()))));
    assert!(windows.observe(Some(Event::Connected)));
    assert!(!windows.observe(None));
    assert_eq!(windows.connection, Connection::Closed);
    assert_eq!(windows.deadline(), now + Duration::from_secs(5));
}

#[tokio::test]
async fn selected_batches_finish_before_polling_and_closed_streams_do_not_spin() {
    use futures::{pin_mut, poll};
    let now = Instant::now();
    let mut state = state(now);
    state
        .catalog
        .attempt(now, || notify::recommended_watcher(|_| {}))
        .unwrap();
    assert_eq!(state.catalog_deadline(), now + Duration::from_secs(300));
    let (window_sender, windows) = mpsc::channel(8);
    let (catalog_sender, catalog) = mpsc::channel(8);
    let (resume_sender, resumes) = watch::channel(0);
    let mut sources = EventSources {
        windows,
        catalog,
        catalog_sender,
        resumes,
    };
    window_sender.send(Event::Connected).await.unwrap();
    let signal = sources.next(&state).await;
    assert!(matches!(&signal, Signal::Window(Some(Event::Connected))));
    // These become ready during the selected debounce; neither may cancel it.
    resume_sender.send_replace(1);
    sources
        .catalog_sender
        .send(CatalogEvent::Failed)
        .await
        .unwrap();
    window_sender.send(Event::Disconnected(None)).await.unwrap();
    {
        let reacting = state.react(signal, &mut sources);
        pin_mut!(reacting);
        assert!(poll!(&mut reacting).is_pending());
        assert_eq!(reacting.await, Some(Refresh::Windows));
    }
    assert_eq!(state.windows.connection, Connection::Disconnected);
    assert_eq!(sources.catalog.len(), 1);
    assert!(sources.resumes.has_changed().unwrap());

    sources
        .catalog_sender
        .send(CatalogEvent::Changed)
        .await
        .unwrap();
    sources
        .catalog_sender
        .send(CatalogEvent::Rewatch)
        .await
        .unwrap();
    assert_eq!(
        state
            .react(Signal::Catalog(CatalogEvent::Changed), &mut sources)
            .await,
        Some(Refresh::Catalog)
    );
    assert!(state.catalog.watcher.is_none());
    let retry = state.catalog.retry_deadline();
    assert_eq!(
        state.react(Signal::Resume, &mut sources).await,
        Some(Refresh::Both)
    );
    assert_eq!(state.catalog.retry_deadline(), retry);

    drop(window_sender);
    drop(resume_sender);
    sources.resumes.borrow_and_update();
    assert_eq!(state.react(Signal::Window(None), &mut sources).await, None);
    // Keep all deadlines in the future: a closed channel must not wake the loop.
    state
        .catalog
        .recovery
        .failed(Instant::now() + Duration::from_secs(60));
    let waiting = sources.next(&state);
    pin_mut!(waiting);
    assert!(poll!(&mut waiting).is_pending());
}
