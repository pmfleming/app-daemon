# app-daemon

Rust application catalog, Hyprland window identity, process-tree CPU and resident-memory accounting, and activation policy for the Shelllist launcher.

```sh
nix develop
cargo test
nix build
```

`app-daemon daemon` exports `org.laufan.AppDaemon`; `app-daemon client` bridges JSONL requests to the session service using `app-api` v1.

## Resource metrics

The daemon samples active application process trees every two seconds, independently of API queries. `cpu_percent` follows `top` semantics (100% is one logical CPU), while `cpu_percent_of_machine` is normalized to the whole machine. PID start times and `/proc/stat` deltas are used to avoid PID-reuse errors and wall-clock/suspend skew.

GPU usage comes from per-process DRM client counters in `/proc/<pid>/fdinfo`. `gpu_percent` uses 100% for one fully occupied GPU engine and may exceed 100% when multiple engines are busy; `gpu_memory_bytes` prefers resident DRM memory and falls back to allocated memory. Unsupported drivers or inaccessible fdinfo report zero.

Storage I/O comes from `/proc/<pid>/io`. `disk_read_bytes` and `disk_write_bytes` are block-storage bytes completed during the sample, so cached reads are intentionally excluded; the corresponding `*_per_second` fields normalize them by sample duration. `open_file_disk_bytes` is the allocated size of unique regular files currently open by an app's process tree. It is a live disk-footprint indicator—not the app's installed size—and shared files are counted once per application.

Energy is an estimate: package energy from Linux powercap/RAPL is preferred and battery discharge power is the fallback. It is attributed by each process tree's share of observed CPU and GPU engine activity. `battery_percent` is the interval's estimated energy divided by full battery capacity; `battery_percent_per_hour` is the equivalent sustained drain rate. `energy_source` is `rapl`, `battery`, or `unavailable`.

CPU, GPU, resident CPU/GPU memory, storage I/O, open-file disk footprint, energy, and battery-equivalent history for active applications is aggregated into 15-second points and retained for 24 hours in `$XDG_STATE_HOME/app-daemon/resource-history-v1.json` (or `~/.local/state/...`). Query it with `applications.history`:

```json
{"target_id":"org.example.App.desktop","since_ms":0,"limit":1000}
```
