use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use shelllist_daemon_core::{
    ClientRoute, addressed_message, event_message, protocol_error_message, response_error_message,
    response_message, shutdown_message, transport_error_message,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackedKind {
    Operation,
    Subscription,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedId {
    pub id: String,
    pub kind: TrackedKind,
}

pub trait CorrelationPolicy: Send + Sync + 'static {
    fn response_id(&self, response: &Value) -> Option<TrackedId>;
    fn event_id(&self, stream: &str, event: &Value) -> Option<String>;
    fn is_terminal(&self, stream: &str, event: &Value) -> bool;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BasicCorrelation;

impl CorrelationPolicy for BasicCorrelation {
    fn response_id(&self, response: &Value) -> Option<TrackedId> {
        response
            .pointer("/data/subscription/id")
            .and_then(Value::as_str)
            .map(|id| TrackedId {
                id: id.to_owned(),
                kind: TrackedKind::Subscription,
            })
    }

    fn event_id(&self, _stream: &str, event: &Value) -> Option<String> {
        event
            .get("subscription_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    fn is_terminal(&self, _stream: &str, _event: &Value) -> bool {
        false
    }
}

pub(crate) enum OutputCommand {
    Response {
        id: String,
        result: std::result::Result<Value, String>,
        cancelled_request_id: Option<String>,
        route: Option<ClientRoute>,
    },
    OwnedIds {
        route: ClientRoute,
        reply: oneshot::Sender<Vec<String>>,
    },
    Cancelled(String),
    Event {
        stream: String,
        event: Value,
    },
    ProtocolError(String),
    TransportError(String),
    ActiveIds(oneshot::Sender<Vec<String>>),
    ResetCorrelation,
    Shutdown(String),
}

#[derive(Clone)]
pub(crate) struct OutputHandle {
    priority_sender: mpsc::Sender<OutputCommand>,
    event_sender: mpsc::Sender<OutputCommand>,
}

impl OutputHandle {
    pub(crate) async fn send(&self, command: OutputCommand) -> Result<()> {
        let sender = if matches!(command, OutputCommand::Event { .. }) {
            &self.event_sender
        } else {
            &self.priority_sender
        };
        sender
            .send(command)
            .await
            .context("send daemon output command")
    }

    pub(crate) async fn owned_ids(&self, route: ClientRoute) -> Vec<String> {
        let (reply, response) = oneshot::channel();
        if self
            .send(OutputCommand::OwnedIds { route, reply })
            .await
            .is_err()
        {
            return Vec::new();
        }
        response.await.unwrap_or_default()
    }

    pub(crate) async fn active_ids(&self) -> Vec<String> {
        let (reply, response) = oneshot::channel();
        if self.send(OutputCommand::ActiveIds(reply)).await.is_err() {
            return Vec::new();
        }
        response.await.unwrap_or_default()
    }
}

enum EventDisposition {
    Emit,
    Buffer(String),
    Drop,
}

struct OutputState<P> {
    policy: P,
    active_ids: HashSet<String>,
    // Only live subscriptions consume routing storage. Ordinary request
    // addresses travel with their task/response, not a long-lived dictionary.
    subscription_routes: HashMap<String, ClientRoute>,
    pending_events: VecDeque<(String, String, Value)>,
    suppressed_ids: HashSet<String>,
    suppressed_order: VecDeque<String>,
    pending_limit: usize,
}

impl<P: CorrelationPolicy> OutputState<P> {
    fn new(policy: P, pending_limit: usize) -> Self {
        Self {
            policy,
            active_ids: HashSet::new(),
            subscription_routes: HashMap::new(),
            pending_events: VecDeque::new(),
            suppressed_ids: HashSet::new(),
            suppressed_order: VecDeque::new(),
            pending_limit,
        }
    }

    fn activate(&mut self, response: &Value, route: Option<&ClientRoute>) -> Vec<(String, Value)> {
        let Some(tracked) = self.policy.response_id(response) else {
            return Vec::new();
        };
        if self.suppressed_ids.contains(&tracked.id) {
            return Vec::new();
        }
        if tracked.kind == TrackedKind::Subscription
            && let Some(route) = route
        {
            self.subscription_routes
                .insert(tracked.id.clone(), route.clone());
        }
        let pending = self.take_pending(&tracked.id);
        self.active_ids.insert(tracked.id);
        pending
    }

    fn event_disposition(&self, stream: &str, event: &Value) -> EventDisposition {
        let Some(id) = self.policy.event_id(stream, event) else {
            return EventDisposition::Emit;
        };
        if self.suppressed_ids.contains(&id) {
            EventDisposition::Drop
        } else if self.active_ids.contains(&id) {
            EventDisposition::Emit
        } else {
            EventDisposition::Buffer(id)
        }
    }

    fn buffer(&mut self, id: String, stream: String, event: Value) {
        if self.pending_limit == 0 {
            return;
        }
        if self.pending_events.len() >= self.pending_limit {
            self.pending_events.pop_front();
        }
        self.pending_events.push_back((id, stream, event));
    }

    fn take_pending(&mut self, id: &str) -> Vec<(String, Value)> {
        let (matching, retained) = std::mem::take(&mut self.pending_events)
            .into_iter()
            .partition(|(event_id, _, _)| event_id == id);
        self.pending_events = retained;
        matching
            .into_iter()
            .map(|(_, stream, event)| (stream, event))
            .collect()
    }

    fn suppress(&mut self, id: String) {
        if self.pending_limit == 0 || !self.suppressed_ids.insert(id.clone()) {
            return;
        }
        if self.suppressed_order.len() >= self.pending_limit
            && let Some(oldest) = self.suppressed_order.pop_front()
        {
            self.suppressed_ids.remove(&oldest);
        }
        self.suppressed_order.push_back(id);
    }

    fn cancelled(&mut self, id: &str) {
        self.active_ids.remove(id);
        self.subscription_routes.remove(id);
        self.take_pending(id);
        self.suppress(id.to_owned());
    }

    fn emitted(&mut self, stream: &str, event: &Value) {
        if self.policy.is_terminal(stream, event)
            && let Some(id) = self.policy.event_id(stream, event)
        {
            self.active_ids.remove(&id);
            self.subscription_routes.remove(&id);
            self.take_pending(&id);
            self.suppress(id);
        }
    }

    fn active_ids(&self) -> Vec<String> {
        let mut ids = self.active_ids.iter().cloned().collect::<Vec<_>>();
        ids.sort();
        ids
    }

    fn owned_ids(&self, owner: &ClientRoute) -> Vec<String> {
        self.subscription_routes
            .iter()
            .filter(|(_, route)| {
                route.consumer_id == owner.consumer_id && route.generation == owner.generation
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn event_message(&self, stream: &str, event: &Value) -> Value {
        // Subscription ownership is independent of domain operation correlation.
        let route = event
            .get("subscription_id")
            .and_then(Value::as_str)
            .and_then(|id| self.subscription_routes.get(id));
        addressed_message(event_message(stream, event.clone()), route)
    }

    fn reset_correlation(&mut self) {
        self.active_ids.clear();
        self.subscription_routes.clear();
        self.pending_events.clear();
        self.suppressed_ids.clear();
        self.suppressed_order.clear();
    }
}

#[must_use]
pub(crate) fn spawn_output_actor<P: CorrelationPolicy>(
    policy: P,
    capacity: usize,
    pending_limit: usize,
) -> (OutputHandle, JoinHandle<Result<()>>) {
    spawn_output_actor_with_writer(policy, capacity, pending_limit, tokio::io::stdout())
}

#[must_use]
pub(crate) fn spawn_output_actor_with_writer<P, W>(
    policy: P,
    capacity: usize,
    pending_limit: usize,
    writer: W,
) -> (OutputHandle, JoinHandle<Result<()>>)
where
    P: CorrelationPolicy,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (priority_sender, priority_receiver) = mpsc::channel(capacity);
    let (event_sender, event_receiver) = mpsc::channel(capacity);
    let handle = OutputHandle {
        priority_sender,
        event_sender,
    };
    let task = tokio::spawn(run_output_actor(
        priority_receiver,
        event_receiver,
        writer,
        OutputState::new(policy, pending_limit),
    ));
    (handle, task)
}

async fn run_output_actor<P, W>(
    mut priority_commands: mpsc::Receiver<OutputCommand>,
    mut events: mpsc::Receiver<OutputCommand>,
    mut writer: W,
    mut state: OutputState<P>,
) -> Result<()>
where
    P: CorrelationPolicy,
    W: AsyncWrite + Unpin,
{
    loop {
        tokio::select! {
            biased;
            Some(command) = priority_commands.recv() => {
                emit_command(&mut writer, &mut state, command).await?;
            }
            Some(command) = events.recv() => {
                emit_command(&mut writer, &mut state, command).await?;
            }
            else => return Ok(()),
        }
    }
}

async fn emit_command<P, W>(
    writer: &mut W,
    state: &mut OutputState<P>,
    command: OutputCommand,
) -> Result<()>
where
    P: CorrelationPolicy,
    W: AsyncWrite + Unpin,
{
    match command {
        OutputCommand::Response {
            id,
            result,
            cancelled_request_id,
            route,
        } => {
            emit_response(
                writer,
                state,
                id,
                result,
                cancelled_request_id,
                route.as_ref(),
            )
            .await
        }
        OutputCommand::OwnedIds { route, reply } => {
            let _ = reply.send(state.owned_ids(&route));
            Ok(())
        }
        OutputCommand::Cancelled(id) => {
            state.cancelled(&id);
            Ok(())
        }
        OutputCommand::Event { stream, event } => emit_event(writer, state, stream, event).await,
        OutputCommand::ProtocolError(error) => {
            emit_line(writer, &protocol_error_message(error)).await
        }
        OutputCommand::TransportError(error) => {
            emit_line(writer, &transport_error_message(error)).await
        }
        OutputCommand::ActiveIds(reply) => {
            let _ = reply.send(state.active_ids());
            Ok(())
        }
        OutputCommand::ResetCorrelation => {
            state.reset_correlation();
            Ok(())
        }
        OutputCommand::Shutdown(id) => emit_line(writer, &shutdown_message(&id)).await,
    }
}

async fn emit_response<P, W>(
    writer: &mut W,
    state: &mut OutputState<P>,
    id: String,
    result: std::result::Result<Value, String>,
    cancelled_request_id: Option<String>,
    route: Option<&ClientRoute>,
) -> Result<()>
where
    P: CorrelationPolicy,
    W: AsyncWrite + Unpin,
{
    if let Some(cancelled) = cancelled_request_id {
        state.cancelled(&cancelled);
    }
    let (line, pending) = match result {
        Ok(response) => {
            let pending = state.activate(&response, route);
            (response_message(&id, response), pending)
        }
        Err(error) => (response_error_message(&id, error), Vec::new()),
    };
    emit_line(writer, &addressed_message(line, route)).await?;
    for (stream, event) in pending {
        let message = state.event_message(&stream, &event);
        state.emitted(&stream, &event);
        emit_line(writer, &message).await?;
    }
    Ok(())
}

async fn emit_event<P, W>(
    writer: &mut W,
    state: &mut OutputState<P>,
    stream: String,
    event: Value,
) -> Result<()>
where
    P: CorrelationPolicy,
    W: AsyncWrite + Unpin,
{
    match state.event_disposition(&stream, &event) {
        EventDisposition::Emit => {
            let message = state.event_message(&stream, &event);
            state.emitted(&stream, &event);
            emit_line(writer, &message).await
        }
        EventDisposition::Buffer(id) => {
            state.buffer(id, stream, event);
            Ok(())
        }
        EventDisposition::Drop => Ok(()),
    }
}

async fn emit_line<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value).context("serialize daemon JSON line")?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .context("write daemon JSON line")?;
    writer.flush().await.context("flush daemon JSON line")
}

#[cfg(test)]
#[path = "routing_tests.rs"]
mod routing_tests;

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, duplex};
    use tokio::sync::mpsc;

    use super::{
        BasicCorrelation, OutputCommand, OutputState, run_output_actor,
        spawn_output_actor_with_writer,
    };

    async fn render(commands: Vec<OutputCommand>) -> Result<Vec<Value>> {
        let (writer, mut reader) = duplex(4096);
        let (output, task) = spawn_output_actor_with_writer(BasicCorrelation, 8, 4, writer);
        for command in commands {
            output.send(command).await?;
        }
        drop(output);
        task.await??;

        let mut text = String::new();
        reader.read_to_string(&mut text).await?;
        text.lines()
            .map(serde_json::from_str)
            .collect::<serde_json::Result<_>>()
            .map_err(Into::into)
    }

    #[tokio::test]
    async fn buffers_subscription_events_until_the_response_is_written() -> Result<()> {
        let lines = render(vec![
            OutputCommand::Event {
                stream: "things.changed".into(),
                event: json!({ "event": "subscribed", "subscription_id": "sub-1" }),
            },
            OutputCommand::Response {
                id: "subscribe".into(),
                result: Ok(json!({ "data": { "subscription": { "id": "sub-1" } } })),
                cancelled_request_id: None,
                route: None,
            },
        ])
        .await?;

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["kind"], "response");
        assert_eq!(lines[1]["kind"], "event");
        Ok(())
    }

    #[tokio::test]
    async fn responses_overtake_queued_events() -> Result<()> {
        let (priority_sender, priority_receiver) = mpsc::channel(4);
        let (event_sender, event_receiver) = mpsc::channel(4);
        event_sender
            .send(OutputCommand::Event {
                stream: "things.changed".into(),
                event: json!({ "sequence": 1 }),
            })
            .await?;
        priority_sender
            .send(OutputCommand::Response {
                id: "status".into(),
                result: Ok(json!({ "ok": true })),
                cancelled_request_id: None,
                route: None,
            })
            .await?;
        drop(priority_sender);
        drop(event_sender);

        let (writer, mut reader) = duplex(4096);
        run_output_actor(
            priority_receiver,
            event_receiver,
            writer,
            OutputState::new(BasicCorrelation, 4),
        )
        .await?;
        let mut text = String::new();
        reader.read_to_string(&mut text).await?;
        let lines = text
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<serde_json::Result<Vec<_>>>()?;

        assert_eq!(lines[0]["kind"], "response");
        assert_eq!(lines[1]["kind"], "event");
        Ok(())
    }

    #[tokio::test]
    async fn drops_late_events_after_cancellation() -> Result<()> {
        let lines = render(vec![
            OutputCommand::Response {
                id: "subscribe".into(),
                result: Ok(json!({ "data": { "subscription": { "id": "sub-1" } } })),
                cancelled_request_id: None,
                route: None,
            },
            OutputCommand::Response {
                id: "cancel".into(),
                result: Ok(json!({ "cancelled": "sub-1" })),
                cancelled_request_id: Some("sub-1".into()),
                route: None,
            },
            OutputCommand::Event {
                stream: "things.changed".into(),
                event: json!({ "subscription_id": "sub-1" }),
            },
        ])
        .await?;

        assert_eq!(lines.len(), 2);
        Ok(())
    }
}
