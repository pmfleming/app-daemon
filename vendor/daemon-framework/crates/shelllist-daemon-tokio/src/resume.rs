//! Suspend-aware resume detection. Wall-clock changes and awake stalls are not resumes.
use std::time::Duration;

use futures::StreamExt;
use rustix::time::{ClockId, clock_gettime};
use tokio::{
    sync::{mpsc, watch},
    time::{MissedTickBehavior, interval, sleep},
};

/// BOOTTIME includes suspend, MONOTONIC does not. Discard preempted samples.
#[must_use]
pub fn suspend_offset() -> Option<i128> {
    fn ns(id: ClockId) -> i128 {
        let t = clock_gettime(id);
        i128::from(t.tv_sec) * 1_000_000_000 + i128::from(t.tv_nsec)
    }
    let before = ns(ClockId::Monotonic);
    let boot = ns(ClockId::Boottime);
    let after = ns(ClockId::Monotonic);
    (after - before < 50_000_000).then_some(boot - (before + after) / 2)
}

#[derive(Debug, Default)]
pub struct ResumeDetector {
    offset: Option<i128>,
    reported_before_signal: bool,
}

impl ResumeDetector {
    fn changed(&mut self, offset: Option<i128>) -> bool {
        let Some(offset) = offset else {
            return false;
        };
        let changed = self
            .offset
            .is_some_and(|previous| offset - previous > 250_000_000);
        self.offset = Some(offset);
        changed
    }

    pub fn poll(&mut self, offset: Option<i128>) -> bool {
        let changed = self.changed(offset);
        self.reported_before_signal |= changed;
        changed
    }

    pub fn signal(&mut self, offset: Option<i128>) -> bool {
        let changed = self.changed(offset);
        let announce = changed || !self.reported_before_signal;
        self.reported_before_signal = false;
        announce
    }

    pub fn resumed(&mut self) -> bool {
        self.poll(suspend_offset())
    }
}

/// Publishes a monotonic generation; stops when the last consumer leaves.
pub async fn monitor_resumes(sender: watch::Sender<u64>) {
    let (events, mut signals) = mpsc::channel(8);
    let task = crate::spawn_named("logind-resumes", logind_events(events));
    let _logind = crate::AbortOnDrop(task.abort_handle());
    let mut poll = interval(Duration::from_secs(2));
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut detector = ResumeDetector::default();
    loop {
        let announce = tokio::select! {
            _ = sender.closed() => return,
            _ = poll.tick() => detector.poll(suspend_offset()),
            Some(()) = signals.recv() => detector.signal(suspend_offset()),
        };
        if announce {
            sender.send_modify(|generation| *generation = generation.saturating_add(1));
        }
    }
}

async fn logind_events(sender: mpsc::Sender<()>) {
    while !sender.is_closed() {
        tokio::select! {
            _ = sender.closed() => return,
            result = logind_connection(&sender) => {
                if let Err(error) = result {
                    tracing::debug!(%error, "logind resume stream unavailable; clock fallback remains active");
                }
            }
        }
        tokio::select! {
            _ = sender.closed() => return,
            _ = sleep(Duration::from_secs(3)) => {},
        }
    }
}

async fn logind_connection(sender: &mpsc::Sender<()>) -> anyhow::Result<()> {
    let connection = zbus::Connection::system().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    let mut signals = proxy.receive_signal("PrepareForSleep").await?;
    while let Some(signal) = signals.next().await {
        let (preparing,): (bool,) = signal.body().deserialize()?;
        if !preparing && sender.send(()).await.is_err() {
            return Ok(());
        }
    }
    anyhow::bail!("logind resume stream ended")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn short_sleep_signal_and_clock_fallback_are_deduplicated() {
        let mut detector = ResumeDetector::default();
        assert!(!detector.poll(Some(0)));
        assert!(detector.signal(Some(100_000_000)));
        assert!(!detector.poll(Some(100_000_000)));
        assert!(detector.poll(Some(5_100_000_000)));
        assert!(!detector.signal(Some(5_100_000_000)));
        assert!(detector.signal(Some(10_100_000_000)));
    }
    #[test]
    fn startup_stalls_and_wall_clock_changes_are_not_resumes() {
        let mut detector = ResumeDetector::default();
        assert!(!detector.poll(Some(123_000_000_000)));
        for _ in 0..10 {
            assert!(!detector.poll(Some(123_000_000_000)));
        }
        assert!(!detector.poll(None));
        assert!(suspend_offset().is_some());
    }
    #[tokio::test]
    async fn monitor_stops_without_consumers() {
        let (sender, receiver) = watch::channel(0);
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(1), monitor_resumes(sender))
            .await
            .unwrap();
    }
}
