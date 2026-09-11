//! Watchers are an optimization, never a prerequisite for reconciliation.
use std::time::{Duration, Instant};

const MIN_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(30);

pub(super) struct WatchRecovery<W> {
    watcher: Option<W>,
    retry_at: Instant,
    retry_delay: Duration,
}

impl<W> WatchRecovery<W> {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            watcher: None,
            retry_at: now,
            retry_delay: MIN_RETRY,
        }
    }

    pub(super) fn rebuild(&mut self, now: Instant) {
        self.watcher = None;
        self.retry_at = now;
        self.retry_delay = MIN_RETRY;
    }

    pub(super) fn failed(&mut self, now: Instant) {
        self.watcher = None;
        self.retry_at = now + self.retry_delay;
        self.retry_delay = (self.retry_delay * 2).min(MAX_RETRY);
    }

    pub(super) fn attempt<E>(
        &mut self,
        now: Instant,
        create: impl FnOnce() -> Result<W, E>,
    ) -> Result<(), E> {
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

    pub(super) fn retry_deadline(&self) -> Option<Instant> {
        self.watcher.is_none().then_some(self.retry_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
