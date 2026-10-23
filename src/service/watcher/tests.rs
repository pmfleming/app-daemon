use super::{WatchRecovery, reconciliation_intervals};
use std::time::{Duration, Instant};

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
    assert_eq!(
        reconciliation_intervals(false, false),
        (Duration::from_secs(5), Duration::from_secs(30))
    );
    assert_eq!(
        reconciliation_intervals(true, true),
        (Duration::from_secs(30), Duration::from_secs(300))
    );
}
