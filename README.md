# app-daemon

Rust application catalog, Hyprland window identity, process-tree CPU and resident-memory accounting, and activation policy for the Shelllist launcher.

```sh
../daemon-framework/tools/local-build develop .
cargo test --locked
../daemon-framework/tools/local-build build .
```

Keep `daemon-framework` beside this checkout. All five daemons use that same current framework, including tracked uncommitted edits; do not vendor or revision-pin it. `local-build` snapshots the local graph once for Nix builds/checks without writing deployment pins. Cargo uses the sibling directly. Add new files to Git first. Hyprland IPC comes from the framework's `shelllist-hyprland` crate; no vendored copy or separate Hyprland checkout is needed. Registry dependencies and nixpkgs remain locked. Without Nix, install stable Rust plus `sh`, `sleep`, and `dbus-daemon`, then use `cargo test --locked`. Runtime launching needs UWSM or a user systemd manager plus the configured launch helpers.

`app-daemon daemon` exports `org.laufan.AppDaemon`; `app-daemon client` bridges JSONL requests to the session service using `app-api` v1. Resource collection is isolated behind an injectable Linux provider so procfs, cgroup, and energy edge cases can be tested without relying on the host.

Application execution returns an accepted operation immediately. Subscribe to `applications.operation` for `running`, `completed`, `failed`, or `cancelled` updates, and cancel an active operation through the transport's existing `cancel` request. Passing `expected_revision` rejects stale actions; `move-to-workspace` also accepts `window_id` and `workspace_id`. Operation status is recoverable through `applications.operation.status` with `{"operation_id":"operation-..."}` (response: `data.operation_status`). Lookup and cancellation are scoped to the originating D-Bus connection. The latest 256 terminal outcomes are retained for up to 15 minutes; unknown, expired, or foreign IDs return `request-not-found`. A lagging operation subscription emits `resync-required`; look up outstanding IDs rather than waiting indefinitely. Admission is bounded to 128 active operations globally and 32 per connection.

Entries marked `DBusActivatable=true`, including entries without `Exec`, use the bounded `org.freedesktop.Application.Activate`/`ActivateAction` protocol first. Startup/activation tokens are forwarded. If activation fails and a valid Exec fallback exists, normal launcher handoff is used; otherwise the operation reports the activation failure.

Other graphical launches, terminal applications, and desktop actions pass their desktop IDs directly to `uwsm-app -t service` (the fast, drop-in client for `uwsm app`) and run in unique units in `app-graphical.slice`. Service-mode handoff returns after the application executable starts instead of keeping the operation open for the process lifetime. This lets UWSM interpret `Terminal`, `Path`, and desktop-action metadata itself.

When UWSM is unavailable, `systemd-run` isolates graphical `gtk-launch` handoffs in independent scopes, and terminal/parsed commands in `Type=exec`, `ExitType=cgroup` services. Desktop `Path` and terminal actions are honored. Applications survive stopping or restarting the daemon's service. Launcher handoffs are bounded to ten seconds and nonzero launcher exits are failures; diagnostic capture is bounded. After a checked launch handoff, a `running` operation carries `launch_backend`/`launch_scope` before window placement finishes. The owning client's operation-status read retains that receipt, so a chooser can dismiss without cancelling or losing the operation. Focus completion acknowledges compositor dispatch; close completion means a close request was sent, not that the window disappeared (the application may ask to save). Consumers must reconcile window snapshots before reporting windows closed. If neither manager helper exists, unmanaged direct spawning is permitted only outside service cgroups; otherwise launching fails explicitly rather than attaching applications to the daemon's lifecycle. The `systemd-run` fallback requires a working user manager.

Window identity first uses verified live PID/start-time ownership captured at launch or recovered from an active application service. It survives cgroup migration and feeds the same window grouping, resource accounting and action-target resolver. Without that evidence, resolution uses a specific application cgroup, then `StartupWMClass`, desktop-ID and unique reverse-DNS suffix matching. Conflicting verified claims remain unmatched rather than falling back to a guessed catalog owner. This keeps isolated Chromium web apps and terminal-hosted applications attached to their launcher row despite generated Wayland app IDs or host-named scopes. Desktop entries marked `X-Shelllist-LaunchOnly=true` never claim windows or resources. See [application ownership](docs/application-ownership.md) for recovery, boundaries and limitations.

