//! Shared suspend detection; resource invalidation remains application policy.
pub(crate) use shelllist_daemon_tokio::{
    ResumeDetector as ResumeClock, monitor_resumes as monitor,
};
