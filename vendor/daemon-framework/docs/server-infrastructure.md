# Server infrastructure and daemon policy

The framework owns concurrency and lifetime mechanics, not a universal daemon
class. Daemons retain their D-Bus interfaces, method dispatch, schemas, snapshots,
resource selection, error mappings and side effects. Existing event payloads and
cancellation return types are unchanged by this migration.

## Workers and resume detection

`TaskGroup` owns a generation of background workers. `abort()` closes its spawn
gate synchronously; `shutdown().await` also joins cancellation cleanup. Drop
aborts outstanding workers. Domain-specific graceful cleanup must run before
aborting workers that perform it. `spawn_named` logs task failures and preserves
panics as failed joins. `catch_task` is an explicit alternative for operations
that translate a panic into a domain failure result.

`ResumeDetector` combines a BOOTTIME/MONOTONIC offset with logind notifications,
rejects badly preempted samples and deduplicates delayed signals. `monitor_resumes`
publishes a generation with reconnecting logind and clock fallback, stopping when
there are no consumers. Bar keeps its logind power/lock policy and uses only the
detector; App uses both the detector and monitor. No wall-clock gap heuristics are
involved.

## Managed subscriptions and owner monitoring

One `OwnedTaskRegistry` belongs to one serving D-Bus connection. It lazily owns a
connection-scoped `OwnerLossMonitor`; there is no process-global bus singleton.
Use a new registry for a replacement connection or independent bus.

```rust,ignore
let id = subscriptions.next_id("subscription");
subscriptions.spawn_for_owner(
    id.clone(),
    Some(owner.to_string()),
    emitter.connection(),
    forward_domain_events(receiver, emitter.clone(), id.clone()),
)?;
```

The registry admits before spawning, registers before polling the worker, and
removes registration on completion, cancellation, panic or owner loss. Cleanup
from an older task cannot remove a reused ID. The worker does not own the registry,
so registry drop can actually terminate it. `shutdown()` closes admission before
aborting and joining registered workers.

`TaskLimits` defaults to 256 total / 64 per owner; daemons can override both. A
missing owner is a separate internal owner bucket. Whether missing caller identity
is permitted remains daemon policy (Bluetooth and Clipboard reject it).

`spawn_until` supports a non-D-Bus lifetime signal. `spawn_for_owner` additionally
ends the task on monitor failure, logging the reason. It does not broadcast a
private event on failure or replay accepted work.

The old manual `insert`/`remove` task lifecycle has been replaced by managed spawn.
Callers should not retain a second registry or create their own start barrier.
The standalone `wait_for_owner_loss` helpers remain available; repeated waits
should instead share an explicit `OwnerLossMonitor`.

Actor-based servers can use `OwnerLossMonitor::subscribe`. `Lagged` requires
rechecking the actor's current owner set with `has_owner`; `Stopped` is a terminal
monitor error. NM reconciles on startup and lag, releases owned work on failure,
and retries the monitor. Subscription and operation cleanup remain NM policy.

## Filesystem persistence

`StagedFile::new` creates a complete, synced temporary file next to the destination.
`commit()` renames it and applies the configured directory durability barrier.
Drop removes only that object's temporary file. Name collisions retry without
removing someone else's file. `write_bytes_atomic` performs both steps, and
`write_json_atomic` serializes through the same implementation.

`AtomicFilePolicy` controls modes, parent syncing, syncing newly created directory
ancestors, and refusing symlinked destinations. PRIVATE uses 0700 directories and
0600 files. DURABLE leaves permissions at their default/umask settings. Use these
policies only below application-owned directories: private writes may chmod the
immediate parent. These helpers are not a sandbox against hostile ancestor-path
replacement; callers such as NM retain repository ownership checks and locking.

A parent-sync error can happen **after** rename. A caller requiring rollback must
keep the previous contents. Clipboard retains its best-effort coordinated settings
rollback; it is not a crash-atomic multi-file transaction. Bar's successful write
remains a durability barrier before hardware changes.

`read_bytes_bounded` opens without following the final symlink or blocking on a
FIFO, requires a regular file and checks the bound during reading as well as at
open. NM retains missing/corrupt/available classification and its size policy.

The Tokio byte read/write wrappers offload I/O. Dropping a write future does not
interrupt an accepted filesystem commit.

## Blocking lanes

`BlockingLane` bounds both queued work and running blocking jobs. `try_submit` and
`call_async` admit synchronously, before spawning blocking tasks. `call` is for
synchronous callers only; never invoke it on a Tokio worker or recursively on its
own saturated lane. `Full`, `Closed` and `Panicked` are infrastructure errors that
daemons map into their existing error envelopes.

Shutdown closes admission and drops queued jobs, then waits up to a supplied
budget for running jobs. Blocking jobs cannot be aborted: `LaneShutdown` reports
remaining work instead of claiming cancellation succeeded. Cancelling a reply
waiter does not free a running job's capacity.

NM keeps lane capacities, fast/read/work classification and NetworkManager error
context local. Its D-Bus ingress now has separate request (64 queued / 8 running)
and control (16 queued / 2 running) lanes. Cancellation does not compete for ordinary
ingress capacity, and direct D-Bus clients cannot bypass that ingress bound.

## Operations and forwarding

`OwnedOperations<T>` supplies owner-scoped admission and single-winner terminal
claims. Synchronize admission, insertion and state transitions in the caller's
mutex/actor. `RecentResults<T>` supplies bounded optional-TTL retention and owned
lookup. It does not abort tasks, choose result visibility, interpret progress,
retry work or cross an operation's commit boundary.

App and Bluetooth use shared active-operation records. App, Bluetooth and NM use
shared terminal retention, with their original retention/visibility policies.
Bluetooth gates progress and terminal publication on an active record. Clipboard's
editing/committing cancellation state machine and NM's cooperative cancellation
remain local; neither is replaced by unconditional task abortion.

`forward_watch` releases each watch borrow before awaiting I/O and distinguishes
initial/change delivery. `forward_broadcast` exposes lag explicitly and stops on
source closure. Daemons map those notifications to their existing event names and
recovery actions. `emit_json_event` preserves the supplied emitter's destination,
interface and the domain's envelope fields. It returns emission failures rather
than choosing a global recovery policy.

## Validation and deployment

Framework tests include independent private buses with identical unique names,
owner disappearance and bus failure, private signal delivery, managed lifecycle
and admission, cancellation/completion races, file failure cleanup, bounded reads,
blocking-lane overload/shutdown, resume deduplication and forwarding lag/closure.
Transport tests require `dbus-daemon` (included in the development/check environment).

App vendors this source; refresh its `vendor/daemon-framework` snapshot together
with the shared framework. The other daemon Cargo manifests use the sibling
checkout, but their Nix builds pin the framework Git input. After committing the
framework changes to the input's `main` branch, update those pins before deployment:

```sh
for daemon in bar-daemon bt-daemon clip-daemon nm-daemon; do
    (cd "../$daemon" && nix flake update daemonFramework)
done
```

Do not deploy only the daemon changes against the previous framework input.
