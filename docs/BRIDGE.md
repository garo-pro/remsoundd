# The bridge socket contract

This is the contract between `remsoundd` and the Hermes Agent gateway plugin. Both sides must stay identical to it. The daemon side is `src/bridge.rs`; a reference client is `tools/bridge_client.py`.

## Connection

- A Unix stream socket, by default `/run/remsoundd/bridge.sock`.
- Mode 0660. Its group is `[bridge] group` in the config (`remsound` in the shipped config). Add the gateway's user to that group.
- One client at a time. A new connection replaces the old one: the old connection is closed, and anything it had queued is dropped. The old client gets no PLAYBACK_DONE for those utterances, because it is gone.
- If no client is connected, the RemSound link stays up and microphone audio is thrown away.

## Framing

Every message, both ways, is:

- `u8` type
- `u32` length, little-endian
- `length` bytes of payload

JSON payloads are UTF-8 objects. Audio payloads are raw signed 16-bit little-endian samples, interleaved when there are two channels. A frame longer than 16 MiB is refused, and the connection is closed.

## Daemon to plugin

### 0x01 HELLO

Sent first, as soon as a client connects.

```json
{"proto": 1, "daemon": "remsoundd 0.1.0", "mic_rate": 16000, "mic_channels": 1}
```

### 0x02 PEER

A RemSound peer connected or was lost. On connect, the client is also sent one `connected` PEER for each peer that is already connected.

```json
{"state": "connected", "name": "PC-DESKTOP", "addr": "100.64.0.5", "rtt_ms": 12}
```

`state` is `connected` or `lost`. `rtt_ms` is the round-trip time when known, otherwise `null`; it is always `null` on `lost`. A peer counts as connected while it answers pings (within the last 5 seconds), and lost once it stops.

### 0x03 MIC

Raw 16 kHz mono s16le microphone audio from the selected peer, in 20 ms chunks of 640 bytes. It flows continuously while that peer is sending, silence included; the plugin decides what to ignore. Lost network packets have already been concealed, so the stream has no holes.

The selected peer is `mic_from` in the config, or else the first peer in `peers` that is sending audio.

### 0x04 PLAYBACK_DONE

```json
{"id": "utt-42"}
```

Sent exactly once for every utterance the client queued:

- after the last frame of that utterance has gone out on the wire, or
- when it is aborted with TTS_ABORT, or
- when there is no connected peer to send it to (it is dropped, and reported done once it has ended).

### 0x7F ERROR

```json
{"message": "unknown cue \"fanfare\"; use listening, done or error"}
```

A message from the client could not be carried out. The connection stays open.

## Plugin to daemon

### 0x11 HELLO

```json
{"proto": 1, "client": "hermes-remsound 1.0.0"}
```

Send it right after reading the daemon's HELLO. A `proto` other than 1 is answered with an ERROR.

### 0x12 TTS_BEGIN

```json
{"id": "utt-42", "sample_rate": 24000, "channels": 1}
```

Opens an utterance. Raw s16le audio at this format follows in TTS_PCM messages.

- `id` must be non-empty and not already queued.
- `sample_rate` may be anything from 1000 to 384000; the daemon resamples it to 48 kHz.
- `channels` is 1 or 2; mono is sent as identical left and right.
- Opening a new utterance while one is still open ends the open one.

### 0x13 TTS_PCM

Raw s16le audio for the open utterance, any length, an even number of bytes. Send it as fast as it is produced: the daemon queues it and paces it out in real time.

### 0x14 TTS_END

```json
{"id": "utt-42"}
```

No more audio for this utterance. The daemon finishes playing what is queued, then sends PLAYBACK_DONE.

### 0x15 TTS_ABORT

```json
{"id": "utt-42"}
```

Drop this utterance's queued audio and send PLAYBACK_DONE at once. An id that is not queued (already done, say) is ignored, so PLAYBACK_DONE is never sent twice.

### 0x16 CUE

```json
{"name": "listening"}
```

Play a short earcon generated in the daemon, mixed over anything else that is playing:

- `listening`: a rising two-tone
- `done`: a falling two-tone
- `error`: two low beeps

## Ordering

- Utterances play in the order their TTS_BEGIN arrived. Each one starts as soon as the one before it finishes.
- If TTS audio arrives more slowly than real time, the gap is filled with silence and the utterance carries on when more arrives.
- Cues do not wait in the queue; they play at once.

## Example session

1. The client connects. The daemon sends HELLO, then PEER `connected` for the PC.
2. The client sends HELLO.
3. MIC frames arrive every 20 ms while the PC sends its microphone.
4. The client sends CUE `done`, TTS_BEGIN `utt-1`, several TTS_PCM, then TTS_END `utt-1`.
5. The daemon sends PLAYBACK_DONE `utt-1` once the last of it has gone out.
