//! Tokio and session D-Bus infrastructure shared by Shelllist daemons.

#[cfg(test)]
mod transport_tests;

mod blocking;
mod dbus;
pub use blocking::{BlockingLane, LaneError, LaneShutdown};
mod file;
mod forward;
mod jsonl;
pub use file::{read_bytes_bounded_async, write_bytes_atomic_async};
pub use forward::{BroadcastEvent, WatchPhase, emit_json_event, forward_broadcast, forward_watch};
mod output_actor;
mod owner;
pub use owner::{OwnerLossError, OwnerLossMonitor, OwnerLossReceiver};
mod resume;
mod shutdown;
mod task;

pub use resume::{ResumeDetector, monitor_resumes, suspend_offset};
pub use task::{AbortOnDrop, TaskGroup, catch_task, spawn_named};
mod subscription;

pub use dbus::{JsonDbusClient, directed_emitter, wait_for_owner_loss, wait_for_owner_name_loss};
pub use jsonl::{CallFailure, CallFailureMapper, CancelMode, JsonlClientConfig, run_jsonl_client};
pub use output_actor::{BasicCorrelation, CorrelationPolicy, TrackedId, TrackedKind};
pub use shutdown::wait_for_shutdown;
pub use subscription::{OwnedTaskRegistry, TaskAdmissionError, TaskLimits};
