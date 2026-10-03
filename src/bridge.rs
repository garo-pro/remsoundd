//! The bridge: a Unix stream socket that hands microphone audio to one local client (the Hermes
//! plugin) and takes its TTS audio. The contract is in docs/BRIDGE.md; keep both sides identical.
//!
//! Framing: `u8 type`, `u32` little-endian length, then the payload.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::audio::{f32_to_i16, stereo_to_mono, Cue, StreamResampler};
use crate::engine::{Command, Event};

pub const PROTO_VERSION: u64 = 1;
pub const MIC_RATE: u32 = 16_000;
/// 20 ms of 16 kHz mono: the MIC chunk size.
pub const MIC_CHUNK_SAMPLES: usize = 320;
/// Larger frames are refused: nothing legitimate comes close.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;
/// Frames queued for a slow client before microphone audio starts being dropped.
const CLIENT_QUEUE: usize = 4096;

pub mod msg {
    // Daemon to plugin.
    pub const HELLO: u8 = 0x01;
    pub const PEER: u8 = 0x02;
    pub const MIC: u8 = 0x03;
    pub const PLAYBACK_DONE: u8 = 0x04;
    pub const ERROR: u8 = 0x7F;
    // Plugin to daemon.
    pub const CLIENT_HELLO: u8 = 0x11;
    pub const TTS_BEGIN: u8 = 0x12;
    pub const TTS_PCM: u8 = 0x13;
    pub const TTS_END: u8 = 0x14;
    pub const TTS_ABORT: u8 = 0x15;
    pub const CUE: u8 = 0x16;
}

pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Read one frame. Ok(None) at a clean end of stream.
pub async fn read_frame<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match reader.read_exact(&mut head[..1]).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    reader.read_exact(&mut head[1..]).await?;
    let len = u32::from_le_bytes(head[1..5].try_into().unwrap());
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("a frame of {len} bytes is too large"),
        ));
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(Some((head[0], payload)))
}

#[derive(Deserialize)]
struct ClientHello {
    proto: u64,
    #[serde(default)]
    client: String,
}

#[derive(Deserialize)]
struct TtsBegin {
    id: String,
    sample_rate: u32,
    channels: usize,
}

#[derive(Deserialize)]
struct WithId {
    id: String,
}

#[derive(Deserialize)]
struct CueMsg {
    name: String,
}

pub struct BridgeSettings {
    pub path: PathBuf,
    pub group: Option<String>,
    pub daemon_version: String,
}

/// Bind the socket: remove a stale one, then set mode 0660 and the configured group.
pub fn bind(settings: &BridgeSettings) -> anyhow::Result<UnixListener> {
    let path = &settings.path;
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(
            meta.file_type().is_socket(),
            "{} exists and is not a socket; refusing to replace it",
            path.display()
        );
        std::fs::remove_file(path)
            .with_context(|| format!("cannot remove the old socket {}", path.display()))?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("cannot create the bridge socket {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    if let Some(group) = &settings.group {
        let gid = nix::unistd::Group::from_name(group)?
            .with_context(|| format!("the bridge group {group:?} does not exist; create it, or remove group from [bridge] in the config"))?
            .gid;
        nix::unistd::chown(path, None, Some(gid)).with_context(|| {
            format!(
                "cannot give the bridge socket to group {group}; is this daemon a member of it?"
            )
        })?;
    }
    Ok(listener)
}

enum Incoming {
    Frame(u64, u8, Vec<u8>),
    Closed(u64, Option<String>),
}

struct Client {
    id: u64,
    tx: mpsc::Sender<Vec<u8>>,
    reader: tokio::task::JoinHandle<()>,
    /// Utterance ids this client queued and has not yet been told are done.
    open_ids: HashSet<String>,
    name: String,
}

#[derive(Clone)]
struct PeerState {
    name: String,
    rtt_ms: Option<u32>,
}

