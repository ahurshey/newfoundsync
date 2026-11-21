// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Tiny persisted server settings: what the operator last chose in the server window (port, source,
//! delay, sound quality, theme, text size) and what it knows about each device (name, volume, sync).
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
    update(|map| {
        map.insert(key.to_string(), value.to_string());
    })
}

/// One saved value, as text. `None` if it was never saved.
pub fn get(key: &str) -> Option<String> {
    load_all().get(key).cloned().filter(|v| !v.is_empty())
}

/// Save several values in ONE rewrite of the file. Values must not contain a newline.
pub fn set_many(pairs: &[(&str, String)]) -> Result<(), String> {
    update(|map| {
        for (k, v) in pairs {
            map.insert((*k).to_string(), v.replace(['\n', '\r'], " "));
        }
    })
}

/// Read-modify-write the settings file through `edit`.
fn update(edit: impl FnOnce(&mut BTreeMap<String, String>)) -> Result<(), String> {
    let path = settings_path().ok_or_else(|| "no config directory available".to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut map = load_all();
    edit(&mut map);
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
/// Called by the Windows GUI and by the Linux screencast Flatpak's hardware-encoder picker.
pub fn save_encode_device(d: newfoundsync_core::video::EncodeDevice) -> Result<(), String> {
    save_key("encode_device", d.as_str())
}

/// The saved A/V offset in milliseconds (audio against video). `None` ⇒ 0.
///
/// Worth persisting because it is a property of the MACHINE — its sound card, its compositor, the
/// buffering of whatever plays the media — not of a session. Having to re-find it by ear on every
/// launch would make it useless.
pub fn load_av_offset_ms() -> Option<i32> {
    load_all().get("av_offset_ms").and_then(|v| v.parse::<i32>().ok()).map(|v| v.clamp(-500, 500))
}

/// Persist the A/V offset (preserving other settings).
pub fn save_av_offset_ms(ms: i32) -> Result<(), String> {
    save_key("av_offset_ms", &ms.clamp(-500, 500).to_string())
}

/// The apps the operator ticked to send to the "Newfoundsync" output device (Linux), by name.
///
/// Persisted because the tick is a statement about the APP ("my music player goes to the stream"),
/// not about a session: having to find and re-tick it on every launch is the chore the list exists
/// to remove.
pub fn load_routed_apps() -> Vec<String> {
    load_all()
        .get("routed_apps")
        .map(|v| v.split('|').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

/// Persist the ticked apps (preserving other settings). A name that could not survive the
/// `key=value` / `|`-separated file is left out rather than allowed to corrupt its neighbours; that
/// app simply has to be ticked again next launch.
pub fn save_routed_apps<'a>(apps: impl IntoIterator<Item = &'a String>) -> Result<(), String> {
    let kept: Vec<&str> = apps
        .into_iter()
        .map(|s| s.as_str())
        .filter(|s| !s.contains(['|', '\n', '\r']) && s.trim() == *s && !s.is_empty())
        .collect();
    save_key("routed_apps", &kept.join("|"))
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

// ---- what the server remembers about each device --------------------------------------------------

/// Everything the server window knows about one device, kept across restarts.
///
/// Before this existed the mixer lived only in the window's memory: closing the server forgot every
/// room's name, level and sync offset, and each had to be set again by hand.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceMemo {
    /// The name to show for it.
    pub name: String,
    /// True when `name` was typed in the server window. It then wins over whatever the device calls
    /// itself; when false the name is just the device's own, remembered so it can be listed offline.
    pub custom_name: bool,
    /// Volume before the master is applied, 0.0..=1.0.
    pub vol: f32,
    /// Server sync offset in ms.
    pub trim_ms: i32,
    pub muted: bool,
    /// Unix seconds when it was last connected.
    pub last_seen: u64,
}

/// Most devices kept. The oldest by `last_seen` are dropped beyond this.
const MAX_DEVICES: usize = 64;
const DEVICE_PREFIX: &str = "dev.";

/// Whether a device's id can be used as part of a settings key.
///
/// The id is chosen by the browser and arrives over the LAN, so it is untrusted: anything that is not
/// a short plain token is simply not remembered, rather than escaped and written into the file.
pub fn device_id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn encode_device(m: &DeviceMemo) -> String {
    // The name goes LAST so it may contain the separator. Control characters would break the
    // line-per-key file, and nobody needs them in a device name.
    let name: String = m.name.chars().filter(|c| !c.is_control()).take(64).collect();
    format!(
        "{}|{}|{}|{}|{}|{}",
        (m.vol.clamp(0.0, 1.0) * 1000.0).round() as i32,
        m.trim_ms.clamp(-500, 500),
        m.muted as u8,
        m.last_seen,
        m.custom_name as u8,
        name
    )
}

fn decode_device(v: &str) -> Option<DeviceMemo> {
    let mut it = v.splitn(6, '|');
    let vol = it.next()?.parse::<i32>().ok()?.clamp(0, 1000) as f32 / 1000.0;
    let trim_ms = it.next()?.parse::<i32>().ok()?.clamp(-500, 500);
    let muted = it.next()? == "1";
    let last_seen = it.next()?.parse::<u64>().ok()?;
    let custom_name = it.next()? == "1";
    let name = it.next()?.to_string();
    Some(DeviceMemo { name, custom_name, vol, trim_ms, muted, last_seen })
}

/// Every remembered device, by its stable id.
pub fn load_devices() -> std::collections::HashMap<String, DeviceMemo> {
    load_all()
        .iter()
        .filter_map(|(k, v)| {
            let id = k.strip_prefix(DEVICE_PREFIX)?;
            device_id_ok(id).then(|| decode_device(v).map(|m| (id.to_string(), m))).flatten()
        })
        .collect()
}

/// Replace the remembered devices with `devices` (preserving every other setting).
pub fn save_devices(devices: &std::collections::HashMap<String, DeviceMemo>) -> Result<(), String> {
    let mut keep: Vec<(&String, &DeviceMemo)> =
        devices.iter().filter(|(id, _)| device_id_ok(id)).collect();
    keep.sort_by(|a, b| b.1.last_seen.cmp(&a.1.last_seen));
    keep.truncate(MAX_DEVICES);
    update(|map| {
        map.retain(|k, _| !k.starts_with(DEVICE_PREFIX));
        for (id, m) in keep {
            map.insert(format!("{DEVICE_PREFIX}{id}"), encode_device(m));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memo(name: &str) -> DeviceMemo {
        DeviceMemo { name: name.into(), custom_name: true, vol: 0.85, trim_ms: -35, muted: true, last_seen: 1_795_000_000 }
    }

    #[test]
    fn a_device_survives_the_round_trip() {
        let m = memo("Living room TV");
        assert_eq!(decode_device(&encode_device(&m)), Some(m));
    }

    #[test]
    fn a_name_may_contain_the_separator_and_an_equals_sign() {
        // The name is the last field precisely so this works; `=` is safe because a settings line is
        // split at its FIRST one, which belongs to the key.
        let m = memo("Den | left = loud");
        assert_eq!(decode_device(&encode_device(&m)), Some(m));
    }

    #[test]
    fn a_name_cannot_break_the_one_line_per_key_file() {
        let enc = encode_device(&memo("Kitchen\nport=1\r"));
        assert!(!enc.contains('\n') && !enc.contains('\r'));
        assert_eq!(decode_device(&enc).unwrap().name, "Kitchenport=1");
    }

    #[test]
    fn out_of_range_values_are_clamped_not_trusted() {
        let m = decode_device("5000|9999|1|7|0|x").unwrap();
        assert_eq!((m.vol, m.trim_ms), (1.0, 500));
        assert!(decode_device("not|a|device").is_none());
    }

    #[test]
    fn only_plain_ids_are_remembered() {
        // The browser's own ids look like this ...
        assert!(device_id_ok("c8k2j9x1lz3f0ab"));
        // ... and these could only come from something hand-crafted.
        for bad in ["", "a=b", "a\nb", "a b", "dev.x", &"x".repeat(65)] {
            assert!(!device_id_ok(bad), "{bad:?} must not become a settings key");
        }
    }
}
