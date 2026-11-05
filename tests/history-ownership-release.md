# History ownership release regression

The coordinated Hyprland release build exposed two history reopen failures in
Nix's release-profile unit tests. Dropping the database owner only closed its lock
descriptor. A concurrent process spawn can inherit that descriptor between fork
and exec, keeping the old owner's lock alive after its database has closed.

`dropping_owner_releases_lock_even_with_an_inherited_descriptor` reproduces this
deterministically by cloning the descriptor. It failed before the fix. It now
checks that a live owner excludes another opener, dropping that owner permits
reopening while the clone remains alive, and closing the old clone cannot release
the new owner's lock.

An ownership guard explicitly unlocks after the connection and database fields
drop. It is installed immediately after successful lock acquisition, including
failed-open cleanup paths. The lock file, exclusive acquisition, and database
safety checks remain intact.

Validation on 2026-11-05: `cargo fmt --check`, warning-denied all-targets Clippy,
the deterministic regression, and `cargo test --locked` passed (66 unit tests and
20 private-session integration tests; hardware/renderer opt-ins remain separate).
The coordinated Nix checks validate the release profile before rollout.
