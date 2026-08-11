# app-daemon

Rust application catalog, Hyprland window identity, process-tree CPU and resident-memory accounting, and activation policy for the Shelllist launcher.

```sh
nix develop
cargo test
nix build
```

`app-daemon daemon` exports `org.laufan.AppDaemon`; `app-daemon client` bridges JSONL requests to the session service using `app-api` v1.
