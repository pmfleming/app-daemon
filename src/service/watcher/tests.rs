use super::{Refresh, WatchRecovery, reconciliation_intervals, refresh_if};
use std::time::{Duration, Instant};

#[test]
fn retry_backoff_is_bounded_and_rebuild_and_success_reset_it() {
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
    watch.failed(now);
    assert_eq!(watch.retry_deadline(), Some(now + Duration::from_secs(1)));
    watch.rebuild(now);
    assert_eq!(watch.retry_deadline(), Some(now));
    assert!(watch.watcher.is_none());
    assert_eq!(
        reconciliation_intervals(false, false),
        (Duration::from_secs(5), Duration::from_secs(30))
    );
    assert_eq!(
        reconciliation_intervals(true, true),
        (Duration::from_secs(30), Duration::from_secs(300))
    );
}

#[tokio::test]
async fn refreshes_only_requested_sources_and_timestamps_completed_reads() {
    let old = Instant::now() - Duration::from_secs(1);
    for refresh in [Refresh::Windows, Refresh::Catalog, Refresh::Both] {
        let (mut catalog, mut windows) = (old, old);
        let catalog_reads = std::cell::Cell::new(0);
        let window_reads = std::cell::Cell::new(0);
        let (_, available) = tokio::join!(
            refresh_if(
                refresh != Refresh::Windows,
                async {
                    catalog_reads.set(1);
                },
                &mut catalog
            ),
            refresh_if(
                refresh != Refresh::Catalog,
                async {
                    window_reads.set(1);
                    false
                },
                &mut windows
            ),
        );
        assert_eq!(
            catalog_reads.get(),
            usize::from(refresh != Refresh::Windows)
        );
        assert_eq!(window_reads.get(), usize::from(refresh != Refresh::Catalog));
        assert_eq!(catalog > old, refresh != Refresh::Windows);
        assert_eq!(windows > old, refresh != Refresh::Catalog);
        assert_eq!(available, (refresh != Refresh::Catalog).then_some(false));
    }
    let mut timestamp = old;
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        refresh_if(false, std::future::pending::<()>(), &mut timestamp),
    )
    .await
    .unwrap();
    assert_eq!(result, None);
    assert_eq!(timestamp, old);
}
