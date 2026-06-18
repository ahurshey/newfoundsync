//! Audio capture. v1 ships system-wide loopback (the whole output mix) via cpal's
//! WASAPI loopback (building an *input* stream on the default *output* device,
//! which cpal turns into a loopback capture and COM-initializes itself).
//!
//! Per-application capture (WASAPI `ActivateAudioInterfaceAsync` +
//! `PROCESS_LOOPBACK`) and an app picker are the v2 follow-up (`app.rs` +
//! `enumerate.rs`) — kept out of v1 so capture is the only unproven surface.

pub mod system;

#[cfg(target_os = "windows")]
pub mod process;
#[cfg(target_os = "windows")]
pub mod sessions;

#[allow(unused_imports)]
pub use system::SystemCapture;
