//! LAN service discovery over mDNS (`mdns-sd`). The server advertises
//! `_newfoundsync._udp.local.` with its audio/clock ports and codec in TXT
//! records; clients browse and present the list to pick from.
//!
//! Mirrors the role of `ensemble/internal/discovery` (and Soundsync's Bonjour
//! `_soundsync._tcp`), trimmed to a single-server app on a trusted LAN.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};

/// mDNS service type for Newfoundsync servers.
pub const SERVICE_TYPE: &str = "_newfoundsync._udp.local.";

/// The primary LAN IPv4 — the source address the OS picks to reach the outside,
/// i.e. the real default-route interface, NOT a VirtualBox/Hyper-V/WSL host-only
/// adapter or a `169.254.x` link-local. We "connect" a UDP socket (no packet is
/// actually sent) purely to make the kernel select the outbound interface.
pub fn primary_lan_ipv4() -> Option<Ipv4Addr> {
    // 8.8.8.8 is just a routing hint; works offline too as long as the LAN has a
    // default route (a normal home/office router). Fall back if there's none.
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

/// What a server publishes about itself.
#[derive(Clone, Debug)]
pub struct ServerAdvert {
    pub name: String,
    pub audio_port: u16,
    pub clock_port: u16,
    pub codec: String,
    pub bitrate: i32,
    pub version: String,
    /// TCP port serving the video stream, or 0 if this server shares audio only.
    pub video_port: u16,
}

/// A registered advertisement; unregisters and shuts the daemon down on drop.
pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    pub fn fullname(&self) -> &str {
        &self.fullname
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

fn host_name() -> String {
    let base = std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "newfoundsync".to_string());
    // mDNS host names must end in ".local."
    format!("{}.local.", base.replace(' ', "-"))
}

/// Register the server on the LAN. Addresses are auto-detected.
pub fn advertise(advert: &ServerAdvert) -> Result<Advertiser> {
    let daemon = ServiceDaemon::new().context("create mDNS daemon")?;

    let mut props: HashMap<String, String> = HashMap::new();
    props.insert("name".into(), advert.name.clone());
    props.insert("clock_port".into(), advert.clock_port.to_string());
    props.insert("codec".into(), advert.codec.clone());
    props.insert("bitrate".into(), advert.bitrate.to_string());
    props.insert("video_port".into(), advert.video_port.to_string());
    props.insert("ver".into(), advert.version.clone());

    // Advertise the real LAN IP only. Auto-detecting all interfaces would also
    // publish host-only/virtual adapters (VirtualBox 192.168.56.x, Hyper-V, WSL)
    // and link-locals — a client on another machine that picked one of those
    // could never reach us. Fall back to auto-detect if we can't determine it.
    let lan_ip = primary_lan_ipv4();
    let addrs = lan_ip.map(|ip| ip.to_string()).unwrap_or_default();
    let mut info = ServiceInfo::new(
        SERVICE_TYPE,
        &advert.name,
        &host_name(),
        addrs.as_str(),
        advert.audio_port,
        props,
    )
    .context("build ServiceInfo")?;
    if lan_ip.is_none() {
        info = info.enable_addr_auto();
        tracing::warn!("no LAN IPv4 detected; advertising all interfaces");
    }

    let fullname = info.get_fullname().to_string();
    daemon.register(info).context("register mDNS service")?;
    tracing::info!(
        service = %fullname,
        audio_port = advert.audio_port,
        lan_ip = ?lan_ip,
        "advertising over mDNS"
    );

    Ok(Advertiser { daemon, fullname })
}

/// A server discovered on the LAN.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredServer {
    pub fullname: String,
    pub name: String,
    pub ip: IpAddr,
    pub audio_port: u16,
    pub clock_port: u16,
    pub codec: String,
    pub bitrate: i32,
    pub version: String,
    /// Video TCP port, or 0 if the server shares audio only.
    pub video_port: u16,
}

/// A running browse session; stops on drop.
pub struct Browser {
    daemon: ServiceDaemon,
    servers: Arc<Mutex<HashMap<String, DiscoveredServer>>>,
    _thread: Option<JoinHandle<()>>,
}

impl Browser {
    /// Currently known servers, sorted by name.
    pub fn servers(&self) -> Vec<DiscoveredServer> {
        let mut v: Vec<_> = self.servers.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
        if let Some(h) = self._thread.take() {
            let _ = h.join();
        }
    }
}

