// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Google Cast: play the stream on a TV or speaker that has Cast built in.
//!
//! The server is the Cast *sender*. It finds Cast devices on the network ([`mdns`], and [`scan`] for
//! networks that drop multicast), and on request
//! opens the device's Cast control channel ([`v2`]) and asks it to launch the operator's registered
//! Cast app, a "custom receiver" whose page is this server's own `/cast` (served on a second,
//! plain-HTTP port, since a Cast device cannot accept the self-signed certificate). That page is the
//! normal web client in a hands-free mode: it starts by itself, takes PCM, and joins the sync like any
//! other device, so its volume and sync are set in the server window's device list.
//!
//! Google only lets a device open an unpublished receiver once the device's serial number is
//! registered against the app in the Cast developer console; until then a launch fails, and the
//! server window says so.

pub mod manager;
pub mod mdns;
pub mod scan;
pub mod v2;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

pub use manager::{CastManager, SessionState};

/// A Google Cast device found on the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CastDevice {
    /// The device's own id (the `id` its mDNS record carries): stable across reboots and address
    /// changes.
    pub id: String,
    /// The name the owner gave it in the Google Home app, e.g. "Living Room TV".
    pub name: String,
    /// The model, e.g. "Chromecast", "Google TV Streamer", "Google Nest Mini".
    pub model: String,
    pub ip: IpAddr,
    /// The Cast control port (8009 on nearly everything; speaker groups use others).
    pub port: u16,
}

/// The devices found so far, by address. Shared with the web server so a Cast page that connects can
/// be told which device it runs on.
pub type CastDirectory = Arc<Mutex<HashMap<IpAddr, CastDevice>>>;

/// The port the Cast page is served from: the HTTPS port plus one (47001 by default).
pub fn receiver_port(https_port: u16) -> u16 {
    https_port.checked_add(1).unwrap_or(https_port - 1)
}

/// The address to register as the receiver URL in Google's Cast console.
pub fn receiver_url(host: &str, https_port: u16) -> String {
    format!("http://{host}:{}/cast", receiver_port(https_port))
}

/// The stable client id a Cast device's page uses, so its name, volume and sync are remembered like
/// any other device's. Only characters a settings key accepts are kept.
pub fn client_id(device_id: &str) -> String {
    let tail: String = device_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(56)
        .collect();
    format!("cast-{tail}")
}

/// A Cast app id as typed into the server window, or `None` if it cannot be one. They are short
/// upper-case hex codes (`CC1AD845`); spaces and case are forgiven.
pub fn parse_app_id(s: &str) -> Option<String> {
    let id: String = s.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_uppercase();
    (!id.is_empty() && id.len() <= 16 && id.chars().all(|c| c.is_ascii_hexdigit())).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_is_a_valid_settings_key() {
        let id = client_id("8a3b5c7d9e0f1a2b3c4d5e6f7a8b9c0d");
        assert_eq!(id, "cast-8a3b5c7d9e0f1a2b3c4d5e6f7a8b9c0d");
        assert!(crate::settings::device_id_ok(&id));
        let from_ip = client_id("192.168.1.20");
        assert_eq!(from_ip, "cast-192-168-1-20");
        assert!(crate::settings::device_id_ok(&from_ip));
        assert!(crate::settings::device_id_ok(&client_id(&"x".repeat(200))));
    }

    #[test]
    fn app_id_parsing() {
        assert_eq!(parse_app_id(" a1b2 c3d4 ").as_deref(), Some("A1B2C3D4"));
        assert_eq!(parse_app_id(""), None);
        assert_eq!(parse_app_id("not-hex"), None);
    }

    #[test]
    fn receiver_url_is_the_next_port() {
        assert_eq!(receiver_url("192.168.1.5", 47000), "http://192.168.1.5:47001/cast");
        assert_eq!(receiver_port(u16::MAX), u16::MAX - 1);
    }
}
