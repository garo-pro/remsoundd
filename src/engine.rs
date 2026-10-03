//! The RemSound transport: one UDP socket, peers, heartbeats, tick proofs, received sessions and
//! the outbound Opus stream. It knows nothing about the bridge socket: callers drive it with
//! [`Command`]s and read [`Event`]s, so a future PipeWire front end can use it unchanged.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rand::Rng;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::audio::Cue;
use crate::config::{PeerSpec, SendMode};
use crate::crypto::{fingerprints_equal, Cipher, Credentials, NonceSequence};
use crate::discovery::SharedPeerTable;
use crate::player::Player;
use crate::protocol::{self, AudioFormat, HeartbeatKind, PacketType, CONTROL_STREAM_ID};
use crate::session::ReceiveSession;
use crate::tickproof;

pub const PING_INTERVAL: Duration = Duration::from_secs(1);
pub const HEALTHY_WINDOW: Duration = Duration::from_secs(2);
pub const UNREACHABLE_WINDOW: Duration = Duration::from_secs(5);
/// Windows prunes a session with no audio for this long (AudioReceiver.SessionIdleTimeout); so do we.
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(4);
/// A peer we have heard audio from this recently stays a send target even if its pongs stop.
pub const RECEIVING_KEEPS_ARMED: Duration = Duration::from_secs(3);
pub const FORMAT_RESEND_INTERVAL: Duration = Duration::from_millis(250);
/// 20 ms at 48 kHz: what we encode and announce.
pub const SEND_FRAME_SAMPLES: usize = 960;
const SEND_FRAME: Duration = Duration::from_millis(20);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const RESOLVE_INTERVAL: Duration = Duration::from_secs(10);
const RESOLVE_REFRESH: Duration = Duration::from_secs(60);
const EVENT_QUEUE: usize = 4096;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub name: String,
    pub instance: Uuid,
    pub credentials: Credentials,
    pub peers: Vec<PeerSpec>,
    pub mic_from: Option<String>,
    pub bind: SocketAddr,
    pub send_mode: SendMode,
    pub bitrate: i32,
    pub jitter: Duration,
    /// Names heard by discovery, used in log lines and events.
    pub names: Option<SharedPeerTable>,
    /// Resolved peer addresses, published for discovery's unicast announcements.
    pub unicast: Option<watch::Sender<Vec<IpAddr>>>,
}

