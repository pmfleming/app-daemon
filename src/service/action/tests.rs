use super::*;

fn windows() -> Snapshot {
    Snapshot {
        available: true,
        revision: 1,
        clients: [1, 2, 3]
            .into_iter()
            .map(|pid| Client {
                address: format!("0x{pid}"),
                class: "app".into(),
                initial_class: "app".into(),
                title: String::new(),
                pid,
                workspace: Default::default(),
                focus_rank: 0,
                mapped: true,
            })
            .collect(),
    }
}

#[test]
fn placement_requires_new_address_and_exact_launch_unit() {
    let windows = windows();
    let previous = vec!["0x1".into()];
    let unit = "app-example@12345678.service";
    let cgroup = |window: &Client| {
        Some(if window.pid == 3 {
            "/app-unrelated.service".into()
        } else {
            format!("/user.slice/{unit}/child")
        })
    };
    assert_eq!(
        correlated_window(&windows, &previous, unit, cgroup).as_deref(),
        Some("0x2")
    );
    assert!(correlated_window(&windows, &previous, unit, |_| None).is_none());
    assert!(
        correlated_window(&windows, &previous, unit, |_| Some(format!("/{unit}"))).is_none(),
        "ambiguous launch must not move any window"
    );
    assert!(correlated_window(&windows, &previous, "app-other@12345678.service", cgroup).is_none());
}

#[test]
fn application_units_are_unique_and_keep_desktop_identity() {
    let first = launch::application_unit("org.example.App.desktop");
    let second = launch::application_unit("org.example.App.desktop");
    assert_ne!(first, second);
    assert!(first.starts_with("app-org.example.App@"));
    assert!(first.ends_with(".service"));
    assert!(!launch::application_unit("a/b.desktop").contains('/'));
}

#[tokio::test]
async fn launches_for_one_target_share_a_cancellation_safe_lock() {
    let service = crate::service::ApplicationService::new();
    let first = service.launch_lock("app.desktop");
    let second = service.launch_lock("app.desktop");
    let other = service.launch_lock("other.desktop");
    assert!(std::sync::Arc::ptr_eq(&first, &second));
    let guard = first.lock().await;
    assert!(second.try_lock().is_err());
    assert!(other.try_lock().is_ok());
    drop(guard);
    assert!(second.try_lock().is_ok());
}
