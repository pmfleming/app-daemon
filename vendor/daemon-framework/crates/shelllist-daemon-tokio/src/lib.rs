//! Tokio and session D-Bus infrastructure shared by Shelllist daemons.

mod dbus;
mod jsonl;
mod output_actor;
mod shutdown;
mod subscription;

pub use dbus::{JsonDbusClient, directed_emitter, wait_for_owner_loss, wait_for_owner_name_loss};
pub use jsonl::{CallFailure, CallFailureMapper, CancelMode, JsonlClientConfig, run_jsonl_client};
pub use output_actor::{BasicCorrelation, CorrelationPolicy, TrackedId, TrackedKind};
pub use shutdown::wait_for_shutdown;
pub use subscription::OwnedTaskRegistry;
