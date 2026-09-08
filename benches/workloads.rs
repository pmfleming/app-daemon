fn main() -> anyhow::Result<()> {
    let iterations = std::env::var("APP_DAEMON_BENCH_ITERS")
        .ok()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(500);
    let report = app_daemon::benchmarks::run(iterations)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
