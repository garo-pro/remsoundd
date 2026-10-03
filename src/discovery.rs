//! LAN and unicast discovery on UDP 47821: JSON announcements every 1.5 s, peers expire after 8 s.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::{watch, Mutex};
use tracing::{debug, info, warn};
use uuid::Uuid;

pub const ANNOUNCE_INTERVAL: Duration = Duration::from_millis(1500);
pub const PEER_EXPIRY: Duration = Duration::from_secs(8);
const HEARD_FROM_EXPIRY: Duration = Duration::from_secs(30);
const MAX_HEARD_FROM: usize = 64;
const MAX_NAME_CHARS: usize = 128;

/// The wire form. Field names and order are exact and case-sensitive.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct OutgoingAnnouncement<'a> {
    instance_id: String,
    name: &'a str,
    audio_port: u16,
    can_send: bool,
    can_receive: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct IncomingAnnouncement {
    instance_id: Option<String>,
    name: Option<String>,
    audio_port: Option<i64>,
    #[serde(default)]
    can_send: bool,
    #[serde(default)]
    can_receive: bool,
}

pub fn announcement_json(instance: Uuid, name: &str, audio_port: u16, can_send: bool, can_receive: bool) -> String {
    serde_json::to_string(&OutgoingAnnouncement {
        instance_id: instance.hyphenated().to_string(),
        name,
        audio_port,
        can_send,
        can_receive,
    })
    .expect("announcement serialises")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    pub instance: Uuid,
    pub name: String,
    pub audio_port: u16,
    pub can_send: bool,
    pub can_receive: bool,
}

/// Parse one datagram. Rejects our own id, the all-zero id, and ports outside 1-65535.
/// A blank name becomes the sender's address; long names are cut to 128 characters.
pub fn parse_announcement(payload: &[u8], from: IpAddr, own: Uuid) -> Option<Announcement> {
    let message: IncomingAnnouncement = serde_json::from_slice(payload).ok()?;
    let instance = Uuid::parse_str(message.instance_id.as_deref()?).ok()?;
    if instance == own || instance.is_nil() {
        return None;
    }
    let port = message.audio_port?;
    if !(1..=65535).contains(&port) {
        return None;
    }
    let name = match message.name.as_deref().map(str::trim) {
        Some(n) if !n.is_empty() => n.chars().take(MAX_NAME_CHARS).collect(),
        _ => from.to_string(),
    };
    Some(Announcement { instance, name, audio_port: port as u16, can_send: message.can_send, can_receive: message.can_receive })
}

#[derive(Debug, Clone)]
pub struct HeardPeer {
    pub announcement: Announcement,
    pub address: IpAddr,
    pub last_seen: Instant,
}

/// What discovery has heard lately, shared with the rest of the daemon (for peer names).
#[derive(Default)]
pub struct PeerTable {
    peers: HashMap<(Uuid, IpAddr), HeardPeer>,
}

impl PeerTable {
    /// Record a peer. Returns true when it is new to the table.
    pub fn record(&mut self, announcement: Announcement, address: IpAddr, now: Instant) -> bool {
        let key = (announcement.instance, address);
        let new = !self.peers.contains_key(&key);
        self.peers.insert(key, HeardPeer { announcement, address, last_seen: now });
        new
    }

    /// Drop peers silent for longer than [`PEER_EXPIRY`], returning them.
    pub fn expire(&mut self, now: Instant) -> Vec<HeardPeer> {
        let mut gone = Vec::new();
        self.peers.retain(|_, p| {
            let keep = now.duration_since(p.last_seen) <= PEER_EXPIRY;
            if !keep {
                gone.push(p.clone());
            }
            keep
        });
        gone
    }

    pub fn name_for(&self, address: IpAddr) -> Option<String> {
        self.peers.values().filter(|p| p.address == address).max_by_key(|p| p.last_seen).map(|p| p.announcement.name.clone())
    }

