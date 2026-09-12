use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use shelllist_daemon_core::{ClientRequest, DaemonEndpoint};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, Semaphore, watch};
use tokio::task::{JoinHandle, JoinSet};

use crate::JsonDbusClient;
use crate::output_actor::{CorrelationPolicy, OutputCommand, OutputHandle, spawn_output_actor};

const OUTPUT_CAPACITY: usize = 64;
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(250);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelMode {
    Json,
    Unit,
}

pub enum CallFailure {
    Api(Value),
    Transport(String),
}

pub type CallFailureMapper = fn(&str, &anyhow::Error) -> CallFailure;

pub struct JsonlClientConfig<P> {
    pub endpoint: DaemonEndpoint,
    pub correlation: P,
    pub cancel_mode: CancelMode,
    pub call_failure: CallFailureMapper,
    pub pending_event_limit: usize,
    pub max_in_flight_requests: usize,
    /// `None` drains every accepted request; a duration bounds graceful drain.
    pub shutdown_timeout: Option<Duration>,
}

#[derive(Clone)]
struct ReconnectingClient {
    endpoint: DaemonEndpoint,
    current: Arc<Mutex<Option<JsonDbusClient>>>,
    event_ready: watch::Sender<bool>,
}

impl ReconnectingClient {
    fn new(endpoint: DaemonEndpoint) -> Self {
        let (event_ready, _) = watch::channel(false);
        Self {
            endpoint,
            current: Arc::new(Mutex::new(None)),
            event_ready,
        }
    }

    async fn get(&self) -> Result<JsonDbusClient> {
        let mut current = self.current.lock().await;
        if let Some(client) = current.as_ref() {
            return Ok(client.clone());
        }
        let client = JsonDbusClient::session(self.endpoint).await?;
        *current = Some(client.clone());
        Ok(client)
    }

    async fn invalidate(&self) {
        self.event_ready.send_replace(false);
        self.current.lock().await.take();
    }

    async fn wait_for_event_forwarding(&self) -> Result<()> {
        let mut ready = self.event_ready.subscribe();
        while !*ready.borrow_and_update() {
            ready
                .changed()
                .await
                .context("daemon event readiness channel closed")?;
        }
        Ok(())
    }
}

pub async fn run_jsonl_client<P: CorrelationPolicy>(config: JsonlClientConfig<P>) -> Result<()> {
    let dbus = ReconnectingClient::new(config.endpoint);
    let (output, output_task) = spawn_output_actor(
        config.correlation,
        OUTPUT_CAPACITY,
        config.pending_event_limit,
    );
    let event_task = spawn_event_forwarder(dbus.clone(), output.clone());
    let owner_task = spawn_owner_watcher(dbus.clone(), output.clone());

    let mut calls = JoinSet::new();
    let request_slots = Arc::new(Semaphore::new(config.max_in_flight_requests.max(1)));
    let shutdown_id = request_loop(
        &dbus,
        &output,
        &mut calls,
        config.cancel_mode,
        config.call_failure,
        request_slots,
    )
    .await?;
    drain_calls(&mut calls, config.shutdown_timeout).await;
    cancel_active(&dbus, &output, config.cancel_mode).await;

    event_task.abort();
    owner_task.abort();
    if let Some(id) = shutdown_id {
        output.send(OutputCommand::Shutdown(id)).await?;
    }
    drop(output);
    output_task
        .await
        .context("join JSONL output task")?
        .context("run JSONL output task")
}

async fn request_loop(
    dbus: &ReconnectingClient,
    output: &OutputHandle,
    calls: &mut JoinSet<()>,
    cancel_mode: CancelMode,
    call_failure: CallFailureMapper,
    request_slots: Arc<Semaphore>,
) -> Result<Option<String>> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await.context("read JSONL request")? {
        let Some(request) = parse_request(&line, output).await? else {
            continue;
        };
        if let ClientRequest::Shutdown { id } = request {
            return Ok(Some(id));
        }
        spawn_request(
            calls,
            dbus.clone(),
            output.clone(),
            request,
            cancel_mode,
            call_failure,
            Arc::clone(&request_slots),
        );
        reap_finished(calls);
    }
    Ok(None)
}

async fn parse_request(line: &str, output: &OutputHandle) -> Result<Option<ClientRequest>> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    match serde_json::from_str(line) {
        Ok(request) => Ok(Some(request)),
        Err(error) => {
            output
                .send(OutputCommand::ProtocolError(error.to_string()))
                .await?;
            Ok(None)
        }
    }
}

