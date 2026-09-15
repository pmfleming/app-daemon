//! Runtime-independent infrastructure shared by Shelllist daemons.

mod endpoint;
mod envelope;
mod file;
mod jsonl_wire;
mod operation;
mod protocol;
pub use operation::{
    OperationAdmissionError, OperationLimits, OwnedOperation, OwnedOperations, RecentResults,
};
mod state;
pub use file::{
    AtomicFilePolicy, StagedFile, parent_directory, read_bytes_bounded, sync_parent,
    write_bytes_atomic,
};

pub use endpoint::{DaemonEndpoint, IdSequence};
pub use envelope::{ApiError, ApiIdentity, Correlation, error, event_envelope, success};
pub use jsonl_wire::{
    ClientMessage, ClientRequest, ClientRoute, RouteKind, addressed_message, event_message,
    protocol_error_message, response_error_message, response_message, shutdown_message,
    transport_error_message,
};
pub use protocol::{fixture_names, load_fixture, registry_names, validate_unique_names};
pub use state::{
    AtomicWritePolicy, StateError, XdgRoot, read_json, resolve_xdg_path, resolve_xdg_root,
    write_json_atomic,
};
