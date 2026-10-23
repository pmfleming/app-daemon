use super::{
    ApplicationService, Arc, Duration, HistoryStore, Ordering, now_milliseconds, persist_snapshot,
    time,
};
use crate::model::ResourceUsage;
use tokio::sync::oneshot;

#[tokio::test]
async fn shutdown_drains_sampling_and_serializes_final_save_after_older_writers()
-> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("history.json");
    let service = ApplicationService::build(false);
    *service.history.lock().await = HistoryStore::load(Some(path.clone()));
    let t = now_milliseconds() / 15_000 * 15_000 - 60_000;
    service
        .history
        .lock()
        .await
        .record("app", t, 1.0, &ResourceUsage::default());
    let old = service.history.lock().await.snapshot(true).unwrap();
    let old_writer = Arc::clone(&service.history_saves).lock_owned().await;
    let (sample_done, release_sample) = oneshot::channel();
    let sampler_service = Arc::clone(&service);
    let sampler = tokio::spawn(async move {
        let _ = release_sample.await;
        sampler_service.history.lock().await.record(
            "app",
            t + 15_000,
            1.0,
            &ResourceUsage::default(),
        );
    });
    service.background_tasks.lock().unwrap().push(sampler);
    let stopping = Arc::clone(&service);
    let shutdown = tokio::spawn(async move {
        stopping.shutdown().await;
    });
    time::timeout(Duration::from_secs(2), async {
        while !service.stopping.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(!shutdown.is_finished());
    sample_done.send(()).unwrap();
    // An older write finishes while shutdown is waiting for serialization.
    persist_snapshot(old)?;
    drop(old_writer);
    time::timeout(Duration::from_secs(2), shutdown).await??;
    let mut loaded = HistoryStore::load(Some(path));
    assert_eq!(
        loaded
            .query_window("app", None, None, None, 10)?
            .points
            .len(),
        2
    );
    assert!(service.request_permit().await.is_err());
    assert!(
        service
            .execute_owned(super::tests::close_missing(None), None)
            .await
            .is_err()
    );
    service.shutdown().await; // idempotent
    let api = crate::api::ApiService::new(service);
    assert_eq!(
        api.dispatch_owned("applications.query", serde_json::json!({}), None)
            .await["error"]["code"],
        "daemon-unavailable"
    );
    Ok(())
}

#[tokio::test]
async fn shutdown_rejects_prepared_actions_waiting_for_admission() {
    use futures::{pin_mut, poll};
    let service = ApplicationService::build(false);
    *service.history.lock().await = HistoryStore::load(None);
    let request = service.request_permit().await.unwrap();
    let mut events = service.subscribe_operations();
    let registry = service.operations.lock().await;
    let execute = service.execute_owned(super::tests::close_missing(None), None);
    let shutdown = service.shutdown();
    pin_mut!(execute, shutdown);
    assert!(poll!(&mut execute).is_pending());
    assert!(poll!(&mut shutdown).is_pending());
    assert!(service.stopping.load(Ordering::Acquire));
    drop(registry);
    assert!(
        execute
            .await
            .unwrap_err()
            .to_string()
            .contains("shutting down")
    );
    assert!(
        poll!(&mut shutdown).is_pending(),
        "an admitted request still owns its permit"
    );
    drop(request);
    shutdown.await;
    assert!(service.operations.lock().await.cancel_all().is_empty());
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}