Application queries rank exact names, desktop IDs, prefixes, substrings, metadata, and short acronyms in descending tiers, and can filter the five Shelllist categories: Shell, Browser, Code, Media, and Text. Results expose `match_score`, `match_kind`, `runtime_score`, and the compatible combined `score`, so launchers can explain or customize their ordering.

Per-application category preferences are persisted in `$XDG_CONFIG_HOME/app-daemon/application-settings-v1.json` (or `~/.config/...`) through `applications.settings.update`. Categories map directly to default workspaces: Shell→1, Browser→2, Code→3, Media→4, and Text→5. The selected workspace overrides launch context. Same-application operations are serialized and use a fresh pre-launch snapshot. Launch, non-running activation, and desktop actions share the same workspace policy. The daemon moves/focuses only an unambiguous new window with verified application identity and process provenance, leaving existing instances in place. Provenance includes the launch unit's MainPID and members, stable D-Bus owner, and established singleton processes captured before handoff; observed PID/start-time identities and descendants survive cgroup migration without accepting recycled PIDs. UWSM remains the preferred launcher, not the workspace-policy owner. A move is successful only after a compositor snapshot confirms the requested workspace.

Launch operations with workspace context expose an optional `placement` object: `workspace_id`, `status` (`pending`, `placed`, `unavailable`, or `failed`), and an optional `reason`. It is retained in owner-scoped operation-status reads. `completed` with `placement.status` of `unavailable` or `failed` means the application **did start** but placement did not succeed; consumers must show the warning without replaying the launch. Ambiguous multi-window launches, singleton handoffs without a new window, unavailable baselines and unprovable ownership leave windows untouched. Launch-only shortcuts do not claim placement. Launch failure remains an ordinary failed operation without a launch receipt. See [placement safety and validation](docs/workspace-placement.md).

## Validation

`cargo test --locked --all-features` includes private D-Bus/compositor integration tests. They create their own bus configuration, isolated HOME/XDG roots, command fixtures, and Unix sockets; no host desktop service is activated. Coverage includes metadata refresh, D-Bus-only activation/actions, operation recovery/ownership, launch failures, the real ten-second handoff timeout, cancellation, shutdown persistence, and compositor recovery.

Two opt-in tests use the real user systemd manager to verify that service-mode executables and scope-mode GTK handoffs survive stopping their host service. They create only uniquely named temporary fixture units/processes and clean them up:

```sh
cargo test --locked --test systemd_lifetime -- --ignored isolated_
```

These tests are excluded from sandbox builds; the private-bus tests run normally in the Nix check phase. Neither suite launches a real graphical application.

## Resource metrics

The daemon samples applications independently of API queries: every two seconds during recent UI demand, and every ten seconds in the background. It discovers the current user's identifiable systemd/UWSM/Flatpak application cgroups and stable D-Bus application services even when no window exists. Whole-cgroup metrics require that the unit itself resolves to the application; a PID inside an unrelated launcher service uses process-tree accounting instead. Window PIDs and observed descendants provide the fallback; their PID/start-time identities remain tracked after windows or parent processes exit, until the processes exit or their PIDs are reused. Processes explicitly owned by a different application scope are excluded from the parent's accounting. Verified launch roots and their descendants can retain their logical application across a host-named scope migration, but sharing that scope alone never grants ownership of its other members. Previous scope memberships are removed before applying verified ownership, and incomplete or mixed cgroup coverage uses process counters rather than borrowing whole-scope totals. Launch-only entries never claim scopes.

Background applications participate in resource history and energy summaries and are exposed as `running: true` with `running_count: 0` (that count describes windows). Changes in the set of running targets publish application revisions. Unknown services and never-observed, unmanaged windowless processes are deliberately not guessed from executable names: accurate attribution requires a known application unit or a previously observed window/process tree. Every result includes its attribution method, sample interval, process coverage, capability flags, and whether processes are shared by multiple application targets.