/// Run the bridge until the listener fails. Owns the engine's event stream.
pub async fn run(
    listener: UnixListener,
    settings: BridgeSettings,
    commands: mpsc::UnboundedSender<Command>,
    mut events: mpsc::Receiver<Event>,
) -> anyhow::Result<()> {
    info!("bridge socket ready at {}", settings.path.display());
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Incoming>();
    let mut client: Option<Client> = None;
    let mut next_id = 0u64;
    let mut peers: HashMap<IpAddr, PeerState> = HashMap::new();
    let mut mic = MicConverter::new()?;
    let mut mic_dropped = 0u64;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("the bridge socket stopped accepting connections")?;
                next_id += 1;
                if let Some(old) = client.take() {
                    info!("a new bridge client connected, replacing {}", old.name);
                    let _ = commands.send(Command::AbortAll);
                }
                let (tx, rx) = mpsc::channel(CLIENT_QUEUE);
                let (read_half, write_half) = stream.into_split();
                tokio::spawn(write_loop(write_half, rx));
                let reader = tokio::spawn(read_loop(next_id, read_half, in_tx.clone()));
                let hello = json!({"proto": PROTO_VERSION, "daemon": settings.daemon_version, "mic_rate": MIC_RATE, "mic_channels": 1});
                let _ = tx.try_send(frame(msg::HELLO, hello.to_string().as_bytes()));
                for (addr, p) in &peers {
                    let _ = tx.try_send(peer_frame("connected", &p.name, *addr, p.rtt_ms));
                }
                mic.reset();
                client = Some(Client { id: next_id, tx, reader, open_ids: HashSet::new(), name: "a client".into() });
                debug!("bridge client {next_id} connected");
            }
            Some(incoming) = in_rx.recv() => match incoming {
                Incoming::Frame(id, kind, payload) => {
                    let Some(c) = client.as_mut().filter(|c| c.id == id) else { continue };
                    if let Err(message) = handle_client_frame(c, kind, &payload, &commands) {
                        warn!("bridge client {}: {message}", c.name);
                        send_control(&mut client, frame(msg::ERROR, json!({"message": message}).to_string().as_bytes()));
                    }
                }
                Incoming::Closed(id, why) => {
                    if client.as_ref().is_some_and(|c| c.id == id) {
                        let c = client.take().unwrap();
                        match why {
                            Some(why) => warn!("bridge client {} disconnected: {why}", c.name),
                            None => info!("bridge client {} disconnected", c.name),
                        }
                        if !c.open_ids.is_empty() {
                            let _ = commands.send(Command::AbortAll);
                        }
                    }
                }
            },
            event = events.recv() => {
                let Some(event) = event else { return Ok(()) };
                match event {
                    Event::Mic { samples, .. } => {
                        let Some(c) = client.as_ref() else { continue };
                        for chunk in mic.push(&samples) {
                            if c.tx.try_send(frame(msg::MIC, &chunk)).is_err() {
                                mic_dropped += 1;
                                if mic_dropped == 1 || mic_dropped % 500 == 0 {
                                    warn!("the bridge client is not reading fast enough; {mic_dropped} microphone chunks dropped so far");
                                }
                            }
                        }
                    }
                    Event::PeerConnected { name, addr, rtt_ms } => {
                        peers.insert(addr, PeerState { name: name.clone(), rtt_ms });
                        send_control(&mut client, peer_frame("connected", &name, addr, rtt_ms));
                    }
                    Event::PeerLost { name, addr } => {
                        peers.remove(&addr);
                        send_control(&mut client, peer_frame("lost", &name, addr, None));
                    }
                    Event::PlaybackDone { id } => {
                        // Only the client that queued it hears about it, and only once.
                        if client.as_mut().is_some_and(|c| c.open_ids.remove(&id)) {
                            send_control(&mut client, frame(msg::PLAYBACK_DONE, json!({"id": id}).to_string().as_bytes()));
                        }
                    }
                    Event::CommandRejected { message } => {
                        send_control(&mut client, frame(msg::ERROR, json!({"message": message}).to_string().as_bytes()));
                    }
                }
            }
        }
    }
}

fn peer_frame(state: &str, name: &str, addr: IpAddr, rtt_ms: Option<u32>) -> Vec<u8> {
    let body = json!({"state": state, "name": name, "addr": addr.to_string(), "rtt_ms": rtt_ms});
    frame(msg::PEER, body.to_string().as_bytes())
}

