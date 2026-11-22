// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Find Google Cast devices with one mDNS question: "who offers `_googlecast._tcp.local`?".
//!
//! The question is sent from an ordinary ephemeral port, which makes it a *legacy unicast* query
//! (RFC 6762 §6.7): responders answer straight back to that port, so nothing has to bind 5353 (where
//! Avahi or Bonjour already sit) or join the multicast group. The socket is bound to the LAN address so
//! the question leaves on the LAN interface rather than whichever one the multicast route picks (a VPN,
//! a container bridge).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use super::CastDevice;

const SERVICE: &str = "_googlecast._tcp.local";
const MDNS_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 251)), 5353);

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;

/// Ask the network for Cast devices and collect the answers for `wait`. The question is repeated a few
/// times within that window, since one lost packet would otherwise hide a device.
pub fn discover(wait: Duration) -> std::io::Result<Vec<CastDevice>> {
    let bind_ip = newfoundsync_core::net::primary_lan_ipv4().unwrap_or(Ipv4Addr::UNSPECIFIED);
    let sock = UdpSocket::bind((bind_ip, 0)).or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)))?;
    sock.set_multicast_ttl_v4(255)?;
    sock.set_read_timeout(Some(Duration::from_millis(150)))?;
    let query = build_query();

    let start = Instant::now();
    let resend_at = [Duration::ZERO, Duration::from_millis(400), Duration::from_millis(1200)];
    let mut sent = 0;
    let mut seen = Records::default();
    let mut buf = [0u8; 9000];
    while start.elapsed() < wait {
        if sent < resend_at.len() && start.elapsed() >= resend_at[sent] {
            if let Err(e) = sock.send_to(&query, MDNS_ADDR) {
                if sent == 0 {
                    return Err(e);
                }
            }
            sent += 1;
        }
        match sock.recv_from(&mut buf) {
            Ok((n, from)) => seen.absorb(&buf[..n], from.ip()),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(seen.devices())
}

/// A standard query for the PTR records of [`SERVICE`], with the "unicast response" bit set.
fn build_query() -> Vec<u8> {
    let mut q = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]; // id 0, flags 0, one question
    for label in SERVICE.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&TYPE_PTR.to_be_bytes());
    q.extend_from_slice(&0x8001u16.to_be_bytes()); // class IN, QU bit
    q
}

/// Everything learned from the answers so far, keyed by the DNS names that tie the records together.
#[derive(Default)]
struct Records {
    /// Service instance names (`<id>._googlecast._tcp.local`) → the address that answered for them.
    instances: HashMap<String, IpAddr>,
    /// Instance → (port, target host name).
    srv: HashMap<String, (u16, String)>,
    /// Instance → TXT key/values.
    txt: HashMap<String, HashMap<String, String>>,
    /// Host name → IPv4 address.
    a: HashMap<String, Ipv4Addr>,
}

impl Records {
    fn absorb(&mut self, pkt: &[u8], from: IpAddr) {
        let _ = self.try_absorb(pkt, from); // a malformed packet is simply ignored
    }

    fn try_absorb(&mut self, pkt: &[u8], from: IpAddr) -> Option<()> {
        if pkt.len() < 12 || pkt[2] & 0x80 == 0 {
            return None; // not a response
        }
        let qd = u16::from_be_bytes([pkt[4], pkt[5]]) as usize;
        let rr = [6, 8, 10].iter().map(|&i| u16::from_be_bytes([pkt[i], pkt[i + 1]]) as usize).sum::<usize>();
        let mut pos = 12;
        for _ in 0..qd {
            pos = read_name(pkt, pos)?.1 + 4;
        }
        for _ in 0..rr {
            let (name, p) = read_name(pkt, pos)?;
            let head = pkt.get(p..p + 10)?;
            let rtype = u16::from_be_bytes([head[0], head[1]]);
            let len = u16::from_be_bytes([head[8], head[9]]) as usize;
            let data_at = p + 10;
            let data = pkt.get(data_at..data_at + len)?;
            let name = name.to_ascii_lowercase();
            match rtype {
                TYPE_PTR if name == SERVICE.to_ascii_lowercase() => {
                    let (inst, _) = read_name(pkt, data_at)?;
                    self.instances.insert(inst.to_ascii_lowercase(), from);
                }
                TYPE_SRV if data.len() >= 7 => {
                    let port = u16::from_be_bytes([data[4], data[5]]);
                    let (target, _) = read_name(pkt, data_at + 6)?;
                    self.instances.entry(name.clone()).or_insert(from);
                    self.srv.insert(name, (port, target.to_ascii_lowercase()));
                }
                TYPE_TXT => {
                    self.txt.insert(name, parse_txt(data));
                }
                TYPE_A if data.len() == 4 => {
                    self.a.insert(name, Ipv4Addr::new(data[0], data[1], data[2], data[3]));
                }
                _ => {}
            }
            pos = data_at + len;
        }
        Some(())
    }