#[derive(Debug)]
pub enum Command {
    TtsBegin { id: String, sample_rate: u32, channels: usize },
    TtsPcm(Vec<i16>),
    TtsEnd { id: String },
    TtsAbort { id: String },
    /// Drop every queued utterance, reporting each done (the bridge client went away).
    AbortAll,
    Cue(Cue),
    /// Raw 48 kHz interleaved stereo for the outbound stream (loopback test).
    Stream(Vec<f32>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    PeerConnected { name: String, addr: IpAddr, rtt_ms: Option<u32> },
    PeerLost { name: String, addr: IpAddr },
    /// Decoded microphone audio from the selected peer, 48 kHz interleaved stereo.
    Mic { from: IpAddr, samples: Vec<f32> },
    PlaybackDone { id: String },
    /// A command could not be carried out; the message is for the bridge client.
    CommandRejected { message: String },
}

pub struct EngineHandle {
    pub commands: mpsc::UnboundedSender<Command>,
    pub events: mpsc::Receiver<Event>,
    pub local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl EngineHandle {
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// Bind the audio socket and start the engine.
pub async fn start(config: EngineConfig) -> anyhow::Result<EngineHandle> {
    let socket = bind_audio_socket(config.bind).with_context(|| {
        format!("cannot open UDP port {}; is another RemSound or remsoundd already running?", config.bind.port())
    })?;
    let local_addr = socket.local_addr()?;
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::channel(EVENT_QUEUE);
    let (stop_tx, stop_rx) = oneshot::channel();
    let engine = Engine::new(config, Arc::new(socket), event_tx)?;
    let task = tokio::spawn(engine.run(cmd_rx, stop_rx));
    Ok(EngineHandle { commands: cmd_tx, events: event_rx, local_addr, shutdown: Some(stop_tx), task: Some(task) })
}

fn bind_audio_socket(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Room for a burst at 400 packets a second without the kernel dropping any.
    let _ = socket.set_recv_buffer_size(1 << 20);
    let _ = socket.set_send_buffer_size(1 << 20);
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    UdpSocket::from_std(socket.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Security {
    Matches,
    Mismatch,
    NeedsUpdate,
}

struct Peer {
    spec: PeerSpec,
    addr: Option<SocketAddr>,
    resolved_at: Option<Instant>,
    resolving: bool,
    first_ping: Option<Instant>,
    last_pong: Option<Instant>,
    rtt_ms: Option<u32>,
    connected: bool,
    said_unreachable: bool,
    last_audio: Option<Instant>,
}

impl Peer {
    fn ip(&self) -> Option<IpAddr> {
        self.addr.map(|a| a.ip())
    }

    fn answering(&self, now: Instant) -> bool {
        self.last_pong.is_some_and(|t| now.duration_since(t) <= UNREACHABLE_WINDOW)
    }
}

/// Logs a message at most once per key per interval.
#[derive(Default)]
struct Throttle(HashMap<String, Instant>);

impl Throttle {
    fn allow(&mut self, key: impl Into<String>, every: Duration, now: Instant) -> bool {
        let key = key.into();
        match self.0.get(&key) {
            Some(at) if now.duration_since(*at) < every => false,
            _ => {
                if self.0.len() > 1024 {
                    self.0.clear();
                }
                self.0.insert(key, now);
                true
            }
        }
    }
}

#[derive(Default)]
struct Counters {
    malformed: u64,
    not_allowed: u64,
    audio_without_session: u64,
    ignored_types: u64,
    mic_dropped: u64,
}

struct Engine {
    config: EngineConfig,
    socket: Arc<UdpSocket>,
    cipher: Cipher,
    nonces: NonceSequence,
    peers: Vec<Peer>,
    sessions: HashMap<(SocketAddr, u16), ReceiveSession>,
    security: HashMap<IpAddr, Security>,
    clock: Instant,
    recent_pings: [i64; 16],
    ping_slot: usize,
    control_seq: u32,
    stream_id: u16,
    audio_seq: u32,
    format_seq: u32,
    last_format: Option<Instant>,
    sending: bool,
    encoder: opus::Encoder,
    player: Player,
    guard: tickproof::Guard,
    proof_seen: HashMap<IpAddr, Instant>,
    events: mpsc::Sender<Event>,
    throttle: Throttle,
    counters: Counters,
    resolved_tx: mpsc::UnboundedSender<(usize, Option<SocketAddr>)>,
    resolved_rx: Option<mpsc::UnboundedReceiver<(usize, Option<SocketAddr>)>>,
}

fn new_stream_id() -> u16 {
    rand::thread_rng().gen_range(1..u16::MAX)
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Engine {
    fn new(config: EngineConfig, socket: Arc<UdpSocket>, events: mpsc::Sender<Event>) -> anyhow::Result<Self> {
        let mut encoder = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)?;
        encoder.set_bitrate(opus::Bitrate::Bits(config.bitrate))?;
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(10)?;
        let peers = config
            .peers
            .iter()
            .map(|spec| Peer {
                addr: spec.host.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, spec.port)),
                spec: spec.clone(),
                resolved_at: None,
                resolving: false,
                first_ping: None,
                last_pong: None,
                rtt_ms: None,
                connected: false,
                said_unreachable: false,
                last_audio: None,
            })
            .collect();
        let (resolved_tx, resolved_rx) = mpsc::unbounded_channel();
        Ok(Self {
            cipher: Cipher::new(&config.credentials.key),
            nonces: NonceSequence::new(),
            peers,
            sessions: HashMap::new(),
            security: HashMap::new(),
            clock: Instant::now(),
            recent_pings: [i64::MIN; 16],
            ping_slot: 0,
            control_seq: 0,
            stream_id: new_stream_id(),
            audio_seq: 0,
            format_seq: 0,
            last_format: None,
            sending: false,
            encoder,
            player: Player::new(2.0),
            guard: tickproof::Guard::default(),
            proof_seen: HashMap::new(),
            events,
            throttle: Throttle::default(),
            counters: Counters::default(),
            resolved_tx,
            resolved_rx: Some(resolved_rx),
            config,
            socket,
        })
    }

    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>, mut stop: oneshot::Receiver<()>) {
        let mut resolved_rx = self.resolved_rx.take().unwrap();
        let mut ping = tokio::time::interval(PING_INTERVAL);
        let mut proofs = tokio::time::interval(tickproof::SEND_INTERVAL);
        let mut send = tokio::time::interval(SEND_FRAME);
        // A late tick is caught up at once: the stream must average exactly real time.
        send.set_missed_tick_behavior(MissedTickBehavior::Burst);
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut resolve = tokio::time::interval(RESOLVE_INTERVAL);
        let mut buf = vec![0u8; 65_536];
        let socket = self.socket.clone();
        let mut commands_open = true;
        self.publish_unicast();
        info!(
            "RemSound transport on UDP port {}, stream {}, sending Opus at {} kbps, {} mode",
            self.socket.local_addr().map(|a| a.port()).unwrap_or(0),
            self.stream_id,
            self.config.bitrate / 1000,
            match self.config.send_mode {
                SendMode::Continuous => "continuous",
                SendMode::Burst => "burst",
            }
        );
        loop {
            tokio::select! {
                _ = &mut stop => break,
                received = socket.recv_from(&mut buf) => match received {
                    Ok((len, from)) => self.handle_packet(&buf[..len], from).await,
                    // On Linux an ICMP "port unreachable" from an earlier send surfaces here; it is not fatal.
                    Err(e) => debug!("UDP receive error: {e}"),
                },
                _ = ping.tick() => self.on_ping_tick().await,
                _ = proofs.tick() => self.send_tick_proofs().await,
                _ = send.tick() => self.on_send_tick().await,
                _ = poll.tick() => self.on_poll_tick(),
                _ = resolve.tick() => self.start_resolving(),
                Some((index, addr)) = resolved_rx.recv() => self.on_resolved(index, addr),
                // With every command sender gone the link stays up until told to stop.
                command = commands.recv(), if commands_open => match command {
                    Some(c) => self.on_command(c),
                    None => commands_open = false,
                },
            }
        }
        for id in self.player.abort_all() {
            self.emit(Event::PlaybackDone { id });
        }
        info!("RemSound transport stopped");
    }

    fn emit(&mut self, event: Event) {
        let is_mic = matches!(event, Event::Mic { .. });
        if let Err(e) = self.events.try_send(event) {
            if is_mic {
                self.counters.mic_dropped += 1;
                if self.throttle.allow("mic-drop", Duration::from_secs(30), Instant::now()) {
                    warn!("microphone audio is being dropped: nothing is reading it fast enough ({} chunks so far)", self.counters.mic_dropped);
                }
            } else if matches!(e, mpsc::error::TrySendError::Full(_)) {
                warn!("event queue full; an event was lost");
            }
        }
    }

    fn peer_name(&self, ip: IpAddr) -> String {
        if let Some(table) = &self.config.names {
            if let Ok(t) = table.try_lock() {
                if let Some(name) = t.name_for(ip) {
                    return name;
                }
            }
        }
        self.peers.iter().find(|p| p.ip() == Some(ip)).map(|p| p.spec.label.clone()).unwrap_or_else(|| ip.to_string())
    }

    /// "PC-DESKTOP at 100.64.0.5", or just the address when that is all we know.
    fn who(&self, ip: IpAddr) -> String {
        let name = self.peer_name(ip);
        if name == ip.to_string() {
            name
        } else {
            format!("{name} at {ip}")
        }
    }

    fn peer_index(&self, ip: IpAddr) -> Option<usize> {
        self.peers.iter().position(|p| p.ip() == Some(ip))
    }

    fn allowed(&self, ip: IpAddr) -> bool {
        self.peer_index(ip).is_some()
    }


    // ---- receiving ------------------------------------------------------------------------------

    async fn handle_packet(&mut self, packet: &[u8], from: SocketAddr) {
        let Some((header, payload)) = protocol::read_header(packet) else {
            self.counters.malformed += 1;
            return;
        };
        let now = Instant::now();
        match header.packet_type() {
            Some(PacketType::Format) => self.on_format(from, header.stream_id, payload, now),
            Some(PacketType::Audio) => self.on_audio(from, header.stream_id, header.sequence, payload, now),
            Some(PacketType::Heartbeat) => self.on_heartbeat(from, payload, now).await,
            Some(PacketType::TickProof) => self.on_tick_proof(from, payload, now),
            // KeepAlive is a pre-2026-05 leftover; Control (remote volume), AddrCheck (relay) and
            // Metronome are outside this daemon's scope.
            _ => self.counters.ignored_types += 1,
        }
    }

    fn on_format(&mut self, from: SocketAddr, stream_id: u16, payload: &[u8], now: Instant) {
        let Some((format, fingerprint)) = protocol::read_format_payload(payload) else {
            self.counters.malformed += 1;
            return;
        };
        if let Err(why) = format.check_usable() {
            if self.throttle.allow(format!("badformat-{}", from.ip()), Duration::from_secs(5), now) {
                warn!("{} announced a stream this daemon cannot play: {why}", self.who(from.ip()));
            }
            return;
        }
        if !self.allowed(from.ip()) {
            self.counters.not_allowed += 1;
            if self.throttle.allow(format!("notallowed-{}", from.ip()), Duration::from_secs(60), now) {
                info!(
                    "{} is sending audio to this daemon but is not in its peers list, so it is ignored. Add {} to peers in the config to accept it",
                    self.who(from.ip()),
                    from.ip()
                );
            }
            return;
        }
        self.note_security(from.ip(), fingerprint.as_ref().map(|f| &f[..]));

        let key = (from, stream_id);
        if let Some(existing) = self.sessions.get(&key) {
            if same_stream_format(&existing.format, &format) {
                return;
            }
        }
        match ReceiveSession::new(format.clone(), self.config.jitter, now) {
            Ok(session) => {
                let changed = self.sessions.insert(key, session).is_some();
                // A sender picks a new stream id on restart or codec change: retire its older streams on the same lane.
                let superseded: Vec<_> = self
                    .sessions
                    .iter()
                    .filter(|((ep, id), s)| *ep == from && *id != stream_id && s.format.lane == format.lane)
                    .map(|(k, _)| *k)
                    .collect();
                for k in superseded {
                    self.sessions.remove(&k);
                    debug!("stream {} from {} replaced by stream {stream_id}", k.1, from);
                }
                let verb = if changed { "changed its stream to" } else { "is sending" };
                info!("{} {verb} {}", self.who(from.ip()), format.describe());
            }
            Err(e) => warn!("could not open a stream from {}: {e}", self.who(from.ip())),
        }
    }

    fn note_security(&mut self, ip: IpAddr, fingerprint: Option<&[u8]>) {
        let status = match fingerprint {
            None => Security::NeedsUpdate,
            Some(f) if fingerprints_equal(f, &self.config.credentials.fingerprint) => Security::Matches,
            Some(_) => Security::Mismatch,
        };
        if self.security.insert(ip, status) == Some(status) {
            return;
        }
        let who = self.who(ip);
        match status {
            Security::Matches => info!("{who}: password matches"),
            Security::Mismatch => warn!(
                "password mismatch: {who} uses a different RemSound password, so its audio cannot be decrypted. Make the two passwords the same"
            ),
            Security::NeedsUpdate => warn!(
                "peer needs update: {who} sent no password fingerprint, so it runs a RemSound too old to encrypt. Update RemSound on it"
            ),
        }
    }

    fn on_audio(&mut self, from: SocketAddr, stream_id: u16, sequence: u32, payload: &[u8], now: Instant) {
        if !self.allowed(from.ip()) {
            self.counters.not_allowed += 1;
            return;
        }
        let Some(session) = self.sessions.get_mut(&(from, stream_id)) else {
            // Normal for up to 250 ms after we start: audio arrives before the next Format packet.
            self.counters.audio_without_session += 1;
            return;
        };
        let failures_before = session.stats.decrypt_failures;
        session.push_audio(sequence, payload, &self.cipher, now);
        let failed = session.stats.decrypt_failures > failures_before;
        let decrypted = session.last_audio == now;
        if decrypted {
            if let Some(i) = self.peer_index(from.ip()) {
                self.peers[i].last_audio = Some(now);
            }
        }
        if failed && self.throttle.allow(format!("decrypt-{}", from.ip()), Duration::from_secs(30), now) {
            warn!("audio from {} does not decrypt: the passwords differ", self.who(from.ip()));
        }
    }

    async fn on_heartbeat(&mut self, from: SocketAddr, payload: &[u8], now: Instant) {
        let Some((kind, stamp)) = protocol::read_heartbeat_payload(payload) else {
            self.counters.malformed += 1;
            return;
        };
        match kind {
            HeartbeatKind::Ping => {
                // Every ping is answered, to wherever it came from, with its stamp echoed verbatim.
                self.control_seq = self.control_seq.wrapping_add(1);
                let pong = protocol::packet(
                    PacketType::Heartbeat,
                    CONTROL_STREAM_ID,
                    self.control_seq,
                    &protocol::write_heartbeat_payload(HeartbeatKind::Pong, stamp),
                );
                send_to(&self.socket, &pong, from).await;
            }
            HeartbeatKind::Pong => {
                // Only a pong answering one of our own recent pings counts.
                if !self.recent_pings.contains(&stamp) {
                    return;
                }
                let rtt = (self.clock.elapsed().as_millis() as i64 - stamp).max(0) as u32;
                let Some(i) = self.peer_index(from.ip()) else { return };
                let peer = &mut self.peers[i];
                peer.rtt_ms = Some(match peer.rtt_ms {
                    Some(old) => (old as f64 * 0.7 + rtt as f64 * 0.3) as u32,
                    None => rtt,
                });
                peer.last_pong = Some(now);
                self.update_health(i, now);
            }
        }
    }

    fn on_tick_proof(&mut self, from: SocketAddr, payload: &[u8], now: Instant) {
        if payload.len() != tickproof::SEALED_PAYLOAD_BYTES {
            self.counters.malformed += 1;
            return;
        }
        // One every two seconds per address is plenty: a peer sends one every five.
        if self.proof_seen.get(&from.ip()).is_some_and(|t| now.duration_since(*t) < Duration::from_secs(2)) {
            return;
        }
        if self.proof_seen.len() > 1024 {
            self.proof_seen.clear();
        }
        self.proof_seen.insert(from.ip(), now);
        let who = self.who(from.ip());
        match self.guard.accept(&self.cipher, payload, unix_now()) {
            Ok(proof) => {
                if self.throttle.allow(format!("proof-ok-{}", from.ip()), Duration::from_secs(3600), now) {
                    let device = self
                        .config
                        .names
                        .as_ref()
                        .and_then(|t| t.try_lock().ok().and_then(|t| t.instance_name(proof.instance)))
                        .map(|n| format!(" (device {n})"))
                        .unwrap_or_default();
                    if self.allowed(from.ip()) {
                        info!("{who}{device} has ticked this daemon and proved it has our password");
                    } else {
                        info!(
                            "{who}{device} has ticked this daemon and proved it has our password, but is not in the peers list. Add {} to peers in the config to connect",
                            from.ip()
                        );
                    }
                }
            }
            Err(rejection) => {
                if self.throttle.allow(format!("proof-bad-{}-{rejection}", from.ip()), Duration::from_secs(600), now) {
                    match rejection {
                        tickproof::Rejection::NotOurPassword => warn!("password mismatch: {who} ticked this daemon, but with a different password"),
                        other => warn!("{who} sent a tick proof that was refused: {other}"),
                    }
                }
            }
        }
    }

    // ---- peer health ------------------------------------------------------------------------------

    fn update_health(&mut self, i: usize, now: Instant) {
        let peer = &self.peers[i];
        let Some(ip) = peer.ip() else { return };
        let answering = peer.answering(now);
        if answering && !peer.connected {
            let rtt = peer.rtt_ms;
            self.peers[i].connected = true;
            self.peers[i].said_unreachable = false;
            let name = self.peer_name(ip);
            let password = match self.security.get(&ip) {
                Some(Security::Matches) => ", password matches",
                Some(Security::Mismatch) => ", but its password is different",
                _ => "",
            };
            info!("{name} connected over {ip}, round trip {} ms{password}", rtt.unwrap_or(0));
            self.emit(Event::PeerConnected { name, addr: ip, rtt_ms: rtt });
        } else if !answering && peer.connected {
            self.peers[i].connected = false;
            let name = self.peer_name(ip);
            warn!("{name} at {ip} is unreachable: no answer to pings for 5 seconds");
            self.emit(Event::PeerLost { name, addr: ip });
        } else if !answering && !peer.said_unreachable && peer.first_ping.is_some_and(|t| now.duration_since(t) > UNREACHABLE_WINDOW) {
            self.peers[i].said_unreachable = true;
            warn!(
                "{} has not answered pings yet. Check that RemSound is running there, that it has this machine ticked, and that UDP port {} is open",
                self.who(ip),
                self.peers[i].spec.port
            );
        }
    }

    fn send_targets(&self, now: Instant) -> Vec<SocketAddr> {
        self.peers
            .iter()
            .filter(|p| p.connected || p.last_audio.is_some_and(|t| now.duration_since(t) <= RECEIVING_KEEPS_ARMED))
            .filter_map(|p| p.addr)
            .collect()
    }

    async fn on_ping_tick(&mut self) {
        let now = Instant::now();
        let stamp = self.clock.elapsed().as_millis() as i64;
        self.recent_pings[self.ping_slot] = stamp;
        self.ping_slot = (self.ping_slot + 1) % self.recent_pings.len();
        self.control_seq = self.control_seq.wrapping_add(1);
        let ping = protocol::packet(PacketType::Heartbeat, CONTROL_STREAM_ID, self.control_seq, &protocol::write_heartbeat_payload(HeartbeatKind::Ping, stamp));
        for i in 0..self.peers.len() {
            if let Some(addr) = self.peers[i].addr {
                self.peers[i].first_ping.get_or_insert(now);
                send_to(&self.socket, &ping, addr).await;
            }
            self.update_health(i, now);
        }
        // Sessions with no audio for 4 s are gone, as on Windows.
        let idle: Vec<_> = self.sessions.iter().filter(|(_, s)| s.idle_for(now) > SESSION_IDLE_TIMEOUT).map(|(k, _)| *k).collect();
        for key in idle {
            if let Some(s) = self.sessions.remove(&key) {
                let st = &s.stats;
                info!(
                    "stopped receiving from {}: no audio for 4 seconds ({} packets, {} recovered by FEC, {} frames concealed)",
                    self.who(key.0.ip()),
                    st.packets,
                    st.fec_recovered,
                    st.concealed_frames
                );
            }
        }
    }

    async fn send_tick_proofs(&mut self) {
        let sealed = tickproof::seal(&self.cipher, self.config.instance, unix_now());
        self.control_seq = self.control_seq.wrapping_add(1);
        let packet = protocol::packet(PacketType::TickProof, CONTROL_STREAM_ID, self.control_seq, &sealed);
        let targets: Vec<_> = self.peers.iter().filter_map(|p| p.addr).collect();
        for addr in targets {
            send_to(&self.socket, &packet, addr).await;
        }
    }

    // ---- sending ----------------------------------------------------------------------------------

    async fn send_format(&mut self, targets: &[SocketAddr], now: Instant) {
        self.last_format = Some(now);
        self.format_seq = self.format_seq.wrapping_add(1);
        let payload = protocol::write_format_payload(&AudioFormat::opus_48k_stereo(SEND_FRAME_SAMPLES as i32), Some(&self.config.credentials.fingerprint));
        let packet = protocol::packet(PacketType::Format, self.stream_id, self.format_seq, &payload);
        for &t in targets {
            send_to(&self.socket, &packet, t).await;
        }
    }

    async fn on_send_tick(&mut self) {
        let now = Instant::now();
        let targets = self.send_targets(now);
        let connected = !targets.is_empty();
        let has_content = self.player.has_content();
        let send_audio = connected && (self.config.send_mode == SendMode::Continuous || has_content);

        // Format goes out every 250 ms while anyone is listening, and at once when a stream (re)starts,
        // so the far end opens its session before the first audio packet.
        let format_due = self.last_format.is_none_or(|t| now.duration_since(t) >= FORMAT_RESEND_INTERVAL);
        if connected && (format_due || (send_audio && !self.sending)) {
            self.send_format(&targets, now).await;
        }

        if !has_content && !send_audio {
            self.sending = false;
            return;
        }
        let (audio, done) = self.player.pull(SEND_FRAME_SAMPLES, connected);
        if send_audio {
            let mut encoded = [0u8; 4000];
            match self.encoder.encode_float(&audio, &mut encoded) {
                Ok(n) => {
                    let sealed = self.cipher.seal_next(&mut self.nonces, &encoded[..n]);
                    self.audio_seq = self.audio_seq.wrapping_add(1);
                    let packet = protocol::packet(PacketType::Audio, self.stream_id, self.audio_seq, &sealed);
                    for &t in &targets {
                        send_to(&self.socket, &packet, t).await;
                    }
                }
                Err(e) => warn!("Opus encoding failed: {e}"),
            }
        }
        self.sending = send_audio;
        for id in done {
            self.emit(Event::PlaybackDone { id });
        }
    }

    fn on_command(&mut self, command: Command) {
        let result = match command {
            Command::TtsBegin { id, sample_rate, channels } => self.player.begin(&id, sample_rate, channels),
            Command::TtsPcm(samples) => self.player.pcm(&samples),
            Command::TtsEnd { id } => self.player.end(&id),
            Command::TtsAbort { id } => {
                if let Some(id) = self.player.abort(&id) {
                    self.emit(Event::PlaybackDone { id });
                }
                Ok(())
            }
            Command::AbortAll => {
                for id in self.player.abort_all() {
                    self.emit(Event::PlaybackDone { id });
                }
                Ok(())
            }
            Command::Cue(cue) => {
                self.player.cue(cue);
                Ok(())
            }
            Command::Stream(samples) => {
                self.player.push_stream(&samples);
                Ok(())
            }
        };
        if let Err(e) = result {
            self.emit(Event::CommandRejected { message: e.to_string() });
        }
    }

    // ---- microphone -------------------------------------------------------------------------------

    /// The peer whose microphone goes to the bridge: `mic_from` if set, else the first peer in
    /// config order that is sending audio.
    fn mic_source(&self, now: Instant) -> Option<IpAddr> {
        if let Some(wanted) = self.config.mic_from.as_deref() {
            let wanted = wanted.trim();
            return self
                .peers
                .iter()
                .find(|p| p.spec.label == wanted || p.spec.host == wanted || p.ip().map(|ip| ip.to_string()).as_deref() == Some(wanted))
                .and_then(Peer::ip);
        }
        self.peers
            .iter()
            .filter_map(Peer::ip)
            .find(|ip| self.sessions.iter().any(|((ep, _), s)| ep.ip() == *ip && s.idle_for(now) < Duration::from_secs(1)))
    }

    fn on_poll_tick(&mut self) {
        let now = Instant::now();
        let source = self.mic_source(now);
        let mut mic = Vec::new();
        for ((ep, _), session) in self.sessions.iter_mut() {
            let samples = session.poll(now);
            if !samples.is_empty() && Some(ep.ip()) == source {
                mic.push((ep.ip(), samples));
            }
        }
        for (from, samples) in mic {
            self.emit(Event::Mic { from, samples });
        }
    }

    // ---- name resolution --------------------------------------------------------------------------

    fn start_resolving(&mut self) {
        let now = Instant::now();
        for (i, peer) in self.peers.iter_mut().enumerate() {
            if peer.spec.host.parse::<IpAddr>().is_ok() || peer.resolving {
                continue;
            }
            if peer.addr.is_some() && peer.resolved_at.is_some_and(|t| now.duration_since(t) < RESOLVE_REFRESH) {
                continue;
            }
            peer.resolving = true;
            let host = peer.spec.host.clone();
            let port = peer.spec.port;
            let tx = self.resolved_tx.clone();
            tokio::spawn(async move {
                let found = match tokio::net::lookup_host((host.as_str(), port)).await {
                    Ok(addrs) => addrs.into_iter().find(SocketAddr::is_ipv4),
                    Err(_) => None,
                };
                let _ = tx.send((i, found));
            });
        }
    }

    fn on_resolved(&mut self, index: usize, addr: Option<SocketAddr>) {
        let now = Instant::now();
        let Some(peer) = self.peers.get_mut(index) else { return };
        peer.resolving = false;
        match addr {
            Some(addr) => {
                peer.resolved_at = Some(now);
                if peer.addr != Some(addr) {
                    info!("{} resolves to {}", peer.spec.host, addr.ip());
                    if peer.addr.is_some() {
                        peer.first_ping = None;
                        peer.last_pong = None;
                        peer.connected = false;
                    }
                    peer.addr = Some(addr);
                    self.publish_unicast();
                }
            }
            None => {
                let host = peer.spec.host.clone();
                if peer.addr.is_none() && self.throttle.allow(format!("resolve-{host}"), Duration::from_secs(300), now) {
                    warn!("cannot find the address of peer {host}; trying again every 10 seconds");
                }
            }
        }
    }

    fn publish_unicast(&self) {
        if let Some(tx) = &self.config.unicast {
            let ips: Vec<IpAddr> = self.peers.iter().filter_map(Peer::ip).collect();
            let _ = tx.send(ips);
        }
    }
}

/// Takes the socket rather than the engine: the Opus state is not `Sync`, so no `&Engine` may be held across an await.
async fn send_to(socket: &UdpSocket, packet: &[u8], to: SocketAddr) {
    if let Err(e) = socket.send_to(packet, to).await {
        debug!("send to {to} failed: {e}");
    }
}

/// Fields that mean a new decoder is needed. Capture latency and the like change nothing.
fn same_stream_format(a: &AudioFormat, b: &AudioFormat) -> bool {
    a.sample_rate == b.sample_rate
        && a.channels == b.channels
        && a.codec == b.codec
        && a.frame_samples_per_channel == b.frame_samples_per_channel
        && a.lane == b.lane
}
