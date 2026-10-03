#!/usr/bin/env python3
"""A tiny remsoundd bridge client, for testing without Hermes.

It connects to the bridge socket, prints every PEER event and error, and writes the
microphone audio it receives to a WAV file (16 kHz, mono, 16-bit). Optionally it plays a
WAV file to the PC as TTS first, and reports when the daemon says it has finished.

Examples:

    python3 bridge_client.py --out mic.wav
    python3 bridge_client.py --out mic.wav --seconds 20 --play hello.wav
    python3 bridge_client.py --socket /tmp/bridge.sock --cue listening

Only the Python standard library is needed. See docs/BRIDGE.md for the protocol.
"""

import argparse
import json
import socket
import struct
import sys
import time
import wave

# Daemon to plugin.
HELLO, PEER, MIC, PLAYBACK_DONE, ERROR = 0x01, 0x02, 0x03, 0x04, 0x7F
# Plugin to daemon.
CLIENT_HELLO, TTS_BEGIN, TTS_PCM, TTS_END, TTS_ABORT, CUE = 0x11, 0x12, 0x13, 0x14, 0x15, 0x16

CLIENT_NAME = "bridge_client.py 1.0"


def send(sock, kind, payload):
    if isinstance(payload, (dict, list)):
        payload = json.dumps(payload).encode("utf-8")
    sock.sendall(struct.pack("<BI", kind, len(payload)) + payload)


def recv_exact(sock, n):
    data = bytearray()
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise ConnectionError("the daemon closed the connection")
        data.extend(chunk)
    return bytes(data)


def recv_frame(sock):
    kind, length = struct.unpack("<BI", recv_exact(sock, 5))
    return kind, recv_exact(sock, length)


def play(sock, path, utterance_id):
    """Send a 16-bit WAV file as one utterance."""
    with wave.open(path, "rb") as w:
        if w.getsampwidth() != 2:
            sys.exit(f"{path} is not 16-bit PCM")
        send(sock, TTS_BEGIN, {"id": utterance_id, "sample_rate": w.getframerate(), "channels": w.getnchannels()})
        while True:
            frames = w.readframes(4096)
            if not frames:
                break
            send(sock, TTS_PCM, frames)
    send(sock, TTS_END, {"id": utterance_id})
    print(f"Sent {path} as utterance {utterance_id}.")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--socket", default="/run/remsoundd/bridge.sock", help="the bridge socket")
    parser.add_argument("--out", default="mic.wav", help="WAV file for the microphone audio")
    parser.add_argument("--seconds", type=float, default=0, help="stop after this long; 0 runs until Ctrl+C")
    parser.add_argument("--play", help="a 16-bit WAV file to send to the PC as TTS")
    parser.add_argument("--cue", choices=["listening", "done", "error"], help="play an earcon on the PC")
    args = parser.parse_args()

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.connect(args.socket)
    except OSError as e:
        sys.exit(f"Cannot connect to {args.socket}: {e}. Is remsoundd running, and are you in its group?")

    kind, payload = recv_frame(sock)
    if kind != HELLO:
        sys.exit(f"Expected HELLO from the daemon, got message type 0x{kind:02X}")
    hello = json.loads(payload)
    print(f"Connected to {hello.get('daemon')}, bridge protocol {hello.get('proto')}, "
          f"microphone at {hello.get('mic_rate')} Hz, {hello.get('mic_channels')} channel.")
    send(sock, CLIENT_HELLO, {"proto": 1, "client": CLIENT_NAME})

    if args.cue:
        send(sock, CUE, {"name": args.cue})
    if args.play:
        play(sock, args.play, "test-1")

    rate = hello.get("mic_rate", 16000)
    out = wave.open(args.out, "wb")
    out.setnchannels(1)
    out.setsampwidth(2)
    out.setframerate(rate)
    mic_bytes = 0
    deadline = time.monotonic() + args.seconds if args.seconds > 0 else None
    print(f"Writing microphone audio to {args.out}. " + ("Press Ctrl+C to stop." if deadline is None else f"Stopping after {args.seconds:g} seconds."))
    try:
        while deadline is None or time.monotonic() < deadline:
            if deadline is not None:
                sock.settimeout(max(0.01, deadline - time.monotonic()))
            try:
                kind, payload = recv_frame(sock)
            except socket.timeout:
                break
            if kind == MIC:
                out.writeframes(payload)
                mic_bytes += len(payload)
            elif kind == PEER:
                peer = json.loads(payload)
                rtt = f", round trip {peer['rtt_ms']} ms" if peer.get("rtt_ms") is not None else ""
                print(f"Peer {peer.get('name')} at {peer.get('addr')} is {peer.get('state')}{rtt}.")
            elif kind == PLAYBACK_DONE:
                print(f"Utterance {json.loads(payload).get('id')} has finished playing.")
            elif kind == ERROR:
                print(f"Error from the daemon: {json.loads(payload).get('message')}")
            else:
                print(f"Ignoring unknown message type 0x{kind:02X}")
    except KeyboardInterrupt:
        pass
    except ConnectionError as e:
        print(e)
    finally:
        out.close()
        sock.close()
    print(f"Saved {mic_bytes / 2 / rate:.1f} seconds of microphone audio to {args.out}.")


if __name__ == "__main__":
    main()
