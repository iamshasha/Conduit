//! OS integration, split by platform.
//!
//! This module holds the cross-platform pieces (shared types, constants, and a
//! few things that are the same everywhere). The per-OS implementations live in
//! [`windows`], [`linux`], and [`macos`]; exactly one is re-exported below so
//! the rest of the crate calls `crate::system::foo` without caring which OS it
//! is on. A function with no real backend on a platform returns an
//! `"unsupported on this platform"` error instead of pretending to work.

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

// ------------------------------------------------------------ shared types

pub struct Gpu {
    pub name: String,
    pub vendor: &'static str,
    pub vram: u64,
    pub shared: u64,
}

pub struct Battery {
    pub percent: Option<u8>,
    pub charging: bool,
    pub on_ac: bool,
}

pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// closed | opened | changing | stopped | playing | paused
    pub status: &'static str,
    /// App that owns the session, e.g. "Spotify.exe".
    pub app: String,
}

// -------------------------------------------------------------- constants

/// Names accepted by `sys.media`.
pub const MEDIA_KEYS: [&str; 7] = ["volume_up", "volume_down", "mute", "play_pause", "next", "prev", "stop"];
pub const MEDIA_ACTIONS: [&str; 6] = ["play", "pause", "toggle", "next", "prev", "stop"];
pub const POWER_ACTIONS: [&str; 6] = ["lock", "sleep", "logoff", "shutdown", "restart", "abort"];

// ---------------------------------------------------------- cross-platform

pub fn exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// CPU brand string, e.g. "AMD Ryzen 7 5800X".
pub fn cpu_model() -> String {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_cpu_all();
    sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default()
}

/// Map a PCI vendor id to a short name (used by every gpu backend).
pub(crate) fn vendor_name(id: u32) -> &'static str {
    match id {
        0x10DE => "NVIDIA",
        0x1002 | 0x1022 => "AMD",
        0x8086 => "Intel",
        0x1414 => "Microsoft",
        0x5143 => "Qualcomm",
        0x106B => "Apple",
        _ => "Unknown",
    }
}