After resume, logind notifications wake resource sampling immediately. A CLOCK_BOOTTIME/CLOCK_MONOTONIC comparison also detects sleep before/after each blocking sample, including when the signal connection is unavailable. CPU, cgroup, GPU, network and energy deltas are re-baselined; the first interval has zero duration and is not added to history. Completed disk footprints/workers are retained. Wall-clock changes are not treated as sleep.

Window snapshots and dispatches use the shared bounded Hyprland command socket directly, without spawning `hyprctl`. Relevant client/focus/workspace events are coalesced for 75 ms; layer, keyboard-layout and submap events do not trigger snapshots. Healthy event delivery uses 30-second window reconciliation and five-minute catalog reconciliation; failures retain five-/thirty-second fallback polling. Reconnect and resume trigger immediate refreshes. Catalog watches cover missing-root parents and symlink ancestors so Nix profile switches do not wait for the slower timer.

Catalog watchers are optional accelerators. Initialization, watch-registration, and runtime failures retain catalog/window reconciliation and retry watcher installation with 1–30 second exponential backoff. A closed event stream never terminates the other state trackers.

`cpu_percent` follows `top` semantics (100% is one logical CPU), while `cpu_percent_of_machine` is normalized to the whole machine. Cgroup `cpu.stat` and `io.stat` deltas are preferred for specific application scopes, preserving completed work from short-lived children; PID start times and `/proc/stat` deltas provide the fallback and avoid PID-reuse errors and wall-clock/suspend skew. Process and thread counts plus major-fault rates are also reported. Procfs sampling is two-stage: the daemon reads lightweight process identity and CPU fields globally, then reads expensive memory, I/O, fd, and DRM details only for attributed processes. Memory details refresh every ten seconds and file footprints every thirty seconds; socket discovery still runs on every sample. Application-directory scans run separately on a bounded two-worker pool and never block publication of CPU, memory, GPU, or network samples.

Memory prefers proportional set size from `/proc/<pid>/smaps_rollup`, avoiding repeated charging of shared pages in multi-process applications. RSS is the fallback. RSS, PSS, private memory, and swap remain separately available, and `memory_source` identifies the source used by the compatible `memory_bytes` field.

GPU usage comes from DRM client counters in `/proc/<pid>/fdinfo`. DRM clients duplicated across file descriptors or processes are counted once. `gpu_percent` is aggregate engine occupancy and may exceed 100%; `gpu_busy_percent` is the busiest engine and is capped at 100%. Resident and allocated GPU memory are reported separately. Capability metadata distinguishes an idle supported GPU from unavailable DRM accounting.

Physical storage I/O comes from `/proc/<pid>/io`; logical cached I/O, operation counts, cancelled writes, and normalized rates are also exposed. Open and memory-mapped files are deduplicated by device and inode. Referenced-file footprint is split between temporary/cache paths and other files.

Application-owned disk space is measured separately by scanning matching directories under XDG config, data, state, cache, runtime, and Flatpak application roots. `disk_space_permanent_bytes` covers config/data/state, `disk_space_temporary_bytes` covers cache/runtime data, and `disk_space_total_bytes` is their sum. This is application data footprint, not package-installed size; unidentified directories and arbitrary `/tmp` names are intentionally not guessed. Directory measurements refresh asynchronously every five minutes, retaining the last completed measurement while a refresh runs or fails. Each walk has a cooperative two-second / 100,000-entry budget, does not follow symlinks or cross filesystem boundaries, and never publishes a partial size. Slow filesystem syscalls can hold a disk worker beyond the time budget but cannot block the resource sampler. Before the first successful scan, disk-space accounting is reported as unavailable.