impl Drop for Client {
    /// Dropping the sender ends the write loop, which closes the socket; stop reading it too.
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Queue a control frame. A client too far behind to take one is cut off rather than misled.
fn send_control(client: &mut Option<Client>, bytes: Vec<u8>) {
    if let Some(c) = client.as_ref() {
        if c.tx.try_send(bytes).is_err() {
            warn!("bridge client {} is not reading; disconnecting it", c.name);
            *client = None;
        }
    }
}

fn handle_client_frame(
    c: &mut Client,
    kind: u8,
    payload: &[u8],
    commands: &mpsc::UnboundedSender<Command>,
) -> Result<(), String> {
    fn parse<'a, T: Deserialize<'a>>(what: &str, payload: &'a [u8]) -> Result<T, String> {
        serde_json::from_slice(payload).map_err(|e| format!("{what} is not valid JSON: {e}"))
    }
    let send = |cmd: Command| {
        commands
            .send(cmd)
            .map_err(|_| "the daemon is shutting down".to_string())
    };
    match kind {
        msg::CLIENT_HELLO => {
            let hello: ClientHello = parse("HELLO", payload)?;
            c.name = if hello.client.is_empty() {
                "a client".into()
            } else {
                hello.client
            };
            info!("bridge client connected: {}", c.name);
            if hello.proto != PROTO_VERSION {
                return Err(format!(
                    "this daemon speaks bridge protocol {PROTO_VERSION}, the client asked for {}",
                    hello.proto
                ));
            }
            Ok(())
        }
        msg::TTS_BEGIN => {
            let begin: TtsBegin = parse("TTS_BEGIN", payload)?;
            if begin.id.is_empty() {
                return Err("TTS_BEGIN needs a non-empty id".into());
            }
            if !(1000..=384_000).contains(&begin.sample_rate) || !(1..=2).contains(&begin.channels)
            {
                return Err(format!(
                    "TTS_BEGIN {}: sample_rate {} and channels {} are not supported",
                    begin.id, begin.sample_rate, begin.channels
                ));
            }
            if !c.open_ids.insert(begin.id.clone()) {
                return Err(format!("utterance id {:?} is already queued", begin.id));
            }
            send(Command::TtsBegin {
                id: begin.id,
                sample_rate: begin.sample_rate,
                channels: begin.channels,
            })
        }
        msg::TTS_PCM => {
            if payload.len() % 2 != 0 {
                return Err("TTS_PCM must hold whole 16-bit samples".into());
            }
            let samples = payload
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            send(Command::TtsPcm(samples))
        }
        msg::TTS_END => send(Command::TtsEnd {
            id: parse::<WithId>("TTS_END", payload)?.id,
        }),
        msg::TTS_ABORT => send(Command::TtsAbort {
            id: parse::<WithId>("TTS_ABORT", payload)?.id,
        }),
        msg::CUE => {
            let cue: CueMsg = parse("CUE", payload)?;
            let cue = Cue::from_name(&cue.name).ok_or_else(|| {
                format!("unknown cue {:?}; use listening, done or error", cue.name)
            })?;
            send(Command::Cue(cue))
        }
        other => Err(format!("unknown message type 0x{other:02X}")),
    }
}

async fn read_loop(
    id: u64,
    mut reader: tokio::net::unix::OwnedReadHalf,
    tx: mpsc::UnboundedSender<Incoming>,
) {
    loop {
        match read_frame(&mut reader).await {
            Ok(Some((kind, payload))) => {
                if tx.send(Incoming::Frame(id, kind, payload)).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = tx.send(Incoming::Closed(id, None));
                return;
            }
            Err(e) => {
                let _ = tx.send(Incoming::Closed(id, Some(e.to_string())));
                return;
            }
        }
    }
}

async fn write_loop(mut writer: tokio::net::unix::OwnedWriteHalf, mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(bytes) = rx.recv().await {
        if writer.write_all(&bytes).await.is_err() {
            return;
        }
    }
}

/// 48 kHz stereo float in, 16 kHz mono s16le out, in 20 ms chunks.
pub struct MicConverter {
    resampler: StreamResampler,
    pending: Vec<u8>,
}

impl MicConverter {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            resampler: StreamResampler::new(48_000, MIC_RATE, 1)?,
            pending: Vec::new(),
        })
    }

    pub fn reset(&mut self) {
        self.pending.clear();
    }

    pub fn push(&mut self, stereo: &[f32]) -> Vec<Vec<u8>> {
        for s in self.resampler.process(&stereo_to_mono(stereo)) {
            self.pending.extend_from_slice(&f32_to_i16(s).to_le_bytes());
        }
        let chunk = MIC_CHUNK_SAMPLES * 2;
        let whole = self.pending.len() / chunk * chunk;
        let out = self.pending[..whole]
            .chunks(chunk)
            .map(<[u8]>::to_vec)
            .collect();
        self.pending.drain(..whole);
        out
    }
}

/// Remove the socket file on shutdown.
pub fn cleanup(path: &Path) {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_oversize_is_refused() {
        let mut bytes = frame(msg::MIC, b"abc");
        bytes.extend(frame(msg::HELLO, b""));
        let mut r = &bytes[..];
        assert_eq!(
            read_frame(&mut r).await.unwrap(),
            Some((msg::MIC, b"abc".to_vec()))
        );
        assert_eq!(
            read_frame(&mut r).await.unwrap(),
            Some((msg::HELLO, vec![]))
        );
        assert_eq!(read_frame(&mut r).await.unwrap(), None);
        let huge = [msg::TTS_PCM, 0xFF, 0xFF, 0xFF, 0xFF];
        assert!(read_frame(&mut &huge[..]).await.is_err());
    }

    #[test]
    fn mic_converter_makes_20ms_chunks() {
        let mut m = MicConverter::new().unwrap();
        let mut chunks = Vec::new();
        for _ in 0..50 {
            chunks.extend(m.push(&vec![0.25f32; 960 * 2]));
        }
        // One second in, minus the resampler's start-up delay still held back.
        assert!(
            chunks.len() >= 48 && chunks.len() <= 50,
            "{} chunks",
            chunks.len()
        );
        assert!(chunks.iter().all(|c| c.len() == 640));
        let last = &chunks[chunks.len() - 1];
        let v = i16::from_le_bytes([last[100], last[101]]);
        assert!(
            (v as i32 - 8192).abs() < 50,
            "a steady 0.25 must come through as about 8192, got {v}"
        );
    }
}
