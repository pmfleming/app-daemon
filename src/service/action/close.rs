//! Close dispatch and observation are different outcomes. Observe the captured
//! compositor identities, never the client's filtered/paginated application list.
use crate::{
    hyprland::{self, Client, Snapshot},
    model::CloseObservation,
};
use std::{collections::HashSet, future::Future, time::Duration};

pub(super) trait Backend {
    async fn close(&self, address: &str) -> anyhow::Result<()>;
    async fn windows(&self) -> Snapshot;
}
pub(super) struct Compositor;
impl Backend for Compositor {
    async fn close(&self, address: &str) -> anyhow::Result<()> {
        hyprland::close(address).await
    }
    async fn windows(&self) -> Snapshot {
        Snapshot::load().await
    }
}

pub(super) async fn observe<B, P, F>(
    backend: &B,
    targets: &[&Client],
    mut publish: P,
    budget: Duration,
) -> CloseObservation
where
    B: Backend,
    P: FnMut(CloseObservation) -> F,
    F: Future<Output = ()>,
{
    let mut result = CloseObservation {
        targeted_window_ids: targets
            .iter()
            .map(|v| hyprland::window_id(&v.address))
            .collect(),
        status: "dispatching".into(),
        ..Default::default()
    };
    result.remaining_window_ids = result.targeted_window_ids.clone();
    // Admission/dispatch state is recoverable even if cancellation interrupts IPC.
    publish(result.clone()).await;
    for target in targets {
        if let Err(error) = backend.close(&target.address).await {
            result.dispatch_error = Some(error.to_string());
            break;
        }
        result
            .dispatched_window_ids
            .push(hyprland::window_id(&target.address));
        publish(result.clone()).await;
    }
    result.status = "observing".into();
    publish(result.clone()).await;
    let deadline = tokio::time::Instant::now() + budget;
    result.status = loop {
        let snapshot = backend.windows().await;
        if !snapshot.available {
            break "unknown";
        }
        result.remaining_window_ids = remaining_windows(targets, &snapshot);
        if result.remaining_window_ids.is_empty() {
            break "closed";
        }
        if tokio::time::Instant::now() >= deadline {
            break "still-open";
        }
        publish(result.clone()).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    .into();
    result
}

fn remaining_windows(targets: &[&Client], snapshot: &Snapshot) -> Vec<String> {
    let live = snapshot
        .clients
        .iter()
        .map(|window| (window.address.as_str(), window.pid))
        .collect::<HashSet<_>>();
    targets
        .iter()
        .filter(|target| live.contains(&(target.address.as_str(), target.pid)))
        .map(|v| hyprland::window_id(&v.address))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Backend, Client, Duration, Snapshot, observe};
    struct Fake {
        remaining: Vec<Client>,
        available: bool,
        fail: bool,
    }
    impl Backend for Fake {
        async fn close(&self, _: &str) -> anyhow::Result<()> {
            anyhow::ensure!(!self.fail, "IPC rejected");
            Ok(())
        }
        async fn windows(&self) -> Snapshot {
            Snapshot {
                available: self.available,
                clients: self.remaining.clone(),
                ..Default::default()
            }
        }
    }
    fn window(address: &str, pid: u32) -> Client {
        serde_json::from_value(serde_json::json!({"address": address, "pid": pid})).unwrap()
    }
    #[tokio::test]
    async fn observes_only_captured_targets_and_never_confuses_unavailable_with_closed() {
        let targets = [window("0xabc", 1), window("0xdef", 2)];
        for (remaining, available, fail, expected, remaining_ids) in [
            (vec![window("0xaaa", 2)], true, false, "closed", vec![]),
            (vec![window("0xabc", 2)], true, false, "closed", vec![]),
            (
                vec![window("0xdef", 2), window("0xdef", 2)],
                true,
                false,
                "still-open",
                vec!["window-def"],
            ),
            (
                targets.to_vec(),
                true,
                false,
                "still-open",
                vec!["window-abc", "window-def"],
            ),
            (
                vec![],
                false,
                false,
                "unknown",
                vec!["window-abc", "window-def"],
            ),
            (
                targets.to_vec(),
                true,
                true,
                "still-open",
                vec!["window-abc", "window-def"],
            ),
        ] {
            let result = observe(
                &Fake {
                    remaining,
                    available,
                    fail,
                },
                &targets.iter().collect::<Vec<_>>(),
                |_| async {},
                Duration::ZERO,
            )
            .await;
            assert_eq!(result.status, expected);
            assert_eq!(result.targeted_window_ids, ["window-abc", "window-def"]);
            assert_eq!(result.remaining_window_ids, remaining_ids);
            assert_eq!(result.dispatch_error.is_some(), fail);
            assert_eq!(result.dispatched_window_ids.is_empty(), fail);
        }
    }
}