    fn devices(&self) -> Vec<CastDevice> {
        let mut out: Vec<CastDevice> = self
            .instances
            .iter()
            .filter(|(inst, _)| inst.ends_with(&SERVICE.to_ascii_lowercase()))
            .filter_map(|(inst, from)| {
                let txt = self.txt.get(inst)?;
                // Lower case, as the subnet scan reports it, so a device found both ways is one device.
                let id = txt.get("id").filter(|s| !s.is_empty())?.to_ascii_lowercase();
                let (port, ip) = match self.srv.get(inst) {
                    Some((port, host)) => (*port, self.a.get(host).map(|v4| IpAddr::V4(*v4)).unwrap_or(*from)),
                    None => (8009, *from),
                };
                Some(CastDevice {
                    name: txt.get("fn").cloned().filter(|s| !s.is_empty()).unwrap_or_else(|| id.clone()),
                    model: txt.get("md").cloned().unwrap_or_default(),
                    id,
                    ip,
                    port,
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.id.cmp(&b.id)));
        out.dedup_by(|a, b| a.id == b.id);
        out
    }
}

/// A DNS name at `pos`, following compression pointers. Returns the name and the position just past
/// it in the record (not past any pointer target).
fn read_name(pkt: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end = None;
    for _ in 0..64 {
        let len = *pkt.get(pos)? as usize;
        if len == 0 {
            return Some((labels.join("."), end.unwrap_or(pos + 1)));
        }
        if len & 0xC0 == 0xC0 {
            let target = ((len & 0x3F) << 8) | *pkt.get(pos + 1)? as usize;
            end.get_or_insert(pos + 2);
            pos = target;
            continue;
        }
        let label = pkt.get(pos + 1..pos + 1 + len)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        pos += 1 + len;
    }
    None // a pointer loop
}

/// TXT record strings (`key=value`, each length-prefixed) as a map.
fn parse_txt(data: &[u8]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut i = 0;
    while i < data.len() {
        let len = data[i] as usize;
        let Some(s) = data.get(i + 1..i + 1 + len) else { break };
        let s = String::from_utf8_lossy(s);
        if let Some((k, v)) = s.split_once('=') {
            out.insert(k.to_ascii_lowercase(), v.to_string());
        }
        i += 1 + len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(out: &mut Vec<u8>, n: &str) {
        for l in n.split('.') {
            out.push(l.len() as u8);
            out.extend_from_slice(l.as_bytes());
        }
        out.push(0);
    }

    fn record(out: &mut Vec<u8>, owner: &[u8], rtype: u16, data: &[u8]) {
        out.extend_from_slice(owner);
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&0x8001u16.to_be_bytes());
        out.extend_from_slice(&120u32.to_be_bytes());
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(data);
    }

    /// A response shaped like a Chromecast's: PTR answer, then SRV/TXT/A as additional records, with
    /// the instance name compressed to a pointer.
    fn chromecast_response() -> Vec<u8> {
        let mut p = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 3];
        // answer: PTR _googlecast._tcp.local → Chromecast-abc._googlecast._tcp.local
        let service_at = p.len();
        let mut svc = Vec::new();
        name(&mut svc, SERVICE);
        let mut ptr_data = Vec::new();
        ptr_data.push(14);
        ptr_data.extend_from_slice(b"Chromecast-abc");
        ptr_data.extend_from_slice(&[0xC0, service_at as u8]); // → "_googlecast._tcp.local"
        record(&mut p, &svc, TYPE_PTR, &ptr_data);
        let instance_at = p.len() - ptr_data.len();
        let inst_ptr = [0xC0, instance_at as u8];
        // SRV: port 8009, target abc.local
        let mut srv = vec![0, 0, 0, 0, 0x1F, 0x49];
        name(&mut srv, "abc.local");
        record(&mut p, &inst_ptr, TYPE_SRV, &srv);
        // TXT: id, fn, md
        let mut txt = Vec::new();
        for kv in ["id=8a3b5c7d9e0f", "fn=Living Room TV", "md=Chromecast"] {
            txt.push(kv.len() as u8);
            txt.extend_from_slice(kv.as_bytes());
        }
        record(&mut p, &inst_ptr, TYPE_TXT, &txt);
        let mut host = Vec::new();
        name(&mut host, "abc.local");
        record(&mut p, &host, TYPE_A, &[192, 168, 1, 40]);
        p
    }

    #[test]
    fn query_asks_for_the_cast_service() {
        let q = build_query();
        assert_eq!(&q[4..6], &[0, 1]);
        assert_eq!(read_name(&q, 12).unwrap().0, SERVICE);
    }

    #[test]
    fn parses_a_chromecast_answer() {
        let mut r = Records::default();
        r.absorb(&chromecast_response(), "192.168.1.99".parse().unwrap());
        let devs = r.devices();
        assert_eq!(devs.len(), 1, "{devs:?}");
        let d = &devs[0];
        assert_eq!(d.id, "8a3b5c7d9e0f");
        assert_eq!(d.name, "Living Room TV");
        assert_eq!(d.model, "Chromecast");
        assert_eq!(d.port, 8009);
        assert_eq!(d.ip, "192.168.1.40".parse::<IpAddr>().unwrap(), "the A record wins over the sender");
    }

    /// Asks the real network. `cargo test -- --ignored discover_live --nocapture` to see what a
    /// search finds from this machine.
    #[test]
    #[ignore]
    fn discover_live() {
        let found = discover(Duration::from_millis(2500)).expect("search");
        for d in &found {
            println!("{d:?}");
        }
    }

    #[test]
    fn garbage_is_ignored() {
        let mut r = Records::default();
        r.absorb(&[0xff; 40], "10.0.0.1".parse().unwrap());
        r.absorb(&[], "10.0.0.1".parse().unwrap());
        let mut looped = vec![0, 0, 0x84, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        looped.extend_from_slice(&[0xC0, 12]); // a name that points at itself
        r.absorb(&looped, "10.0.0.1".parse().unwrap());
        assert!(r.devices().is_empty());
    }
}
