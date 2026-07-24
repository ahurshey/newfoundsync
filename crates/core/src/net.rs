// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Local-network address helpers.
//!
//! This is what survived `discovery.rs`. That module implemented mDNS advertise/browse for a
//! multi-server, three-UDP-port architecture the project abandoned in favour of a single HTTPS +
//! WebSocket server that clients reach by URL (or QR code). Only the "which of my IPs is the real LAN
//! one" helper was ever called, so the rest — and the `mdns-sd` dependency it pulled into every
//! build — was removed. If LAN auto-discovery is ever wanted again, start fresh against the WebSocket
//! design rather than restoring code written for the old one (`git log -- crates/core/src/discovery.rs`).

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

/// The primary LAN IPv4 — the source address the OS picks to reach the outside, i.e. the real
/// default-route interface, NOT a VirtualBox/Hyper-V/WSL host-only adapter or a `169.254.x`
/// link-local. We "connect" a UDP socket (no packet is actually sent) purely to make the kernel
/// select the outbound interface.
pub fn primary_lan_ipv4() -> Option<Ipv4Addr> {
    // 8.8.8.8 is just a routing hint; works offline too as long as the LAN has a default route (a
    // normal home/office router). Fall back if there's none.
    for hint in ["8.8.8.8:53", "192.168.1.1:9", "1.1.1.1:53"] {
        if let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)) {
            if sock.connect(hint).is_ok() {
                if let Ok(addr) = sock.local_addr() {
                    if let IpAddr::V4(ip) = addr.ip() {
                        if !ip.is_loopback() && !ip.is_unspecified() && !ip.is_link_local() {
                            return Some(ip);
                        }
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment-dependent by nature (CI containers may have no default route), so this asserts the
    /// invariant that must hold WHENEVER an address is returned, rather than requiring one.
    #[test]
    fn primary_lan_ipv4_is_sane_when_present() {
        if let Some(ip) = primary_lan_ipv4() {
            assert!(!ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified());
        }
    }
}