fn spawn_request(
    calls: &mut JoinSet<()>,
    dbus: ReconnectingClient,
    output: OutputHandle,
    request: ClientRequest,
    cancel_mode: CancelMode,
    call_failure: CallFailureMapper,
    request_slots: Arc<Semaphore>,
) {
    calls.spawn(async move {
        // Cancellation is control-plane traffic: it must not wait behind calls it
        // is intended to cancel. Ordinary calls and subscriptions remain bounded.
        let _permit = match &request {
            ClientRequest::Call { .. } | ClientRequest::Subscribe { .. } => {
                request_slots.acquire_owned().await.ok()
            }
            ClientRequest::Cancel { .. } | ClientRequest::Shutdown { .. } => None,
        };
        let command = execute_request(dbus, &output, request, cancel_mode, call_failure).await;
        let _ = output.send(command).await;
    });
}

async fn execute_request(
    dbus: ReconnectingClient,
    output: &OutputHandle,
    request: ClientRequest,
    cancel_mode: CancelMode,
    call_failure: CallFailureMapper,
) -> OutputCommand {
    match request {
        ClientRequest::Call { id, method, params } => {
            let result = with_transport_timeout(output, REQUEST_TIMEOUT, async {
                dbus.get().await?.call(&method, params).await
            })
            .await
            .map_err(|error| call_failure(&method, &error))
            .or_else(|failure| match failure {
                CallFailure::Api(response) => Ok(response),
                CallFailure::Transport(error) => Err(error),
            });
            response_command(id, result, None)
        }
        ClientRequest::Subscribe { id, streams } => {
            let result = with_transport_timeout(output, REQUEST_TIMEOUT, async {
                dbus.wait_for_event_forwarding().await?;
                dbus.get().await?.subscribe(streams).await
            })
            .await
            .map_err(|error| error.to_string());
            response_command(id, result, None)
        }
        ClientRequest::Cancel { id, request_id } => {
            let result = with_transport_timeout(output, CONTROL_TIMEOUT, async {
                let client = dbus.get().await?;
                cancel(&client, &request_id, cancel_mode).await
            })
            .await
            .map_err(|error| error.to_string());
            let cancelled = result.as_ref().ok().map(|_| request_id);
            response_command(id, result, cancelled)
        }
        ClientRequest::Shutdown { id } => OutputCommand::Shutdown(id),
    }
}

async fn with_transport_timeout<T>(
    output: &OutputHandle,
    timeout: Duration,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(timeout, future).await {
        Ok(result) => result,
        Err(_) => {
            let message = format!("daemon request timed out after {}ms", timeout.as_millis());
            // The frontend restarts the bridge on transport-error. Keep this
            // generation intact until then: replacing just the cached client
            // strands the event reader on its old connection and leaves new
            // subscriptions waiting for readiness that can never arrive.
            output
                .send(OutputCommand::TransportError(message.clone()))
                .await?;
            anyhow::bail!(message)
        }
    }
}

fn response_command(
    id: String,
    result: std::result::Result<Value, String>,
    cancelled_request_id: Option<String>,
) -> OutputCommand {
    OutputCommand::Response {
        id,
        result,
        cancelled_request_id,
    }
}

async fn cancel(dbus: &JsonDbusClient, request_id: &str, mode: CancelMode) -> Result<Value> {
    match mode {
        CancelMode::Json => dbus.cancel_json(request_id).await,
        CancelMode::Unit => dbus.cancel_unit(request_id).await,
    }
}

fn spawn_event_forwarder(dbus: ReconnectingClient, output: OutputHandle) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut delay = INITIAL_RECONNECT_DELAY;
        let mut last_error = None;
        loop {
            dbus.event_ready.send_replace(false);
            let message = event_forwarding_error(&dbus, &output).await;
            if output.send(OutputCommand::ResetCorrelation).await.is_err()
                || !report_transport_error(&output, &mut last_error, message).await
            {
                return;
            }
            dbus.invalidate().await;
            tokio::time::sleep(delay).await;
            delay = delay.saturating_mul(2).min(MAX_RECONNECT_DELAY);
        }
    })
}

async fn event_forwarding_error(dbus: &ReconnectingClient, output: &OutputHandle) -> String {
    let result = async {
        dbus.get()
            .await?
            .forward_events(output, &dbus.event_ready)
            .await
    }
    .await;
    result.map_or_else(
        |error| error.to_string(),
        |()| "daemon event forwarding stopped".to_owned(),
    )
}

async fn report_transport_error(
    output: &OutputHandle,
    last_error: &mut Option<String>,
    message: String,
) -> bool {
    if last_error.as_deref() == Some(message.as_str()) {
        return true;
    }
    let sent = output
        .send(OutputCommand::TransportError(message.clone()))
        .await
        .is_ok();
    *last_error = Some(message);
    sent
}

