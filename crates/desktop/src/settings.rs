// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Tiny persisted server settings (currently just the HTTP port picked in the GUI).
//!
//! Stored as a dependency-free `key=value` text file in the OS config dir:
//!   Windows: `%APPDATA%\Newfoundsync\settings.txt`
//!   else:    `$XDG_CONFIG_HOME/newfoundsync/settings.txt` (or `~/.config/...`)
//!
//! Port resolution order (see `main`): an explicit `--port` flag wins, else the saved value,
//! else [`newfoundsync_core::config::DEFAULT_HTTP_PORT`].

use std::collections::BTreeMap;
use std::path::PathBuf;

fn settings_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("Newfoundsync").join("settings.txt"))
}

/// Read the whole settings file into a key→value map (empty on any error). A `BTreeMap`
/// keeps the rewritten file stable/sorted so a save doesn't churn unrelated keys' order.
fn load_all() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Some(p) = settings_path() {
        if let Ok(txt) = std::fs::read_to_string(p) {
            for line in txt.lines() {
                if let Some((k, v)) = line.split_once('=') {
                    map.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
        }
    }
    map
}

/// Set one key and rewrite the whole file, preserving every other key. Returns an error
/// string for the GUI to surface; we never panic on a settings write.
fn save_key(key: &str, value: &str) -> Result<(), String> {
    let path = settings_path().ok_or_else(|| "no config directory available".to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut map = load_all();
    map.insert(key.to_string(), value.to_string());
    let body: String = map.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    // Atomic write: fill a sibling temp file, then rename it over the target. An interrupted write
    // can then never leave a truncated/corrupt settings.txt — either the old file or the complete
    // new one is present. (std::fs::rename replaces the destination on Windows via MoveFileEx.)
    let tmp_path = path.with_extension("txt.tmp");
    std::fs::write(&tmp_path, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp_path, &path).map_err(|e| e.to_string())
}

/// The saved HTTP port, if a valid one was previously stored. `None` ⇒ caller uses the default.
pub fn load_port() -> Option<u16> {
    load_all()
        .get("port")
        .and_then(|v| v.parse::<u16>().ok())
        .filter(|&n| n != 0)
}

/// Persist the chosen HTTP port (preserving other settings).
pub fn save_port(port: u16) -> Result<(), String> {
    save_key("port", &port.to_string())
}

/// The saved video encode device (Auto / GPU / CPU), if one was previously stored.
/// `None` ⇒ caller uses [`EncodeDevice::Auto`], which is the historical behaviour.
pub fn load_encode_device() -> Option<newfoundsync_core::video::EncodeDevice> {
    load_all().get("encode_device").and_then(|v| newfoundsync_core::video::EncodeDevice::parse(v))
}

/// Persist the chosen video encode device (preserving other settings).
///
/// Only ever called from the GUI's encode-device picker, which is Windows-only (no other platform
/// has a GPU encoder to choose). The loader above stays cross-platform so a value set on Windows is
/// still honoured if the same config file is read elsewhere.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn save_encode_device(d: newfoundsync_core::video::EncodeDevice) -> Result<(), String> {
    save_key("encode_device", d.as_str())
}

/// The saved xdg-desktop-portal ScreenCast restore token, if a screen share was previously
/// approved. `None` ⇒ the portal will show its picker dialog.
///
/// Only meaningful on Linux, but kept unconditional like the rest of this module — it is a string
/// in a text file, and cfg-ing it would buy nothing.
pub fn load_screencast_token() -> Option<String> {
    load_all().get("screencast_token").map(|s| s.to_string()).filter(|s| !s.is_empty())
}

/// Persist the portal's restore token so the next run does not re-prompt.
///
/// The token ROTATES: the portal issues a fresh one with every Start response and invalidates the
/// old one, so this must be called on every successful session or the saved value goes stale and the
/// dialog comes back.
pub fn save_screencast_token(token: &str) -> Result<(), String> {
    save_key("screencast_token", token)
}
