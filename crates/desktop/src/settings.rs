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
    std::fs::write(&path, body).map_err(|e| e.to_string())
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
