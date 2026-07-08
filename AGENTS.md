# AGENTS.md

BrainDrain is a Rust workspace with platform frontends:

- `crates/core`: shared provider/domain model.
- `crates/service`: provider registry/dispatch.
- `crates/daemon`: session D-Bus daemon plus in-process cache type.
- `crates/desktop`: Linux desktop integration helpers shared by CLI/GUI.
- `crates/cli`: `braindrain` CLI.
- `apps/linux`: GTK/libadwaita Relm4 GUI app (`braindrain-gui`).
- `apps/macos`: SwiftUI menu-bar app.
- `apps/plasma`: KDE Plasma widget.

On Linux, the daemon is the single source of truth: once `daemon install` has
been run, it is D-Bus auto-activatable, so any client call on the bus name
`dev.sargunv.BrainDrain1` will start it on demand. GUI clients (GTK app, Plasma
widget) talk to it over the session bus and surface an install prompt when it
isn't reachable; there is no in-process fallback.
