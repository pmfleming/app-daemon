# Review issue 5: reuse the session's capability cache

App-daemon now shares one immutable process-lifetime compositor client across
reads, event watching, current-workspace capture, focus, close and move. Success
is cached by that client; a failed probe remains retryable. SessionGone still
exits the daemon and never rebinds this client or cache to a different session.

Validation: the private process/bus/socket test
`mutation_capabilities_retry_failure_cache_success_and_reset_on_process_restart`
passes. It rejects the first capability check, successfully retries, then proves
another mutation performs no preflight. Restarting the daemon requires all three
preflight exchanges again. Existing SessionGone tests cover the no-rebind exit.
Formatting passes; log `/tmp/app-ipc-review-cache.log`.
