# Named workspace launch routing

With temporary workspaces disabled, explicit `current` launches resolve the
active workspace's address instead of requiring a number. Default launches
already returned before reading the active workspace: the review's claim that
this unnecessary read could block them does not apply to the reviewed HEAD.
Placement confirmation compares address identity when present, so a renamed
workspace display cannot cause false failure or wrong confirmation.

Validation: full locked app tests passed; a new private bus/process test then
passed through admission, launch, move, compositor confirmation and recovered
placement for a named current workspace. A captured-JSON unit regression covers
display-name changes. Mock moves now represent named workspaces without synthetic
negative IDs. `cargo fmt` and diff whitespace checks pass. Logs:
`/tmp/app-ipc-review-named.log` and `/tmp/app-ipc-review-named-launch.log`.

Temporary lifecycle reservations still require numbered owner contexts. This
change does not turn owner errors into permission to bypass lifecycle guards.
