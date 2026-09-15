//! Offload durable filesystem work; cancellation does not interrupt a commit.
use anyhow::{Context, Result};
use shelllist_daemon_core::AtomicFilePolicy;
use std::path::PathBuf;

pub async fn write_bytes_atomic_async(
    path: PathBuf,
    contents: Vec<u8>,
    policy: AtomicFilePolicy,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        shelllist_daemon_core::write_bytes_atomic(&path, &contents, policy)
    })
    .await
    .context("join atomic state write")?
    .context("write atomic state")
}

pub async fn read_bytes_bounded_async(path: PathBuf, max_bytes: u64) -> Result<Option<Vec<u8>>> {
    tokio::task::spawn_blocking(move || shelllist_daemon_core::read_bytes_bounded(&path, max_bytes))
        .await
        .context("join bounded state read")?
        .context("read bounded state")
}
