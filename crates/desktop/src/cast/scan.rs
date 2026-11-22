// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! The search that works when mDNS does not: try the Cast control port on every address in this
//! PC's /24, and ask whatever answers for its name.
//!
//! Plenty of home networks drop multicast between wired and wireless clients (IGMP snooping with no
//! querier, "multicast enhancement" on the access point), so a TV that Avahi has cached can still be
//! deaf to a fresh mDNS question. The TCP connect and an HTTP GET are plain unicast and get through.
//!
//! A Cast device describes itself at `http://<ip>:8008/setup/eureka_info`: `name` is its Google Home
//! name and `ssdp_udn` is its id in UUID form — the same id its mDNS record carries, minus the dashes —
//! so a device found both ways is recognised as one.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::CastDevice;

const CAST_PORT: u16 = 8009;
const SETUP_PORT: u16 = 8008;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(700);
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);

/// Every Cast device in `lan`'s /24 (except `lan` itself).
pub async fn scan(lan: Ipv4Addr) -> Vec<CastDevice> {
    let [a, b, c, _] = lan.octets();
    let probes = (1..=254u8)
        .map(|d| Ipv4Addr::new(a, b, c, d))
        .filter(|ip| *ip != lan)
        .map(|ip| tokio::spawn(probe(ip)));
    let mut found = Vec::new();
    for p in probes.collect::<Vec<_>>() {
        if let Ok(Some(dev)) = p.await {
            found.push(dev);
        }
    }
    found
}

/// The device at `ip`, if it has an open Cast port.
async fn probe(ip: Ipv4Addr) -> Option<CastDevice> {
    let addr = SocketAddr::new(IpAddr::V4(ip), CAST_PORT);
    tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await.ok()?.ok()?;
    let (name, id) = match describe(ip).await {
        Some(d) => d,
        None => (format!("Cast device at {ip}"), ip.to_string()),
    };
    Some(CastDevice { id, name, model: String::new(), ip: IpAddr::V4(ip), port: CAST_PORT })
}

/// `(name, id)` from the device's setup endpoint.
async fn describe(ip: Ipv4Addr) -> Option<(String, String)> {
    let body = tokio::time::timeout(HTTP_TIMEOUT, async {
        let mut s = TcpStream::connect(SocketAddr::new(IpAddr::V4(ip), SETUP_PORT)).await.ok()?;
        // HTTP/1.1: at least one Google TV build never answers an HTTP/1.0 request.
        let req = format!(
            "GET /setup/eureka_info HTTP/1.1\r\nHost: {ip}:{SETUP_PORT}\r\nConnection: close\r\n\r\n"
        );
        s.write_all(req.as_bytes()).await.ok()?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = s.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if response_complete(&buf) || buf.len() > 256 * 1024 {
                break;
            }
        }
        Some(buf)
    })
    .await
    .ok()??;
    parse_eureka(&body)
}

/// Whether `buf` holds the headers and the whole `Content-Length` body (the device may keep the
/// connection open despite `Connection: close`).
fn response_complete(buf: &[u8]) -> bool {
    let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { return false };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
    head.lines()
        .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
        .is_some_and(|len| buf.len() >= head_end + 4 + len)
}

fn parse_eureka(response: &[u8]) -> Option<(String, String)> {
    let start = response.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let v: serde_json::Value = serde_json::from_slice(&response[start..]).ok()?;
    let name = v.get("name")?.as_str()?.trim().to_string();
    let id: String = v.get("ssdp_udn")?.as_str()?.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    (!name.is_empty() && !id.is_empty()).then(|| (name, id.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_name_and_id_from_eureka_info() {
        let resp = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n\
            {\"name\":\"Living Room TV\",\"ssdp_udn\":\"2b577a5d-7c24-24cd-29d8-90d6b52626a6\"}";
        let (name, id) = parse_eureka(resp).unwrap();
        assert_eq!(name, "Living Room TV");
        assert_eq!(id, "2b577a5d7c2424cd29d890d6b52626a6", "matches the id in the device's mDNS record");
    }

    /// Scans this PC's real /24: `cargo test -- --ignored scan_live --nocapture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn scan_live() {
        let lan = newfoundsync_core::net::primary_lan_ipv4().expect("on a LAN");
        for d in scan(lan).await {
            println!("{d:?}");
        }
    }

    #[test]
    fn knows_when_the_body_is_in() {
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc"));
        assert!(response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabcde"));
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\nContent-Len"));
    }

    #[test]
    fn rejects_other_responses() {
        assert!(parse_eureka(b"HTTP/1.1 404 Not Found\r\n\r\nnope").is_none());
        assert!(parse_eureka(b"no header end").is_none());
    }
}
