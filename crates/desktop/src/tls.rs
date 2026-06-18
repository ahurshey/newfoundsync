//! Self-signed TLS for the web server.
//!
//! Browsers only expose WebCodecs (and Wake Lock, etc.) in a *secure context* —
//! HTTPS or `localhost`. A plain `http://192.168.x.x` LAN URL is "insecure", so the
//! decoder is hidden and nothing plays on phones / other PCs. There's no public CA
//! that will certify a private LAN IP, so we mint our own self-signed certificate
//! (persisted, with the LAN IP + hostname in the SAN). The user accepts a one-time
//! "not private — proceed" prompt per device; after that the page is a secure
//! context and WebCodecs works.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

use newfoundsync_core::discovery;

/// Where the persisted cert/key live (stable across restarts → accept once/device).
fn cert_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("Newfoundsync")
}

/// Load the persisted cert/key (PEM), or generate + persist a fresh self-signed
/// pair. Returns `(cert_pem, key_pem)`.
pub fn load_or_create_cert() -> Result<(Vec<u8>, Vec<u8>)> {
    let dir = cert_dir();
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    if let (Ok(cert), Ok(key)) = (fs::read(&cert_path), fs::read(&key_path)) {
        if !cert.is_empty() && !key.is_empty() {
            return Ok((cert, key));
        }
    }

    // Subject Alternative Names: the LAN IP + hostname + localhost. (For a
    // self-signed cert the SAN doesn't remove the warning, but it's correct.)
    let mut sans: Vec<String> = vec!["localhost".into(), "127.0.0.1".into()];
    if let Some(ip) = discovery::primary_lan_ipv4() {
        sans.push(ip.to_string());
    }
    if let Ok(host) = std::env::var("COMPUTERNAME") {
        if !host.is_empty() {
            sans.push(host.clone());
            sans.push(format!("{host}.local"));
        }
    }

    let certified =
        rcgen::generate_simple_self_signed(sans).context("generate self-signed certificate")?;
    let cert_pem = certified.cert.pem().into_bytes();
    let key_pem = certified.signing_key.serialize_pem().into_bytes();

    // Best-effort persist; if the dir isn't writable we still serve this session.
    if fs::create_dir_all(&dir).is_ok() {
        let _ = fs::write(&cert_path, &cert_pem);
        let _ = fs::write(&key_path, &key_pem);
    }

    Ok((cert_pem, key_pem))
}
