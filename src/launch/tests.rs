use super::*;

fn shell(script: &str) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", script]);
    command
}

#[tokio::test]
async fn launcher_exit_status_and_diagnostics_are_reported() {
    checked_handoff(shell("exit 0"), "fixture").await.unwrap();
    let failure = checked_handoff(shell("printf 'fixture rejected' >&2; exit 42"), "fixture")
        .await
        .unwrap_err()
        .to_string();
    assert!(failure.contains("42"), "{failure}");
    assert!(failure.contains("fixture rejected"), "{failure}");
    let failure = checked_handoff(Command::new("/no/such/app-daemon-launcher"), "fixture")
        .await
        .unwrap_err()
        .to_string();
    assert!(failure.contains("start fixture"));
}

#[tokio::test]
async fn stuck_launchers_are_bounded_and_diagnostics_are_capped() {
    let started = std::time::Instant::now();
    let failure =
        checked_handoff_with_timeout(shell("exec sleep 10"), "fixture", Duration::from_millis(50))
            .await
            .unwrap_err()
            .to_string();
    assert!(failure.contains("timed out"), "{failure}");
    assert!(started.elapsed() < Duration::from_secs(2));
    let mut detail = Vec::new();
    capture_diagnostic(&mut detail, &vec![b'x'; 16_384]);
    capture_diagnostic(&mut detail, b"more");
    assert_eq!(detail.len(), 8192);
}

#[tokio::test]
async fn handoff_does_not_wait_for_inherited_stderr_to_close() -> anyhow::Result<()> {
    // The fixture descendant lasts only 0.2 seconds, avoiding persistent children.
    let started = std::time::Instant::now();
    checked_handoff_with_timeout(
        shell("sleep 0.2 & exit 0"),
        "fixture",
        Duration::from_millis(100),
    )
    .await?;
    assert!(started.elapsed() < Duration::from_millis(180));
    Ok(())
}
