# app-daemon

Rust application catalog, Hyprland window identity, process-tree CPU and resident-memory accounting, and activation policy for the Shelllist launcher.

```sh
nix develop
cargo test
nix build
```

`app-daemon daemon` exports `org.laufan.AppDaemon`; `app-daemon client` bridges JSONL requests to the session service using `app-api` v1. Resource collection is isolated behind an injectable Linux provider so procfs, cgroup, and energy edge cases can be tested without relying on the host.

Application execution returns an accepted operation immediately. Subscribe to `applications.operation` for `running`, `completed`, `failed`, or `cancelled` updates, and cancel an active operation through the transport's existing `cancel` request. Passing `expected_revision` rejects stale actions; `move-to-workspace` also accepts `window_id` and `workspace_id`.

Graphical launches, terminal applications, and desktop actions pass their desktop IDs directly to `uwsm-app -t service` (the fast, drop-in client for `uwsm app`) and run in `app-graphical.slice`. Service-mode handoff returns after the application executable starts instead of keeping the operation open for the process lifetime. This lets UWSM interpret `Terminal`, `Path`, and desktop-action metadata itself. The handoff is bounded to ten seconds; direct `gtk-launch`, `xdg-terminal-exec`, or parsed-command execution is asynchronous and used only when UWSM is unavailable.

Window identity first uses a specific UWSM application cgroup when its generated unit maps unambiguously to a desktop ID, then falls back to `StartupWMClass`, desktop-ID, and unique reverse-DNS suffix matching. This keeps terminal-hosted and chooser-based applications attached to their launcher row instead of charging them to the terminal or a generic window group. Desktop entries marked `X-Shelllist-LaunchOnly=true` are exposed as `desktop-shortcut` results and intentionally never claim windows or resources.

Application queries rank exact names, desktop IDs, prefixes, substrings, metadata, and short acronyms in descending tiers, and can filter the five Shelllist categories: Shell, Browser, Code, Media, and Text. Results expose `match_score`, `match_kind`, `runtime_score`, and the compatible combined `score`, so launchers can explain or customize their ordering.

Per-application category preferences are persisted in `$XDG_CONFIG_HOME/app-daemon/application-settings-v1.json` (or `~/.config/...`) through `applications.settings.update`. Categories map directly to default workspaces: Shell→1, Browser→2, Code→3, Media→4, and Text→5. The selected workspace overrides launch context; the daemon identifies and silently moves only the newly created window, leaving existing instances in place.

## Resource metrics

The daemon samples active applications every two seconds, independently of API queries. A specific systemd/Flatpak application cgroup is used to discover members when one is available; otherwise the Hyprland window PID and its descendants are used. Every result includes its attribution method, sample interval, process coverage, capability flags, and whether processes are shared by multiple application targets.

`cpu_percent` follows `top` semantics (100% is one logical CPU), while `cpu_percent_of_machine` is normalized to the whole machine. Cgroup `cpu.stat` and `io.stat` deltas are preferred for specific application scopes, preserving completed work from short-lived children; PID start times and `/proc/stat` deltas provide the fallback and avoid PID-reuse errors and wall-clock/suspend skew. Process and thread counts plus major-fault rates are also reported. Procfs sampling is two-stage: the daemon reads lightweight process identity and CPU fields globally, then reads expensive memory, I/O, fd, and DRM details only for attributed processes. Expensive per-process and per-application reads use a bounded worker pool.

Memory prefers proportional set size from `/proc/<pid>/smaps_rollup`, avoiding repeated charging of shared pages in multi-process applications. RSS is the fallback. RSS, PSS, private memory, and swap remain separately available, and `memory_source` identifies the source used by the compatible `memory_bytes` field.

GPU usage comes from DRM client counters in `/proc/<pid>/fdinfo`. DRM clients duplicated across file descriptors or processes are counted once. `gpu_percent` is aggregate engine occupancy and may exceed 100%; `gpu_busy_percent` is the busiest engine and is capped at 100%. Resident and allocated GPU memory are reported separately. Capability metadata distinguishes an idle supported GPU from unavailable DRM accounting.

Physical storage I/O comes from `/proc/<pid>/io`; logical cached I/O, operation counts, cancelled writes, and normalized rates are also exposed. Open and memory-mapped files are deduplicated by device and inode. Referenced-file footprint is split between temporary/cache paths and other files.

Application-owned disk space is measured separately by scanning matching directories under XDG config, data, state, cache, runtime, and Flatpak application roots. `disk_space_permanent_bytes` covers config/data/state, `disk_space_temporary_bytes` covers cache/runtime data, and `disk_space_total_bytes` is their sum. This is application data footprint, not package-installed size; unidentified directories and arbitrary `/tmp` names are intentionally not guessed. Directory measurements refresh every 30 seconds.

Network connection count is derived from unique sockets held by the attributed processes. Receive and transmit rates aggregate Linux INET_DIAG lifetime counters for the application's known TCP socket inodes, then report interval deltas; UDP and Unix sockets remain connection-only because Linux does not expose equivalent per-socket lifetime byte counters. Network-namespace totals are never misreported as process traffic.

Energy remains an estimate. Linux powercap/RAPL package energy is attributed by observed CPU-time share and marked low confidence. Battery discharge is exposed only as system power context because it includes the display, radios, storage, and idle losses; it is no longer assigned to individual applications. `energy_source`, `energy_confidence`, and `attributed_fraction` describe every value.

Some kernels expose RAPL `energy_uj` counters as root-only. The unprivileged user daemon reports energy as unavailable rather than inventing a value. On NixOS, access can be explicitly granted to desktop users in the `video` group (this relaxes the kernel's energy-counter side-channel protection):

```nix
services.udev.extraRules = ''
  ACTION=="add|change", SUBSYSTEM=="powercap", TEST=="energy_uj", ATTR{enabled}="1", RUN+="${pkgs.coreutils}/bin/chgrp video /sys%p/energy_uj", RUN+="${pkgs.coreutils}/bin/chmod 0440 /sys%p/energy_uj"
'';
```

Apply the rule by rebooting or by retriggering the `powercap` subsystem after rebuilding. Power is otherwise available only while a battery reports an actual discharge rate; charging and AC-only measurements are not treated as system consumption.

Resource history is aligned to 15-second wall-clock buckets and retained for 24 hours in `$XDG_STATE_HOME/app-daemon/resource-history-v1.json` (or `~/.local/state/...`). Points include averages, peaks, sample count, coverage, and mixed-source metadata. A compact one-minute application-energy ledger in the same file is retained for seven days and powers `applications.energyOverview` without keeping a week of full resource samples. Expired partial buckets are finalized even after an application exits.

History is returned oldest-first. The response includes an opaque `next_cursor`; pass it back to retrieve the next page or poll for points recorded after the last response:

```json
{"target_id":"org.example.App.desktop","since_ms":0,"cursor":null,"limit":1000}
```

For a sorted energy summary across applications, call `applications.energyOverview` with `{"since_ms":0,"limit":20}`. The response contains attributed mWh, relative shares, desktop names/icons, and source/confidence metadata. It includes only energy the sampler can attribute (currently RAPL CPU-time share).

Cursors are versioned and bound to their target application. Invalid, stale-format, or cross-target cursors produce a validation error.
