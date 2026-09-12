//! Resume notifications plus a suspend-aware fallback independent of wall time.
use futures::StreamExt;
use rustix::time::{ClockId, clock_gettime};
use std::time::Duration;
use tokio::{
    sync::{mpsc, watch},
    time::{MissedTickBehavior, interval, sleep},
};

#[derive(Debug, Default)]
pub(crate) struct ResumeClock {
    offset: Option<i128>,
}

impl ResumeClock {
    pub(crate) fn resumed(&mut self) -> bool {
        self.observe(suspend_offset())
    }

    fn observe(&mut self, offset: Option<i128>) -> bool {
        let Some(offset) = offset else {
            return false;
        };
        let changed = self
            .offset
            .is_some_and(|previous| offset - previous > 250_000_000);
        self.offset = Some(offset);
        changed
    }
}

fn suspend_offset() -> Option<i128> {
    fn ns(id: ClockId) -> i128 {
        let t = clock_gettime(id);
        i128::from(t.tv_sec) * 1_000_000_000 + i128::from(t.tv_nsec)
    }
    let before = ns(ClockId::Monotonic);
    let boot = ns(ClockId::Boottime);
    let after = ns(ClockId::Monotonic);
    (after - before < 50_000_000).then_some(boot - (before + after) / 2)
}

pub(crate) async fn monitor(sender: watch::Sender<u64>) {
    let (events, mut signals) = mpsc::channel(8);
    let task = tokio::spawn(logind_events(events));
    let _logind = crate::platform::AbortOnDrop(task.abort_handle());
    let mut poll = interval(Duration::from_secs(2));
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut clock = ResumeClock::default();
    let mut reported_before_signal = false;
    loop {
        let signal = tokio::select! {
            _ = sender.closed() => { task.abort(); return; },
            _ = poll.tick() => false,
            Some(()) = signals.recv() => true,
        };
        let changed = clock.resumed();
        let announce = if signal {
            let announce = changed || !reported_before_signal;
            reported_before_signal = false;
            announce
        } else {
            reported_before_signal |= changed;
            changed
        };
        if announce {
            sender.send_modify(|generation| *generation = generation.saturating_add(1));
        }
    }
}

async fn logind_events(sender: mpsc::Sender<()>) {
    while !sender.is_closed() {
        if let Err(error) = logind_connection(&sender).await {
            tracing::debug!(%error, "resume signal stream unavailable; suspend-aware clock fallback remains active");
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
    anyhow::bail!("logind resume signal stream ended")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn suspend_not_wall_clock_jumps_or_long_awake_stalls_changes_epoch() {
        let mut clock = ResumeClock::default();
        assert!(!clock.observe(Some(5_000_000_000))); // existing sleeps before startup
        assert!(!clock.observe(Some(5_000_000_000))); // wall-clock / awake stall not an input
        assert!(!clock.observe(None));
        assert!(clock.observe(Some(6_000_000_000))); // one-second suspend
        assert!(!clock.observe(Some(6_000_000_000)));
    }
}
