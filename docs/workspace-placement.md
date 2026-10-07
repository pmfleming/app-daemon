# Category workspace placement correction

All user application launches are assumed to originate in Shelllist/app-daemon.
UWSM remains the preferred launcher and owns session/process isolation; the daemon
owns workspace policy. No global compositor rules or executable-name guesses are
introduced. Existing windows are never relocated by launch/category assignment.

## Stages

1. **Launch provenance**: anchor PID/start-time identities after checked handoff,
   using the unique systemd service's MainPID, exact launch-cgroup members, direct
   child PID, or the stable D-Bus application's unique owner. Retain observed
   descendants across scope migration and reparenting; reject recycled PIDs.
2. **Routing and verification**: share placement across launch, non-running
   activation and desktop actions; include established singleton processes,
   exclude the pre-launch window set, and confirm the compositor workspace.
   Publish separate structured launch/placement outcomes without replaying a
   successful launch when placement is unavailable.
3. **Consumer feedback and acceptance**: minimally adapt Shelllist to show
   placement warnings after handoff, add Qt coverage and document the contract.

## Safety boundaries

A matching application class alone is not launch ownership. A singleton's
previously identified live process may create a new window, but existing window
addresses remain ineligible. Ambiguous multiple candidates are left untouched.
Unobservable process handoffs (including terminal servers without a provable
application relationship) must report unavailable placement, not guess. UWSM,
systemd and D-Bus failures retain existing bounded handoff/cancellation behavior.

## Stage 1 validation

`cargo test --locked launch:: --lib` covers scope migration, observed descendants,
reparenting, unrelated processes, PID reuse, procfs parsing, exact unit boundaries
and bounded launcher handoff. Later stages add private-bus/compositor integration
coverage; no real graphical application is launched by these tests.

## Stage 2 validation and API

`cargo test --locked --all-features` includes private D-Bus/systemd/compositor
fixtures. These verify all five saved categories overriding workspace context for
launch, activation and desktop actions; MainPID ownership outside the launch
cgroup; established singleton and D-Bus owner windows; existing-window exclusion;
ambiguous/unrelated windows; rejected moves and acknowledged-but-unobserved moves.
Operation-status reads retain the same structured placement result as events.

The additive `placement` object has `workspace_id`, `status` and optional `reason`.
`pending` accompanies checked launch handoff; `placed` requires observed compositor
state; `unavailable` means no safe candidate/baseline; `failed` means dispatch or
verification failed. A completed launch with failed/unavailable placement is a
partial success: do not retry the launch. A subsequent focus failure can still
fail the operation while preserving the successful launch and placement receipt.
No placement object is supplied for ordinary existing-window focus, launch-only
shortcuts, or launches with no workspace request. Older consumers can ignore the
new object, but should upgrade to surface partial-success warnings.

## Stage 3 consumer contract and acceptance

Shelllist uses the structured status only to present daemon-owned placement
warnings. Completed partial successes stay inline when visible or notify after
handoff, including outcomes recovered through an owned status read. Duplicate or
foreign events cannot notify twice or replay a launch. The shared keyboard model
and settings acknowledgement are unchanged. The app-api fixture includes a
serialized partial-success example and the four placement statuses; daemon
serialization tests and Shelllist's fixture comparison keep both sides aligned.

Validation completed:

- Rust all-feature suite: 47 unit tests and 12 private-session tests passed.
- Rust Clippy (all targets/features, warnings denied) passed.
- Real user-systemd lifetime fixtures: both opt-in tests passed and cleaned up.
- Shelllist Qt suite: 388 passed, 0 failed, 1 renderer-dependent skip.
- Shelllist strict QML lint, app-api fixture comparison and lifecycle checks passed.

No real graphical apps were launched or moved for validation, and the running
NixOS daemon was not replaced. Build/deploy both repositories before live
acceptance: from workspace 9, launch an app assigned to each category and check
its resulting workspace; check Spotify scope migration and D-Bus/new-window
singletons. Existing windows must remain untouched. An unprovable handoff must
warn rather than moving a guessed window or starting another instance.