/// Start browsing for Newfoundsync servers on the LAN.
pub fn browse() -> Result<Browser> {
    let daemon = ServiceDaemon::new().context("create mDNS daemon")?;
    let receiver = daemon.browse(SERVICE_TYPE).context("start mDNS browse")?;
    let servers: Arc<Mutex<HashMap<String, DiscoveredServer>>> = Arc::new(Mutex::new(HashMap::new()));

    let servers_thread = servers.clone();
    let handle = thread::Builder::new()
        .name("mdns-browse".into())
        .spawn(move || {
            while let Ok(event) = receiver.recv() {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        if let Some(s) = to_server(&info) {
                            servers_thread
                                .lock()
                                .unwrap()
                                .insert(s.fullname.clone(), s);
                        }
                    }
                    ServiceEvent::ServiceRemoved(_ty, fullname) => {
                        servers_thread.lock().unwrap().remove(&fullname);
                    }
                    _ => {}
                }
            }
        })
        .context("spawn mDNS browse thread")?;

    Ok(Browser {
        daemon,
        servers,
        _thread: Some(handle),
    })
}

/// Two IPv4s share a /24 (cheap "same LAN subnet" heuristic).
fn same_subnet24(a: Ipv4Addr, b: Ipv4Addr) -> bool {
    let (a, b) = (a.octets(), b.octets());
    a[0] == b[0] && a[1] == b[1] && a[2] == b[2]
}

/// Choose the best address to reach a server from the ones it advertises:
/// drop loopback/link-local, prefer an IPv4 on our own subnet, then any IPv4.
fn pick_addr(addrs: impl Iterator<Item = IpAddr>) -> Option<IpAddr> {
    pick_addr_with(addrs, primary_lan_ipv4())
}

/// Testable core of [`pick_addr`]; `mine` is our own LAN IPv4 (for subnet match).
fn pick_addr_with(addrs: impl Iterator<Item = IpAddr>, mine: Option<Ipv4Addr>) -> Option<IpAddr> {
    let cands: Vec<IpAddr> = addrs
        .filter(|a| !a.is_loopback())
        .filter(|a| !matches!(a, IpAddr::V4(v) if v.is_link_local()))
        .collect();
    if let Some(mine) = mine {
        if let Some(a) = cands
            .iter()
            .find(|a| matches!(a, IpAddr::V4(v) if same_subnet24(*v, mine)))
        {
            return Some(*a);
        }
    }
    cands
        .iter()
        .find(|a| a.is_ipv4())
        .copied()
        .or_else(|| cands.into_iter().next())
}

fn to_server(info: &ResolvedService) -> Option<DiscoveredServer> {
    let ip = pick_addr(info.get_addresses().iter().map(|s| s.to_ip_addr()))?;
    let prop = |k: &str| info.get_property_val_str(k);
    Some(DiscoveredServer {
        fullname: info.get_fullname().to_string(),
        name: prop("name")
            .map(str::to_string)
            .unwrap_or_else(|| info.get_fullname().to_string()),
        ip,
        audio_port: info.get_port(),
        clock_port: prop("clock_port").and_then(|s| s.parse().ok()).unwrap_or(0),
        codec: prop("codec").unwrap_or("pcm").to_string(),
        bitrate: prop("bitrate").and_then(|s| s.parse().ok()).unwrap_or(0),
        version: prop("ver").unwrap_or("").to_string(),
        video_port: prop("video_port").and_then(|s| s.parse().ok()).unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn pick_addr_prefers_same_subnet_over_virtual() {
        let mine: Ipv4Addr = "192.168.50.20".parse().unwrap();
        // Server advertises a VirtualBox host-only IP first, then the real LAN IP.
        let addrs = vec![ip("192.168.56.1"), ip("192.168.50.96")];
        assert_eq!(
            pick_addr_with(addrs.into_iter(), Some(mine)),
            Some(ip("192.168.50.96")),
        );
    }

    #[test]
    fn pick_addr_skips_loopback_and_link_local() {
        let addrs = vec![ip("127.0.0.1"), ip("169.254.0.5"), ip("10.1.2.3")];
        assert_eq!(pick_addr_with(addrs.into_iter(), None), Some(ip("10.1.2.3")));
    }

    #[test]
    fn pick_addr_none_when_only_unreachable() {
        let addrs = vec![ip("127.0.0.1"), ip("169.254.9.9")];
        assert_eq!(pick_addr_with(addrs.into_iter(), None), None);
    }

    #[test]
    fn primary_lan_ipv4_is_sane_when_present() {
        if let Some(ip) = primary_lan_ipv4() {
            assert!(!ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified());
        }
    }
}
