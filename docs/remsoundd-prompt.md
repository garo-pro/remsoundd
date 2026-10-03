# Build `remsoundd`: a headless RemSound peer for Linux, in Rust

## Goal

Build `remsoundd`, a small Rust daemon for Debian that speaks the RemSound protocol and acts as a full-duplex peer to the Windows RemSound app. It is a clean-room reimplementation for Linux, not a port of the C# code.

Its first job is to be the audio transport for a Hermes Agent gateway plugin (built separately, see "The bridge socket" below):

- **PC to Linux:** the Windows RemSound app sends my microphone to `remsoundd`. The daemon decodes it, downmixes and resamples to 16 kHz mono int16, and streams it to the plugin over a Unix socket.
- **Linux to PC:** the plugin sends TTS audio over the socket. The daemon resamples to 48 kHz stereo, Opus-encodes, encrypts and streams it to the Windows app, where I choose the output device.
- It runs as a systemd service, starts at boot, and needs no audio hardware on the Linux box.

Later it may grow into a general Linux RemSound client (playing to and capturing from PipeWire). Keep the transport layer independent of the bridge socket so that stays easy.

## Before you write any code: verify this plan against the sources

Everything below comes from research on RemSound `main` as of 2026-10-03, including a key-derivation golden vector I checked in Python. Treat it as a strong starting point, not as truth. RemSound changes often.

