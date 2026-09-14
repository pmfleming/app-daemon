# shelllist-hyprland

Bounded Hyprland IPC discovery, command, and event transport shared by Shelllist's Rust daemons.

`Client::request` uses the command socket directly with a two-second deadline. `watch_events_detailed` reports Connected, Disconnected, and raw Message events so consumers can filter domain changes and adapt fallback polling. Reconnection is delayed by one second, including accept-then-EOF failures; closing the receiver stops the watcher. The original unit-valued `watch_events` API remains available.

`Client::work_areas` uses one native JSON batch and resolves workspace rules into
monitor-keyed logical insets. Parsing, monitor selectors, window/group counts and
rule-order semantics are tested here. Replies are bounded to 16 MiB; invalid
snapshots fail explicitly. `work_area::geometry_event` identifies invalidating
compositor events. bar-daemon owns the shared on-demand cache and subscriber
lifetime, rather than creating a poller per UI surface.

Other domain models remain in their owning daemons. Shelllist keeps QScreen size,
pixel clamping, window placement and presentation-specific layer rules.