Network connection count is derived from unique sockets held by the attributed processes, discovered anew on every resource sample independently of the file-footprint cache. Receive and transmit rates aggregate Linux INET_DIAG lifetime counters for the application's known TCP socket inodes, then report interval deltas. Newly opened sockets in already observed processes include their initial bytes; startup, newly attributed processes, and recovery establish a baseline instead. This is best-effort sampled accounting: connections that open and close entirely between samples, and final bytes after the last observation, cannot be recovered. UDP and Unix sockets remain connection-only because Linux does not expose equivalent per-socket lifetime byte counters. Network-namespace totals are never misreported as process traffic.

Energy remains an estimate. Linux powercap/RAPL package energy is attributed by observed CPU-time share and marked low confidence. Battery discharge is exposed only as system power context because it includes the display, radios, storage, and idle losses; it is no longer assigned to individual applications. `energy_source`, `energy_confidence`, and `attributed_fraction` describe every value.

Some kernels expose RAPL `energy_uj` counters as root-only. The unprivileged user daemon reports energy as unavailable rather than inventing a value. On NixOS, access can be explicitly granted to desktop users in the `video` group (this relaxes the kernel's energy-counter side-channel protection):

```nix
services.udev.extraRules = ''
  ACTION=="add|change", SUBSYSTEM=="powercap", TEST=="energy_uj", ATTR{enabled}="1", RUN+="${pkgs.coreutils}/bin/chgrp video /sys%p/energy_uj", RUN+="${pkgs.coreutils}/bin/chmod 0440 /sys%p/energy_uj"
'';
```

Apply the rule by rebooting or by retriggering the `powercap` subsystem after rebuilding. Power is otherwise available only while a battery reports an actual discharge rate; charging and AC-only measurements are not treated as system consumption.

Resource history is aligned to 15-second wall-clock buckets and retained for 24 hours in `$XDG_STATE_HOME/app-daemon/resource-history-v1.json` (or `~/.local/state/...`). Points include averages, peaks, sample count, coverage, and mixed-source metadata. Each new point also includes per-metric `availability`: a capability is true only if it was available for every observed sample in that bucket. Unsupported or mixed-availability buckets render as gaps, while supported idle measurements remain zero. Older records without this metadata have unknown optional capabilities. A compact one-minute application-energy ledger in the same file is retained for seven days and powers `applications.energyOverview` without keeping a week of full resource samples. Expired partial buckets are finalized even after an application exits.

History is returned oldest-first. The response includes an opaque `next_cursor`; pass it back to retrieve the next page or poll for points recorded after the last response.

New clients should also supply `until_ms` to freeze a window during pagination.
Every page includes a `summary` for the **entire selected window**, even an empty
incremental page: window bounds, a revision, and per-metric availability, mean,
peak, observed milliseconds and temporal coverage. Means are weighted by observed
`duration_ms` (clipped at the window boundary), not by bucket count. Peaks retain
the maximum recorded inside participating buckets; a partial bucket cannot locate
a peak more precisely. Summaries include observations overlapping either window
boundary, even when a bucket ends after `until_ms`. Returned points and cursors
remain filtered by bucket-end timestamps; a summary-only overlapping bucket does
not advance the cursor. Missing measurements yield null statistics, not zero.
Legacy capability normalization happens in the daemon before points are returned.

Example request:

```json
{"target_id":"org.example.App.desktop","since_ms":0,"cursor":null,"limit":1000}
```

For a sorted energy summary across applications, call `applications.energyOverview` with `{"since_ms":0,"limit":20}`. The response contains attributed mWh, relative shares, desktop names/icons, and source/confidence metadata. It includes only energy the sampler can attribute (currently RAPL CPU-time share).

Cursors are versioned and bound to their target application. Invalid, stale-format, cross-target, or expired cursors produce a validation error. A cursor expires when its referenced bucket is pruned, even if that application has newer history; cursors referencing retained buckets remain valid. Finalized buckets are sorted and unique, including after restart or a backward wall-clock adjustment. If a restart merges new samples into a previously returned partial bucket, or clock rollback backfills older buckets, existing cursors explicitly become stale: restart pagination without a cursor to obtain the corrected history. Normal appends and restarts without rewrites preserve cursor validity while the referenced bucket remains retained.
