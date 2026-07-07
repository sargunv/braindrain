//! Desktop integration helpers for BrainDrain: daemon systemd/D-Bus service
//! lifecycle and Plasma widget install/uninstall.
//!
//! All functionality is Linux-only. On other platforms the public modules are
//! empty and any function call would fail to compile (matching the previous
//! `ensure_linux` behavior, which bailed at runtime on non-Linux).

#[cfg(target_os = "linux")]
pub mod daemon;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub mod plasma;

#[cfg(target_os = "linux")]
mod util;
