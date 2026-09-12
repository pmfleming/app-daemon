# shelllist-hyprland

Bounded Hyprland IPC discovery, command, and event transport shared by Shelllist's Rust daemons.

`Client::request` uses the command socket directly with a two-second deadline. `watch_events_detailed` reports Connected, Disconnected, and raw Message events so consumers can filter domain changes and adapt fallback polling. Reconnection is delayed by one second, including accept-then-EOF failures; closing the receiver stops the watcher. The original unit-valued `watch_events` API remains available.

Domain models remain in their owning daemons. Shelllist keeps compositor presentation such as layer rules.
