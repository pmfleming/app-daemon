# app-daemon

Rust application catalog, Hyprland window identity, process-tree CPU and resident-memory accounting, and activation policy for the Shelllist launcher.

```sh
nix develop
cargo test
nix build
```

`app-daemon daemon` exports `org.laufan.AppDaemon`; `app-daemon client` bridges JSONL requests to the session service using `app-api` v1. Resource collection is isolated behind an injectable Linux provider so procfs, cgroup, and energy edge cases can be tested without relying on the host.

Application execution returns an accepted operation immediately. Subscribe to `applications.operation` for `running`, `completed`, `failed`, or `cancelled` updates, and cancel an active operation through the transport's existing `cancel` request. Passing `expected_revision` rejects stale actions; `move-to-workspace` also accepts `window_id` and `workspace_id`.

Graphical launches, terminal applications, and desktop actions prefer `uwsm-app` scopes under `app-graphical.slice`, falling back to direct execution when UWSM is unavailable.

## Resource metrics

The daemon samples active applications every two seconds, independently of API queries. A specific systemd/Flatpak application cgroup is used to discover members when one is available; otherwise the Hyprland window PID and its descendants are used. Every result includes its attribution method, sample interval, process coverage, capability flags, and whether processes are shared by multiple application targets.

`cpu_percent` follows `top` semantics (100% is one logical CPU), while `cpu_percent_of_machine` is normalized to the whole machine. Cgroup `cpu.stat` and `io.stat` deltas are preferred for specific application scopes, preserving completed work from short-lived children; PID start times and `/proc/stat` deltas provide the fallback and avoid PID-reuse errors and wall-clock/suspend skew. Process and thread counts plus major-fault rates are also reported. Procfs sampling is two-stage: the daemon reads lightweight process identity and CPU fields globally, then reads expensive memory, I/O, fd, and DRM details only for attributed processes. Expensive per-process and per-application reads use a bounded worker pool.

Memory prefers proportional set size from `/proc/<pid>/smaps_rollup`, avoiding repeated charging of shared pages in multi-process applications. RSS is the fallback. RSS, PSS, private memory, and swap remain separately available, and `memory_source` identifies the source used by the compatible `memory_bytes` field.

GPU usage comes from DRM client counters in `/proc/<pid>/fdinfo`. DRM clients duplicated across file descriptors or processes are counted once. `gpu_percent` is aggregate engine occupancy and may exceed 100%; `gpu_busy_percent` is the busiest engine and is capped at 100%. Resident and allocated GPU memory are reported separately. Capability metadata distinguishes an idle supported GPU from unavailable DRM accounting.

Physical storage I/O comes from `/proc/<pid>/io`; logical cached I/O, operation counts, cancelled writes, and normalized rates are also exposed. Open and memory-mapped files are deduplicated by device and inode. Referenced-file footprint is split between temporary/cache paths and other files.

Application-owned disk space is measured separately by scanning matching directories under XDG config, data, state, cache, runtime, and Flatpak application roots. `disk_space_permanent_bytes` covers config/data/state, `disk_space_temporary_bytes` covers cache/runtime data, and `disk_space_total_bytes` is their sum. This is application data footprint, not package-installed size; unidentified directories and arbitrary `/tmp` names are intentionally not guessed. Directory measurements refresh every 30 seconds.

Network connection count is derived from unique sockets held by the attributed processes. Per-application network byte accounting is explicitly marked unavailable until an optional cgroup eBPF collector is present; network-namespace totals are not misreported as process traffic.

Energy remains an estimate. Linux powercap/RAPL package energy is attributed by observed CPU-time share and marked low confidence. Battery discharge is exposed only as system power context because it includes the display, radios, storage, and idle losses; it is no longer assigned to individual applications. `energy_source`, `energy_confidence`, and `attributed_fraction` describe every value.

Resource history is aligned to 15-second wall-clock buckets and retained for 24 hours in `$XDG_STATE_HOME/app-daemon/resource-history-v1.json` (or `~/.local/state/...`). Points include averages, peaks, sample count, coverage, and mixed-source metadata. Expired partial buckets are finalized even after an application exits.

History is returned oldest-first. The response includes an opaque `next_cursor`; pass it back to retrieve the next page or poll for points recorded after the last response:

```json
{"target_id":"org.example.App.desktop","since_ms":0,"cursor":null,"limit":1000}
```

Cursors are versioned and bound to their target application. Invalid, stale-format, or cross-target cursors produce a validation error.
