use super::{Command, Duration, LaunchBackend, LaunchReceipt, checked_handoff_with_timeout};

#[test]
fn launch_isolation_recognizes_service_boundaries_and_exact_unit_ownership() {
    assert!(super::service_cgroup(
        "/user.slice/app-daemon.service/child"
    ));
    assert!(!super::service_cgroup("/session-1.scope"));
    let mut receipt = LaunchReceipt::from(LaunchBackend::Systemd);
    assert!(!receipt.owns_cgroup("/app-example@123.service"));
    receipt.unit = Some("app-example@123.service".into());
    assert!(receipt.owns_cgroup("/user.slice/app-example@123.service/child"));
    assert!(!receipt.owns_cgroup("/app-other@123.service"));
    assert!(!receipt.owns_cgroup("/app-example@123.service-extra"));
}

#[tokio::test]
async fn handoff_bounds_diagnostics_and_does_not_wait_for_descendant_stderr() {
    let mut command = Command::new("sh");
    // The descendant lasts 0.2s. The handoff must finish before its inherited
    // stderr closes, while also draining and bounding noisy launcher output.
    command.args(["-c", "i=0; while [ $i -lt 2048 ]; do printf 'diagnostic'; i=$((i+1)); done >&2; sleep 0.2 & exit 42"]);
    let started = std::time::Instant::now();
    let error = checked_handoff_with_timeout(command, "fixture", Duration::from_millis(100))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("42"), "{error}");
    assert!(error.contains("diagnostic"));
    assert!(
        error.len() < 10_000,
        "diagnostics must be bounded, not retained in full"
    );
    assert!(started.elapsed() < Duration::from_millis(180));
    assert!(
        checked_handoff_with_timeout(
            Command::new("/no/such/app-daemon-launcher"),
            "fixture",
            Duration::from_millis(100),
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("start fixture")
    );
}
