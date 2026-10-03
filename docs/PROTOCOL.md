# The RemSound wire protocol, as verified

This is the protocol `remsoundd` speaks. Every claim was checked against the reference sources on 2026-10-03:

- RemSound (C#, MIT), commit `6ccec52` (v6.1, 2026-10-02), cloned from `https://github.com/Ednunp/RemSound`.
- RemSoundAndroid (Kotlin), commit `50c21d0` (2026-06-12), cloned from `https://github.com/aryanchoudharypro/RemSoundAndroid`.

Citations are `file:line` in the RemSound tree unless they say Android. Where the source disagrees with the original build brief (`docs/remsoundd-prompt.md`), the paragraph is marked **Differs from the brief**. Where the brief was right, nothing is marked.

The cross-port contract (`src/RemSound.App/SelfTest.CrossPort.cs`) and the known answers (`src/RemSound.App/SelfTest.KnownAnswers.cs`) are the spec. Their byte strings are ported into `tests/known_answers.rs`.

## Transport

UDP over IPv4.

- The audio port is 47830 (`src/RemSound.Core/RemPacket.cs:171`, pinned at `SelfTest.CrossPort.cs:55`). Format, Audio, Heartbeat, Control, TickProof and AddrCheck packets all travel on it.
- Discovery uses port 47821 (`src/RemSound.Core/PeerDiscoveryService.cs:23`, pinned at `SelfTest.CrossPort.cs:142`).
- Port 47831 is the Windows app's loopback-only DAW plugin link (`SelfTest.CrossPort.cs:199`). The Android app broadcasts its discovery announcement to 47831 (Android `RemReceiver.kt:116`), which no desktop listens on for discovery. `remsoundd` does not copy that.

**Differs from the brief (minor).** The Windows app does not send from 47830. Its sender socket is bound to an ephemeral port (`src/RemSound.Sender/AudioSender.cs:323`), and audio, pings, pongs and tick proofs all leave from that port. Only its listener is on 47830. The receiver's allow-list compares source IP only, never the port (`src/RemSound.Receiver/AudioReceiver.cs:566`). So any source port works. `remsoundd` still sends from its bound 47830 socket, because one socket is simpler and it keeps the NAT pinhole symmetric. It always sends to the peer's port 47830 (or the configured port) and answers pings to the packet's source endpoint, whatever port that is.

## Packet header

12 bytes, little-endian (`RemPacket.cs:83-91`, `RemPacket.cs:187-200`):

- `u32` magic `0x444E4D52`, on the wire `52 4D 4E 44`, "RMND" (`RemPacket.cs:178`)
- `u8` version, always 1 (`RemPacket.cs:179`)
- `u8` type
- `u16` streamId. A writer turns 0 into 1 (`RemPacket.cs:197`), and a reader does the same (`RemPacket.cs:272`).
- `u32` sequence

A reader rejects a packet shorter than 12 bytes, a wrong magic or a version other than 1 (`RemPacket.cs:267-269`).

Known answers (`SelfTest.KnownAnswers.cs:26-35`):

- Audio, stream `0x1234`, sequence `0x0A0B0C0D` is `524D4E4401023412 0D0C0B0A`.
- Audio, stream 0, sequence 1 is `524D4E4401020100 01000000`.
- `524D4E44 01 04 FFFF 07000000` reads as Heartbeat, stream `0xFFFF`, sequence 7.

### Packet types

From `RemPacket.cs:5-39`, pinned at `SelfTest.CrossPort.cs:59-67`:

- 1, Format: unencrypted.
- 2, Audio: encrypted.
- 3, KeepAlive: legacy, dropped silently (`AudioReceiver.cs:1508`).
- 4, Heartbeat: unencrypted.
- 5, Control: sealed remote-volume commands. The receiver only passes on a payload of exactly 38 bytes from an allow-listed sender (`AudioReceiver.cs:1533-1544`). `remsoundd` ignores Control.
- 6 to 9: the relay's lobby (`RemPacket.cs:22`). Out of scope.
- 10, AddrCheck: a relay address proof, echoed verbatim to its sender (`AudioReceiver.cs:1518-1523`). Only matters through the relay. `remsoundd` drops it in v1.
- 11, TickProof: sealed, see below.
- 12, Metronome: `remsoundd` ignores it.

Heartbeats and tick proofs use streamId `0xFFFF` (`HeartbeatService.cs:349`, `TickProof.cs:51`).

## Crypto

All from `src/RemSound.Core/RemSoundCrypto.cs`.

- Key: PBKDF2-HMAC-SHA256 of the UTF-8 password, 100,000 iterations, salt UTF-8 `"RemSound.v1.audio-key"`, 32 bytes (`RemSoundCrypto.cs:50`, `:61`, `:71`, `:117-119`).
- Fingerprint: the same with salt `"RemSound.v1.fingerprint"`, 8 bytes (`RemSoundCrypto.cs:51`, `:72`, `:123-125`).
- Golden vector (`SelfTest.CrossPort.cs:108-118`), re-derived in Python on 2026-10-03: password `remsound cross-port vector` gives key `9CD07772496B22220FAC888EB0F5FBA005953FF2F71AABF391740FB1D9491B74` and fingerprint `A77BF56B9EF1266B`.
- The iteration count is pinned because v5.6 raised it to 600k and silenced the iOS app. v5.7 reverted it (`RemSoundCrypto.cs:55-61`, `SelfTest.CrossPort.cs:98-99`).
- An empty password means no key, so no audio is sent or accepted (`RemSoundCrypto.cs:92-97`, `SenderLane.cs:534`). The Android app derives from the empty string too; `remsoundd` refuses to start without a password instead.

### Sealed payload layout

AES-256-GCM, 16-byte tag, no associated data. On the wire: `nonce(12) || tag(16) || ciphertext`, 28 bytes of overhead (`RemSoundCrypto.cs:127-146`, `:173`, `:187-194`). Known answer: a payload sealed by hand with nonce `0102030405060708090A0B0C` must open (`SelfTest.KnownAnswers.cs:58-78`, `:101-108`).

### Nonces

- Audio uses a counter nonce: a random 6-byte prefix chosen per encryptor instance, then a 6-byte little-endian counter starting at 0 (`RemSoundCrypto.cs:214-230`). A new encryptor gets a new prefix (`SenderLane.cs:637-640`).
- Tick proofs and Control use a fresh random 12-byte nonce per seal (`RemSoundCrypto.cs:131-134`).
- The receiver just reads the nonce off the packet, so the scheme is the sender's business.

## Format packet (type 1)

Unencrypted. Every field is a little-endian `i32` (`RemPacket.cs:209-260`):

- 0: sampleRate
- 4: channels
- 8: bitsPerSample
- 12: encoding
- 16: blockAlign
- 20: avgBytesPerSec
- 24: codec. 1 is PCM (24-bit), 2 is Opus, 3 is custom-mode "Jamulus-style" Opus (`src/RemSound.Core/AudioTransportCodec.cs:5-11`).
- 28: frameSamplesPerChannel, the frame size in samples per channel (since v3.0; it used to be milliseconds, `AudioFormatInfo.cs:10-20`).
- 32: `u8` lane. 0 Mixed, 1 WasapiLane, 2 AsioLane (`src/RemSound.Core/RenderRoute.cs:28-43`). Unknown values read as Mixed (`RemPacket.cs:317-323`).
- 33: `u8` labFlags. Bit 0 means "I can decode custom-mode Opus" (`AudioFormatInfo.cs:38-43`).
- 34 to 35: zero.
- 36: 8-byte password fingerprint.
- 44: `u16` capture latency in tenths of a millisecond, 0 for not stated (`RemPacket.cs:136-141`, `:248-256`).

Sizes are 32, 36, 44 and 46 bytes (`SelfTest.CrossPort.cs:76-80`). A reader requires at least 32 and ignores trailing bytes (`RemPacket.cs:296`). The fingerprint is read only when the payload is at least 44 bytes; a shorter payload means the sender predates encryption (`RemPacket.cs:297-300`). The capture latency is read only when it is at least 46 (`RemPacket.cs:306-309`).

What a Windows sender actually writes (`src/RemSound.Sender/SenderLane.cs:605-607`):

- Opus: 48000, 2, 16, 1, 4, 192000, codec 2, the encoder's frame size, its lane, its capture latency.
- PCM: 48000, 2, 24, 1, 6, 288000, codec 1, the PCM frame size, its lane, its capture latency.

It always sends 46 bytes when it has a password (`SenderLane.cs:617-625`) and resends the Format packet every 250 ms per stream (`SenderLane.cs:37`, `:584-590`). Its labFlags bit is set only while it plays through ASIO with the custom-Opus library loaded (`SenderLane.cs:610-611`).

`remsoundd` sends exactly the Opus row: 48000, 2, 16, 1, 4, 192000, codec 2, 960, lane 0, labFlags 0, its fingerprint, capture latency 0. With labFlags 0 in both its Format packets and its heartbeats, a Windows sender never sends it custom-mode Opus (`SenderLane.cs:287-288`, `HeartbeatService.cs:395`).

Known answer (`SelfTest.KnownAnswers.cs:37-49`): Opus, 120 samples, AsioLane, 3.5 ms, fingerprint `A0..A7` is
`80BB0000 02000000 10000000 01000000 04000000 00EE0200 02000000 78000000 02 00 0000 A0A1A2A3A4A5A6A7 2300`. Its first 32 bytes must still read, as the Mixed lane.

### The receiver's validity rules

`AudioFormatInfo.IsUsable` (`src/RemSound.Core/AudioFormatInfo.cs:78-133`) runs before anything is allocated (`AudioReceiver.cs:1594-1599`):

- channels 1 or 2
- sampleRate 8000 to 192000
- frameSamplesPerChannel 1 to 2880
- codec 1, 2 or 3
- Opus: sampleRate 8000, 12000, 16000, 24000 or 48000
- custom Opus: 48000 and 64, 128 or 256 samples, and the native library present

bitsPerSample, encoding, blockAlign and avgBytesPerSec are deliberately not checked. `remsoundd` applies the same rules, except that it rejects codec 3 because it has no custom-mode decoder.

## Audio packet (type 2)

### Opus

The payload is one sealed raw Opus packet (`SenderLane.cs:552-579`, `:664-671`). The Windows encoder (`src/RemSound.Sender/OpusEncoderState.cs:41-58`, `AudioSender.cs:38`):

- 48 kHz stereo, `OPUS_APPLICATION_RESTRICTED_LOWDELAY`
- 192 kbps, VBR, complexity 10
- in-band FEC on, expected packet loss 10%
- frame size clamped to 120 to 2880 samples per channel. The profile default is 120, 2.5 ms (`src/RemSound.Core/Profile.cs:120`), so a default Windows sender emits 400 packets a second.

The Windows receiver sizes its decode buffer from the announced frame size, with a floor of 120 (`src/RemSound.Receiver/StreamSession.cs:585-587`). So the announced size must be at least the real packet duration. `remsoundd` announces 960 and sends 960-sample (20 ms) frames.

On a single-packet gap the Windows receiver decodes the next packet's FEC first. Larger gaps are counted and not repaired (`StreamSession.cs:589-625`).

### PCM

Each part carries a 6-byte sub-header before its slice (`RemPacket.cs:405-443`):

- `u32` frameId
- `u8` partIndex
- `u8` totalParts

A reader rejects totalParts 0 or partIndex not below totalParts (`RemPacket.cs:441`).

The plaintext of one frame is packed signed 24-bit little-endian, interleaved stereo. A float is clamped to plus or minus 1, multiplied by 8388607 and truncated; a reader divides by 8388607 (`src/RemSound.Core/PcmPack.cs`, known answers at `SelfTest.KnownAnswers.cs:256-263`). The whole frame is sealed once, then the ciphertext is cut into parts of at most 1454 bytes (`SenderLane.cs:521-550`, `RemPacket.cs:185`). Each part gets its own Audio packet, with the wire sequence incrementing per part (`SenderLane.cs:652-662`).

Since 2026-09-15 a Windows sender sizes PCM frames to fit one part: 233 samples per channel (`AudioSender.cs:41-44`). Older senders sent 240-sample frames in two parts.

The Windows reassembler is strictly in order: part 0 starts a frame and any other order discards it (`src/RemSound.Receiver/PcmFrameAssembler.cs:41-79`). `remsoundd` is more tolerant: it assembles parts in any order per frameId and drops incomplete frames after a short window.

The Windows receiver conceals up to 8 lost PCM frames with a 1 ms fade to silence (`StreamSession.cs:489-549`).

## Heartbeat (type 4)

Payload: `u8` kind (0 Ping, 1 Pong), then `i64` little-endian originator timestamp in milliseconds from the originator's own monotonic clock. A 10th byte of flags is added only when set (`RemPacket.cs:142-154`, `:340-368`). A reader requires at least 9 bytes and ignores the rest. Known answers: a Pong with `0x0102030405060708` is `010807060504030201`, and `00 0807060504030201` reads as a Ping (`SelfTest.KnownAnswers.cs:51-56`).

- Every Ping is answered with a Pong that echoes the timestamp verbatim, sent to the Ping's source endpoint (`HeartbeatService.cs:397-418`).
- Windows pings each selected peer once a second at its audio endpoint (`HeartbeatService.cs:45`, `:320-365`).
- A Pong counts only if it echoes one of the sender's last 16 ping timestamps, and it is matched to a peer by IP only (`HeartbeatService.cs:424-446`).
- Health: Healthy with a Pong in the last 2 s, Stale up to 5 s, Unreachable after that, or after 5 s of pinging with no Pong at all (`HeartbeatService.cs:47-49`, `:283-301`).

**Differs from the brief.** Windows does not stop streaming to a peer when it turns Unreachable at 5 s. It stops after the peer has been Unreachable for longer than **30 s** (`src/RemSound.App/MainForm.cs:782`, `src/RemSound.Core/PeerArming.cs:15-34`). It also keeps streaming, however the heartbeat looks, to any peer it has received audio from in the last 3 s (`MainForm.cs:7315-7317`). Because `remsoundd` sends a continuous stream by default, a Windows PC keeps sending it the microphone even across a one-way heartbeat failure.

## TickProof (type 11)

`src/RemSound.Core/TickProof.cs`:

- Header type 11, streamId `0xFFFF` (`TickProof.cs:47-54`).
- Sealed payload of exactly 53 bytes: a random-nonce seal over 25 plaintext bytes (`TickProof.cs:29-32`, `:37-44`):
  - `u8` version, 1
  - `i64` little-endian Unix UTC seconds
  - 16-byte instance GUID in RFC 4122 big-endian order, the order its text form reads
- Sent every 5 s to each ticked peer (`TickProof.cs:35`, `src/RemSound.App/MainForm.AcceptConnections.cs:169-185`).
- Known answer: id `00112233-4455-6677-8899-aabbccddeeff` at 1,790,000,000 s is plaintext `01 803BB16A00000000 00112233445566778899AABBCCDDEEFF` (`SelfTest.KnownAnswers.cs:80-87`).

The guard (`TickProof.cs:77-117`) rejects a seal that does not open with our key ("not our password"), a timestamp more than 10 minutes either side of now, and a replay, keyed by the first 8 bytes of the nonce read little-endian. It remembers up to 4096 nonces, pruning those older than twice the window.

What Windows does with one (`MainForm.AcceptConnections.cs:120-165`): at most one per address every 2 s goes further; in Manual mode nothing happens; otherwise a proof that opens with its key leads to a prompt or, in Automatic mode, to ticking the sender back. It names the sender by looking the GUID up in its discovery list, so `remsoundd` should announce with the same GUID it seals into the proof.

## Discovery (port 47821)

`src/RemSound.Core/PeerDiscoveryService.cs`:

- UTF-8 JSON with exactly these property names, case-sensitive (`PeerDiscoveryService.cs:476`). Pinned form (`SelfTest.CrossPort.cs:150-154`):
  `{"InstanceId":"11111111-2222-3333-4444-555555555555","Name":"ED_DT","AudioPort":47830,"CanSend":true,"CanReceive":false}`
- Announced every 1.5 s (`PeerDiscoveryService.cs:27`) to 255.255.255.255 and every interface's subnet broadcast (`:400-437`), and unicast to the app's known peer IPs and to anyone heard announcing in the last 30 s, capped at 64 (`:303-331`, `:358-393`).
- Peers expire after 8 s of silence (`:31`, `:443-462`).
- A reader drops its own InstanceId, the all-zero GUID, and an AudioPort outside 1 to 65535. A blank name becomes the source IP, and names are cut to 128 characters (`:187-212`).
- The listener binds 0.0.0.0:47821 with SO_REUSEADDR (`:109-112`).

**Differs from the brief (minor).** The Windows app's own InstanceId is a fresh random GUID every run (`PeerDiscoveryService.cs:33`). So it remembers *other* devices by id, but `remsoundd` must not remember a Windows PC by its id across restarts. `remsoundd` identifies peers by configured address, and keeps its own GUID stable in its state directory.

## Receive gate

From `src/RemSound.Receiver/AudioReceiver.cs`:

- Format and Audio are dropped unless the source IP is on the allow-list of ticked peers (`:1601-1612`, `:1844-1848`). Format is parsed and validated first (`:1583-1599`).
- The fingerprint is compared in constant time: no fingerprint means "peer needs update", a different one means "password mismatch" (`:601-617`). The session still opens; its audio simply never decrypts.
- Sessions are keyed by (source endpoint, streamId) (`:1637`). A Format packet matching the existing session changes nothing (`:1641-1644`).
- A new streamId from the same endpoint with the same lane supersedes the older sessions (`:1708-1745`). A Windows sender picks a new random streamId on every start, codec change and frame-size change (`SenderLane.cs:174`, `:193-248`).
- An Audio packet with no session is dropped (`:1863`). So after a session is pruned, audio is lost until the next Format packet arrives.

## Stream lifetime, and burst mode

- A session with no decoded audio for 4 s is pruned (`AudioReceiver.cs:46`, `:1296-1348`).
- Inside the playout, a stop longer than 100 ms disarms playback. When audio returns, nothing plays until the buffer refills to its target, and then it fades in (`src/RemSound.Receiver/SessionPlayout.cs:104-117`, `:827-831`).

So in burst mode, every utterance after a pause of more than 100 ms starts one jitter-buffer target late, with a fade-in. After 4 s of silence the session is gone, and audio is dropped until a Format packet re-opens it. `remsoundd` therefore defaults to `send_mode = "continuous"`, encoding digital silence between utterances. In burst mode it keeps sending Format every 250 ms whether or not audio flows, and sends one immediately before the first frame of a burst.

## Summary of differences from the brief

None of these changes the design.

1. Windows stops streaming to an unreachable peer after 30 s, not about 5 s, and never while it is receiving audio from that peer.
2. Windows sends from an ephemeral port, not 47830. The allow-list is by IP only, so the port does not matter.
3. The Windows app's own discovery GUID changes every run.
4. A default Windows Opus sender uses 120-sample (2.5 ms) frames, 400 packets a second.
5. Burst mode costs one jitter-buffer target plus a fade-in at every utterance after a 100 ms gap, and loses audio for up to 250 ms after a 4 s gap. This confirms continuous mode as the default.
