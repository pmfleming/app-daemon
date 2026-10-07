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