fn spawn_owner_watcher(dbus: ReconnectingClient, output: OutputHandle) -> JoinHandle<()> {
    tokio::spawn(async move { while watch_owner_once(&dbus, &output).await {} })
}

async fn watch_owner_once(dbus: &ReconnectingClient, output: &OutputHandle) -> bool {
    let result = async { dbus.get().await?.watch_replacement().await }.await;
    match result {
        Ok(()) => {
            dbus.invalidate().await;
            if output.send(OutputCommand::ResetCorrelation).await.is_err() {
                return false;
            }
            output
                .send(OutputCommand::TransportError(format!(
                    "{} D-Bus owner changed",
                    dbus.endpoint.executable
                )))
                .await
                .is_ok()
        }
        Err(error) => {
            tracing::warn!(%error, "daemon owner watcher stopped");
            tokio::time::sleep(INITIAL_RECONNECT_DELAY).await;
            true
        }
    }
}

fn reap_finished(calls: &mut JoinSet<()>) {
    while let Some(result) = calls.try_join_next() {
        log_join_result(result);
    }
}

fn log_join_result(result: std::result::Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        tracing::warn!(%error, "daemon JSONL call task failed");
    }
}

async fn drain_calls(calls: &mut JoinSet<()>, timeout: Option<Duration>) {
    let Some(timeout) = timeout else {
        drain_all(calls).await;
        return;
    };
    if tokio::time::timeout(timeout, drain_all(calls))
        .await
        .is_ok()
    {
        return;
    }
    calls.abort_all();
    while calls.join_next().await.is_some() {}
}

async fn drain_all(calls: &mut JoinSet<()>) {
    while let Some(result) = calls.join_next().await {
        log_join_result(result);
    }
}

async fn cancel_active(dbus: &ReconnectingClient, output: &OutputHandle, mode: CancelMode) {
    let Ok(client) = dbus.get().await else {
        return;
    };
    for id in output.active_ids().await {
        let _ = cancel(&client, &id, mode).await;
    }
}

#[cfg(test)]
mod tests {
    use std::{future::pending, time::Duration};

    use anyhow::Result;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, BufReader, duplex};

    use super::{response_command, with_transport_timeout};
    use crate::output_actor::{BasicCorrelation, OutputCommand, spawn_output_actor_with_writer};

    #[tokio::test]
    async fn timeout_reports_recovery_before_the_correlated_response() -> Result<()> {
        let (writer, reader) = duplex(4096);
        let (output, task) = spawn_output_actor_with_writer(BasicCorrelation, 8, 8, writer);
        let mut lines = BufReader::new(reader).lines();

        // The domain mapper may turn the error into a regular API response. The
        // frontend must still receive an independent transport recovery signal.
        let error =
            with_transport_timeout(&output, Duration::from_millis(1), pending::<Result<()>>())
                .await
                .expect_err("request must time out");
        output
            .send(response_command(
                "slow-call".into(),
                Ok(json!({ "ok": false, "error": { "code": "daemon-unavailable" } })),
                None,
            ))
            .await?;
        drop(output);
        task.await??;

        let recovery: Value = serde_json::from_str(&lines.next_line().await?.unwrap())?;
        assert_eq!(recovery["kind"], "transport-error");
        assert_eq!(recovery["error"], error.to_string());
        let response: Value = serde_json::from_str(&lines.next_line().await?.unwrap())?;
        assert_eq!(response["id"], "slow-call");
        assert_eq!(response["kind"], "response");
        assert!(lines.next_line().await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn completed_requests_do_not_trigger_transport_recovery() -> Result<()> {
        let (writer, reader) = duplex(4096);
        let (output, task) = spawn_output_actor_with_writer(BasicCorrelation, 8, 8, writer);
        let mut lines = BufReader::new(reader).lines();
        assert_eq!(
            with_transport_timeout(&output, Duration::from_secs(1), async { Ok(42) }).await?,
            42
        );
        let error = with_transport_timeout::<()>(&output, Duration::from_secs(1), async {
            anyhow::bail!("ordinary request failure")
        })
        .await
        .expect_err("request returns its error");
        assert_eq!(error.to_string(), "ordinary request failure");
        output.send(OutputCommand::Shutdown("done".into())).await?;
        drop(output);
        task.await??;
        let shutdown: Value = serde_json::from_str(&lines.next_line().await?.unwrap())?;
        assert_eq!(shutdown["id"], "done");
        assert!(lines.next_line().await?.is_none());
        Ok(())
    }
}