    pub fn instance_name(&self, instance: Uuid) -> Option<String> {
        self.peers.values().find(|p| p.announcement.instance == instance).map(|p| p.announcement.name.clone())
    }

    pub fn list(&self) -> Vec<HeardPeer> {
        let mut v: Vec<_> = self.peers.values().cloned().collect();
        v.sort_by(|a, b| a.announcement.name.cmp(&b.announcement.name).then(a.address.cmp(&b.address)));
        v
    }
}

pub type SharedPeerTable = Arc<Mutex<PeerTable>>;

/// Settings for the discovery service.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub instance: Uuid,
    pub name: String,
    pub audio_port: u16,
    pub discovery_port: u16,
    pub can_send: bool,
    pub can_receive: bool,
    /// Configured peers, announced to directly: broadcast does not cross Tailscale or other VPNs.
    pub unicast_targets: watch::Receiver<Vec<IpAddr>>,
    /// Announce at all (false for `remsoundd discover`, which only listens).
    pub announce: bool,
}

/// Bind the discovery socket with SO_REUSEADDR and SO_REUSEPORT, so a `remsoundd discover` run can
/// share the port with a running daemon for broadcasts.
pub fn bind_discovery_socket(port: u16) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]
    socket.set_reuse_port(true)?;
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    UdpSocket::from_std(socket.into())
}

/// Every IPv4 interface's subnet broadcast, plus the limited broadcast.
pub fn broadcast_addresses() -> Vec<Ipv4Addr> {
    let mut out = vec![Ipv4Addr::BROADCAST];
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for iface in interfaces {
            if iface.is_loopback() {
                continue;
            }
            if let if_addrs::IfAddr::V4(v4) = iface.addr {
                let bcast = v4.broadcast.unwrap_or_else(|| {
                    Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask))
                });
                if !out.contains(&bcast) {
                    out.push(bcast);
                }
            }
        }
    }
    out
}