1. Clone the sources:
   - `git clone https://github.com/Ednunp/RemSound` (C#, MIT): the reference implementation.
   - `git clone https://github.com/aryanchoudharypro/RemSoundAndroid` (Kotlin, receive-only): a small second implementation, useful for cross-checking.
2. Read these files first:
   - `src/RemSound.App/SelfTest.CrossPort.cs`: **the cross-port contract.** Every wire constant other ports depend on, pinned, plus golden vectors. Treat it as the spec.
   - `src/RemSound.App/SelfTest.KnownAnswers.cs`: exact bytes for the header, format payload, heartbeat and sealed payloads. **Port every known answer into your Rust test suite before writing the networking code.**
   - `src/RemSound.Core/RemPacket.cs`: packet header, packet types, format and heartbeat payloads, PCM multipart sub-header.
   - `src/RemSound.Core/RemSoundCrypto.cs`: key derivation, fingerprint, AES-GCM layout, nonce scheme.
   - `src/RemSound.Core/TickProof.cs`: the "I have ticked you and hold the password" packet.
   - `src/RemSound.Core/PeerDiscoveryService.cs`: LAN and unicast discovery.
   - `src/RemSound.Core/HeartbeatService.cs` and `PeerArming.cs`: ping/pong, and why an unreachable peer stops getting audio.
   - `src/RemSound.Core/AudioFormatInfo.cs`: format fields and the receiver's validity rules.
   - `src/RemSound.Sender/SenderLane.cs` and `OpusEncoderState.cs`: how a real sender frames, encodes, encrypts and announces format.
   - `src/RemSound.Receiver/AudioReceiver.cs` and `SessionPlayout.cs`: the allow-list gate, fingerprint check, session keying by (endpoint, streamId), and the idle prune.
   - `server/remsound-relay.py`: a Python implementation of packet handling, handy as readable reference.
3. Write `docs/PROTOCOL.md` in this repo: the wire protocol as you verified it, citing source file and line for each claim. Flag every place where the code disagrees with this prompt.
4. Then **stop and show me a short summary of any disagreements** before implementing. If nothing material changed, say so and continue.

## What I already know about the protocol (verify all of it)

### Transport

- UDP, IPv4. **One port for everything: 47830.** Bind it, and send from the same socket, because the Windows receiver allow-lists peers by **source IP**.
- Discovery uses a separate port, **47821**.
- 47831 is the Windows app's loopback-only DAW-plugin link, not discovery. The Android app announces to 47831, which looks like an outdated bug; don't copy it.

### Packet header (12 bytes, little-endian)

- `u32` magic `0x444E4D52` (bytes `52 4D 4E 44`, "RMND")
- `u8` version `1`
- `u8` type
- `u16` streamId (0 is written as 1)
- `u32` sequence

Packet types:

| Type | Value | Notes |
|---|---|---|
| Format | 1 | unencrypted |
| Audio | 2 | encrypted |
| KeepAlive | 3 | legacy; silently drop |
| Heartbeat | 4 | unencrypted |
| Control | 5 | sealed remote volume commands; you can ignore these |
| relay lobby | 6–9 | only needed for the relay server, out of scope for v1 |
| AddrCheck | 10 | relay address proof; echo verbatim to the sender if you ever use the relay |
| TickProof | 11 | sealed |
| Metronome | 12 | ignore |

### Crypto (must be byte-exact, or audio silently never decrypts)

- **Key:** PBKDF2-HMAC-SHA256, 100,000 iterations, salt = UTF-8 `"RemSound.v1.audio-key"`, 32-byte output.
- **Fingerprint:** same, salt = `"RemSound.v1.fingerprint"`, 8-byte output.
- **Golden vector** (I verified this): password `remsound cross-port vector` gives
  - key `9CD07772496B22220FAC888EB0F5FBA005953FF2F71AABF391740FB1D9491B74`
  - fingerprint `A77BF56B9EF1266B`
- The iteration count matters historically: v5.6 raised it to 600k and broke the iOS app; v5.7 reverted to 100k. Pin it with a test.
- **AES-256-GCM.** Encrypted payload layout: `nonce(12) || tag(16) || ciphertext`, 28 bytes overhead.
- **Nonces for streaming:** random 48-bit prefix per encryptor instance, then a 48-bit little-endian counter. Never reuse a nonce under the same key. A fresh prefix on every restart is mandatory.
- An empty password means no audio at all. Encryption is mandatory. The Android app has a special-case key for an empty password; don't copy that.

### Format packet (type 1, unencrypted)

Payload: 46 bytes when sending with a fingerprint. Little-endian `i32` fields:

| Offset | Field | Value to send |
|---|---|---|
| 0 | sampleRate | 48000 |
| 4 | channels | 2 |
| 8 | bitsPerSample | |
| 12 | encoding | |
| 16 | blockAlign | |
| 20 | avgBytesPerSec | |
| 24 | codec | 1 = PCM 24-bit, 2 = Opus, 3 = custom Opus |
| 28 | frameSamplesPerChannel | |

Then:

- 32: `u8` lane, send 0 (Mixed)
- 33: `u8` labFlags, **send 0** (non-zero would invite custom-mode Opus you can't decode)
- 34–35: zero
- 36: 8-byte password fingerprint
- 44: `u16` capture latency in tenths of a millisecond (0 = not stated)

Copy the exact values a Windows Opus sender writes (`SenderLane.EnsureFormatPacketSent`). A real sender resends Format **every 250 ms**. Readers must accept 32-, 36-, 44- and 46-byte payloads.

### Audio packet (type 2)

- **Opus:** payload = encrypt(one raw Opus packet). Windows encodes 48 kHz stereo with `OPUS_APPLICATION_RESTRICTED_LOWDELAY`, 192 kbps, in-band FEC on, frame sizes 120 to 960 samples per channel.
- **PCM:** a 6-byte sub-header (`u32` frameId, `u8` partIndex, `u8` totalParts) precedes each part. The ciphertext of one whole frame (packed signed 24-bit LE stereo) is split across parts of at most 1454 bytes. Reassemble, then decrypt.
- **Support decoding both.** The Windows user may pick either codec. They will never send you codec 3 as long as your labFlags stay 0.

### Heartbeat (type 4)

- Payload: `u8` kind (0 = Ping, 1 = Pong) then `i64` originator timestamp in ms. A 10-byte variant adds a flags byte; ignore extra bytes.
- Answer every Ping with a Pong that echoes the timestamp verbatim, sent to the packet's source.
- Windows pings each selected peer once a second. A peer whose pongs stop is marked unreachable after about 5 s, and the Windows sender **stops streaming to it**.
- Send your own pings at 1 Hz too, and track RTT and peer health.

### TickProof (type 11)

- Send every **5 s** to each peer you have selected, with streamId `0xFFFF`.
- Sealed payload is 53 bytes = AES-GCM (random nonce) over 25 plaintext bytes:
  - `u8` version = 1
  - `i64` LE Unix seconds
  - 16-byte instance GUID in RFC 4122 big-endian byte order
- This lets the Windows app's "Accept connections from other peers: Automatically" tick you back without me touching the PC.
- When you receive one: verify it, enforce a ±10 min clock-skew window, and reject replayed nonces (`TickProofGuard`).

### Discovery (port 47821)

- UTF-8 JSON, exact property names, case-sensitive, for example `{"InstanceId":"<guid>","Name":"<name>","AudioPort":47830,"CanSend":true,"CanReceive":true}`.
- Announce every 1.5 s to every LAN broadcast address, **and unicast it to each configured peer IP**. Broadcast does not cross Tailscale; I'll likely connect over Tailscale.
- Peers expire after 8 s of silence. Ignore your own InstanceId and the all-zero GUID.
- Keep the instance GUID **stable across restarts** (persist it). The Windows app remembers devices by id.

### Receive gate, mirroring the Windows rules

- Accept Format and Audio only from configured peer IPs.
- Check the sender's fingerprint against yours, and log a clear "password mismatch" or "peer needs update" if it differs or is missing.
- Validate the format before allocating anything (`AudioFormatInfo.IsUsable`).
- Key sessions by (source endpoint, streamId). A new streamId means a fresh session: reset the decoder and buffer.

### Stream lifetime

- The Windows receiver drops a session that has had no audio for **4 s** (`AudioReceiver.SessionIdleTimeout`, `PruneIdleSessions`).
- Recommended default: **keep a continuous outbound Opus stream while a peer is connected, encoding digital silence when there is no TTS.** The Windows session never tears down, there's no warm-up delay at the start of each reply, and "playback done" timing stays predictable. Make it configurable (`send_mode = continuous | burst`).
- Verify against the receiver code what happens on gaps in burst mode.

## Windows-side setup (put this in the README)

On the PC, in RemSound:

- Profile password matches the daemon's.
- **Send my audio** on, with the microphone ticked under WASAPI audio inputs to send.
- **Receive audio** on, with headphones or speakers ticked.
- Tick the Linux box (it appears under Discovered peers, or add it by IP).
- Optionally: Preferences → Connectivity → Accept connections from other peers → Automatically.

Codec: Opus broadcast quality over Wi-Fi or Tailscale; PCM is fine on wired LAN.

## Architecture (adjust after verification)

- **Async runtime:** `tokio`.
- **Codec:** `audiopus` or `opus` crate. Encode 48 kHz stereo, 20 ms frames (960 samples) are fine for speech. Check what the receiver accepts. 64 to 96 kbps is plenty. FEC on.
- **Crypto:** RustCrypto `aes-gcm`, `pbkdf2`, `sha2`, `hmac`. Use `getrandom` / `rand` for nonce prefixes and GUIDs.
- **Resampling:** `rubato`. 48 kHz stereo in becomes 16 kHz mono (average L and R) for the plugin; any rate from the plugin becomes 48 kHz stereo.
- **Jitter handling on receive:** a small jitter buffer (40–80 ms) with Opus PLC and FEC on loss. Speech recognition tolerates a little delay; it doesn't tolerate gaps.
- **Config:** TOML at `/etc/remsoundd/config.toml`, overridable with `--config`. Contents:
  - password, or `password_file`; never log it
  - display name
  - peers: list of IPs or hostnames
  - bridge socket path
  - send mode and bitrate
  - log level
- **Logging:** `tracing`. Log with plain-words context ("PC-DESKTOP connected over 100.64.0.5, password matches"), because I read journald with a screen reader.
- **CLI subcommands:**
  - `remsoundd run`
  - `remsoundd check`: validate config, derive key, print fingerprint
  - `remsoundd discover`: list peers heard for 10 s
  - `remsoundd loopback-test`: receive mic audio and send it straight back with a 1 s delay, so I can test the whole path without Hermes
  - `remsoundd record <file.wav>`: dump decoded incoming audio
- **Packaging:** `systemd/remsoundd.service` with:
  - `Type=notify` or simple, `Restart=on-failure`
  - `DynamicUser=` or a dedicated `remsound` user
  - hardening options
  - a `RuntimeDirectory=` for the socket
  
  Plus a `.deb` via `cargo-deb` if easy.

## The bridge socket (shared contract with the Hermes plugin; keep both sides identical)

Unix stream socket, default `/run/remsoundd/bridge.sock`. Mode 0660, group configurable, so the Hermes gateway user can connect.

**Framing:** `u8 type`, `u32 LE length`, then `length` bytes of payload. JSON payloads are UTF-8.

Daemon → plugin:

| Type | Name | Payload |
|---|---|---|
| 0x01 | HELLO | JSON `{"proto":1,"daemon":"remsoundd x.y.z","mic_rate":16000,"mic_channels":1}` |
| 0x02 | PEER | JSON `{"state":"connected"\|"lost","name":"...","addr":"...","rtt_ms":12}` |
| 0x03 | MIC | raw 16 kHz mono s16le, any length (typically 20 ms = 640 bytes); only from the selected peer |
| 0x04 | PLAYBACK_DONE | JSON `{"id":"<utterance id>"}`, sent once the last frame of that utterance has gone out on the wire |
| 0x7F | ERROR | JSON `{"message":"..."}` |

Plugin → daemon:

| Type | Name | Payload |
|---|---|---|
| 0x11 | HELLO | JSON `{"proto":1,"client":"hermes-remsound x.y.z"}` |
| 0x12 | TTS_BEGIN | JSON `{"id":"...","sample_rate":24000,"channels":1}`, raw s16le follows |
| 0x13 | TTS_PCM | raw s16le at the declared format |
| 0x14 | TTS_END | JSON `{"id":"..."}`; finish queued audio, then send PLAYBACK_DONE |
| 0x15 | TTS_ABORT | JSON `{"id":"..."}`; drop queued audio, then send PLAYBACK_DONE |
| 0x16 | CUE | JSON `{"name":"listening"\|"done"\|"error"}`; play a short earcon generated in the daemon (e.g. rising two-tone for listening, falling for done) |

Rules:

- **One client at a time.** A new connection replaces the old one.
- **MIC flows continuously while a peer is connected.** The plugin decides what to ignore.
- **Utterances queue in order.** PLAYBACK_DONE is always sent exactly once per id, even on abort or peer loss.
- If the socket isn't connected, keep the RemSound link up and drop the mic audio.

Write the contract into `docs/BRIDGE.md`. Add a tiny Python test client in `tools/bridge_client.py` that prints PEER events and writes MIC audio to a WAV. That lets me test without Hermes.

## Testing

- Unit tests:
  - every SelfTest known answer, ported
  - the golden vector
  - header, format and heartbeat round trips
  - PCM multipart reassembly, including out-of-order and missing parts
  - nonce uniqueness
  - TickProof seal and open, skew and replay rejection
  - discovery JSON exact match against the pinned string in `SelfTest.CrossPort.cs`
- An integration test that runs two `remsoundd` instances on loopback with different ports and passes audio both ways.
- Final manual test with me: `remsoundd loopback-test` against my Windows PC. I speak and hear myself back.

## Working style

- Small commits, conventional messages.
- Write the README for a screen-reader user:
  - real headings, short paragraphs
  - commands in code blocks
  - no ASCII diagrams, and no tables used for layout
- Don't add features beyond this scope (no relay groups, no metronome, no remote volume) without asking. Leave clean extension points.
- If the source contradicts this prompt, the source wins. Tell me what changed.
