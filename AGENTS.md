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

On Linux, GUI clients should prefer the D-Bus daemon and fall back to embedded
`BrainDrainDaemon<ServiceBackend>` only when the daemon is unavailable.