/// Run discovery until the task is dropped. Peers heard are recorded in `table`.
pub async fn run(config: DiscoveryConfig, table: SharedPeerTable) -> anyhow::Result<()> {
    let socket = bind_discovery_socket(config.discovery_port)?;
    info!("discovery listening on UDP port {}", config.discovery_port);
    let message = announcement_json(config.instance, &config.name, config.audio_port, config.can_send, config.can_receive);
    let mut heard_from: HashMap<IpAddr, Instant> = HashMap::new();
    let mut ticker = tokio::time::interval(ANNOUNCE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut broadcast = broadcast_addresses();
    let mut last_broadcast_refresh = Instant::now();
    let mut buf = vec![0u8; 2048];
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = Instant::now();
                if config.announce {
                    // Interfaces change rarely; re-read them every 30 s rather than on every tick.
                    if now.duration_since(last_broadcast_refresh) > Duration::from_secs(30) {
                        broadcast = broadcast_addresses();
                        last_broadcast_refresh = now;
                    }
                    heard_from.retain(|_, at| now.duration_since(*at) <= HEARD_FROM_EXPIRY);
                    let mut targets: Vec<IpAddr> = broadcast.iter().map(|a| IpAddr::V4(*a)).collect();
                    for ip in config.unicast_targets.borrow().iter().chain(heard_from.keys()) {
                        if ip.is_ipv4() && !targets.contains(ip) {
                            targets.push(*ip);
                        }
                    }
                    for ip in targets {
                        // Best effort: discovery is a convenience, audio works without it.
                        if let Err(e) = socket.send_to(message.as_bytes(), (ip, config.discovery_port)).await {
                            debug!("discovery announcement to {ip} failed: {e}");
                        }
                    }
                }
                for gone in table.lock().await.expire(now) {
                    info!("discovery: {} at {} has gone quiet", gone.announcement.name, gone.address);
                }
            }
            received = socket.recv_from(&mut buf) => {
                let (len, from) = match received {
                    Ok(v) => v,
                    Err(e) => { warn!("discovery receive failed: {e}"); tokio::time::sleep(Duration::from_millis(500)).await; continue; }
                };
                let Some(announcement) = parse_announcement(&buf[..len], from.ip(), config.instance) else { continue };
                let now = Instant::now();
                if heard_from.len() >= MAX_HEARD_FROM && !heard_from.contains_key(&from.ip()) {
                    if let Some(oldest) = heard_from.iter().min_by_key(|(_, at)| **at).map(|(ip, _)| *ip) {
                        heard_from.remove(&oldest);
                    }
                }
                heard_from.insert(from.ip(), now);
                let name = announcement.name.clone();
                if table.lock().await.record(announcement, from.ip(), now) {
                    info!("discovery: heard {name} at {}", from.ip());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_announcement_matches_exactly() {
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        assert_eq!(
            announcement_json(id, "ED_DT", 47830, true, false),
            r#"{"InstanceId":"11111111-2222-3333-4444-555555555555","Name":"ED_DT","AudioPort":47830,"CanSend":true,"CanReceive":false}"#
        );
    }

    #[test]
    fn parses_another_ports_announcement() {
        let own = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let theirs = br#"{"InstanceId":"66666666-7777-8888-9999-000000000000","Name":"iPhone","AudioPort":47830,"CanSend":true,"CanReceive":true}"#;
        let a = parse_announcement(theirs, "192.168.1.8".parse().unwrap(), own).unwrap();
        assert_eq!((a.name.as_str(), a.audio_port), ("iPhone", 47830));
    }

    #[test]
    fn rejects_own_nil_bad_port_and_garbage() {
        let own = Uuid::new_v4();
        let from: IpAddr = "10.0.0.1".parse().unwrap();
        let own_msg = announcement_json(own, "me", 47830, true, true);
        assert!(parse_announcement(own_msg.as_bytes(), from, own).is_none());
        let nil = announcement_json(Uuid::nil(), "x", 47830, true, true);
        assert!(parse_announcement(nil.as_bytes(), from, own).is_none());
        let other = Uuid::new_v4();
        for port in ["0", "65536", "-1", "99999"] {
            let m = format!(r#"{{"InstanceId":"{other}","Name":"x","AudioPort":{port},"CanSend":true,"CanReceive":true}}"#);
            assert!(parse_announcement(m.as_bytes(), from, own).is_none(), "port {port}");
        }
        assert!(parse_announcement(b"not json", from, own).is_none());
        // Property names are case-sensitive, as .NET reads them.
        let lower = format!(r#"{{"instanceId":"{other}","name":"x","audioPort":47830}}"#);
        assert!(parse_announcement(lower.as_bytes(), from, own).is_none());
    }

    #[test]
    fn blank_name_becomes_address_and_long_names_are_cut() {
        let own = Uuid::new_v4();
        let other = Uuid::new_v4();
        let from: IpAddr = "10.0.0.9".parse().unwrap();
        let blank = format!(r#"{{"InstanceId":"{other}","Name":"   ","AudioPort":47830,"CanSend":true,"CanReceive":true}}"#);
        assert_eq!(parse_announcement(blank.as_bytes(), from, own).unwrap().name, "10.0.0.9");
        let long = announcement_json(other, &"n".repeat(300), 47830, true, true);
        assert_eq!(parse_announcement(long.as_bytes(), from, own).unwrap().name.chars().count(), 128);
    }

    #[test]
    fn table_expires_after_eight_seconds() {
        let mut t = PeerTable::default();
        let a = Announcement { instance: Uuid::new_v4(), name: "PC".into(), audio_port: 47830, can_send: true, can_receive: true };
        let start = Instant::now();
        assert!(t.record(a.clone(), "10.0.0.2".parse().unwrap(), start));
        assert!(!t.record(a, "10.0.0.2".parse().unwrap(), start));
        assert!(t.expire(start + Duration::from_secs(7)).is_empty());
        assert_eq!(t.expire(start + Duration::from_secs(9)).len(), 1);
        assert!(t.list().is_empty());
    }
}
