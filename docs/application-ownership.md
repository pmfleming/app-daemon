# Application ownership across scope migration

## Problem and scope

Native Wayland `app_id` is a client-supplied surface label. Hyprland exposes it as
`class` and reports the Wayland connection's PID, not a complete helper-process
inventory. Chrome 150 app-mode windows generate a web-app/profile ID independently
of `--class`. Its portal integration also moves only the browser process into
`app-com.google.Chrome-<PID>.scope`; earlier helpers remain in the launch service
and later helpers inherit the new scope. Neither a desktop class alias nor cgroup
membership alone describes the entire logical application.

The daemon now reconciles verified launch ownership. It does **not** move
processes, stop units, disable sandboxing, patch Chrome, change portal identities,
parse browsing history/profiles/environment, or add application-specific hostnames.
No app-api fields or Shelllist interaction behavior change.

## Evidence and lifetime

`ownership::Ownership` retains desktop IDs against PID/start-time pairs captured
from the existing checked launch receipt. Only live roots of that receipt seed
persistent ownership; ancestry does not promote independently launched child
applications into roots of the parent's claim. Launch-only or removed catalog
entries cannot establish ownership.

Bounded user-systemd recovery runs with resource reconciliation, not API queries.
It enumerates active application **services**, resolves their unit names through
the catalog, and reads live `MainPID`. Before accepting an observation it checks
nonzero `InvocationID`, re-reads invocation/PID/active state without a property
cache, and checks the process start time again. Historical `ExecMainPID` is never
used. Each candidate has a 250 ms deadline, at most eight run concurrently, and
the entire recovery has a two-second deadline. Completed claims survive another
candidate timing out. A missing manager does not erase live claims already seen.

Current-user procfs observations revalidate process identity around cgroup reads.
Observed descendants remain associated across reparenting, and new descendants
can join later. Exit, PID reuse, catalog removal and independently named child
application boundaries revoke or prevent inheritance. A verified root may adopt
its own new scope, including helper subgroups, but cannot claim unrelated
same-scope siblings or independently named nested application units. Conflicting
verified claims remain unresolved for that process rather than choosing an
arbitrary desktop ID. Independent child roots remain separate even when their
parent has conflicting evidence.

Reconciliation is serialized with launch registration, including when an async
caller is cancelled while its blocking procfs task continues. No PID-only cache
is persisted to disk. State is reconstructed from live evidence after restart.

## Consumers and safety

- Query grouping and action targeting use the same snapshot, validating the live
  PID/start-time before applying a cached claim. Unclaimed windows retain existing
  cgroup/class/desktop-ID fallback behavior.
- Ownership changes participate in query/revision/admission tokens and state
  publication. This includes reconciliation without a compositor window change.
- Resource discovery validates the snapshot against its own process observation,
  removes displaced scope memberships and prevents ambiguous ownership from
  charging either application. Existing process deduplication, independent-child
  exclusion, PID-reuse baselines and conservative cgroup-counter fallback remain.
- Resource samples remain asynchronous historical observations; newly registered
  ownership can precede the next resource sample. Existing history is not rewritten.
- Placement still requires launch provenance, an unambiguous **new** window,
  pre-launch address exclusion and compositor verification. Successful launch with
  unavailable placement never causes automatic replay.
- Focus/close/move validate application/window identity against a fresh compositor
  snapshot. Close remains a compositor window-close request, not process killing.

## Deliberate limits

Isolated web-app profiles have separate browser instances and fit this model.
Several web apps sharing one browser process do not have separable process-level
usage; this change does not claim otherwise. Conflicting verified root claims
stay unknown. Application ancestry/labels are not a security sandbox.

Recovery needs live evidence. If a root and its service disappeared before daemon
restart and only reparented helpers in a generic scope remain, their old ownership
cannot be reconstructed safely. Existing fallback behavior applies. Launches
outside the daemon without an identifiable live application service similarly do
not gain guessed ownership. Portal permission identity and other taskbars are not
changed by Shelllist's logical attribution.

## Validation

Unit tests cover concurrent isolated applications and an ordinary browser, scope
migration, early/late helpers, same-scope siblings, nested unit boundaries,
reparenting, PID reuse, catalog removal, conflicts and launch-only entries.
Resource tests exercise reassignment from an already sampled host scope and check
non-overlapping process counts and conservative counter selection.

Private D-Bus/compositor integration tests cover generated Wayland classes,
verified placement, single-row grouping, daemon restart recovery, revision-bound
actions and exclusion of an unrelated same-class window. Recovery rejection tests
cover historical-only PIDs, inactive services and changing invocation IDs. Existing
launch, cancellation, singleton and placement-failure coverage remains in place.

Run `cargo test --locked --all-features` and
`cargo clippy --locked --all-features --all-targets -- -D warnings`.

Live acceptance after deployment: launch Pocket Casts and Audible alongside
ordinary Chrome; start/stop playback and check a single row and complete helper
accounting per isolated app; restart app-daemon; activate and close each app;
check ordinary Chrome is unaffected. No real browser playback, application close
or running-daemon replacement is performed by the automated fixtures.

## Source evidence for the environment investigated

- [Hyprland 0.56.2 window app ID/PID handling](https://github.com/hyprwm/Hyprland/blob/efb50993780079460b0cbed1363e2166a2de1d9f/src/desktop/view/Window.cpp)
- [Chrome 150.0.7871.128 Wayland web-app identity](https://github.com/chromium/chromium/blob/150.0.7871.128/chrome/browser/ui/views/frame/browser_native_widget_aura_linux.cc)
- [Chrome's portal-driven single-process scope migration](https://github.com/chromium/chromium/blob/150.0.7871.128/components/dbus/xdg/systemd.cc)
