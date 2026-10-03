#!/bin/bash
# Smoke test for an installed remsoundd package. Run as root after `apt install ./remsoundd_*.deb`.
#
# A second remsoundd plays the part of the Windows PC on port 47840. The installed daemon runs on
# 47830, either as the real systemd service (when systemd is running) or directly as the
# remsoundd user with the same groups (in a container). A 450 Hz tone sent from the fake PC must
# reach a bridge client running as the gateway user, and nobody else may connect.
set -euo pipefail

CLIENT=/usr/share/remsoundd/bridge_client.py
WORK=$(mktemp -d /var/tmp/remsoundd-smoke.XXXXXX)
chmod 1777 "$WORK"  # the gateway user writes its recording here
PIDS=()

say() { printf '\n== %s\n' "$*"; }
fail() { printf 'FAILED: %s\n' "$*" >&2; exit 1; }

cleanup() {
    for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    if [ "${MODE:-}" = systemd ]; then
        systemctl stop remsoundd || true
    fi
}
trap cleanup EXIT

if [ -d /run/systemd/system ]; then MODE=systemd; else MODE=direct; fi
say "remsoundd $(remsoundd --version | cut -d' ' -f2) on $(. /etc/os-release && echo "$PRETTY_NAME"), $MODE mode"

say "the package created its user and group"
id remsoundd
getent group remsound
id hermes >/dev/null 2>&1 || useradd --create-home hermes
usermod -a -G remsound hermes

say "configuration"
install -m 640 -o root -g remsoundd /dev/null /etc/remsoundd/password
printf 'smoke test password\n' > /etc/remsoundd/password
sed -i 's/^peers = .*/peers = ["127.0.0.1:47840"]/' /etc/remsoundd/config.toml
remsoundd check

cat > "$WORK/pc.toml" <<EOF
name = "FAKE-PC"
password = "smoke test password"
peers = ["127.0.0.1:47830"]
audio_port = 47840
discovery = false
state_dir = "$WORK/pc-state"
[bridge]
socket = "$WORK/pc.sock"
EOF
/usr/bin/remsoundd --config "$WORK/pc.toml" run 2> "$WORK/pc.log" &
PIDS+=($!)

say "starting the daemon"
if [ "$MODE" = systemd ]; then
    systemd-analyze verify /usr/lib/systemd/system/remsoundd.service
    systemctl start remsoundd
else
    install -d -o remsoundd -g remsoundd -m 755 /run/remsoundd /var/lib/remsoundd
    runuser -u remsoundd -g remsoundd -G remsound -- /usr/bin/remsoundd run 2> "$WORK/daemon.log" &
    PIDS+=($!)
fi
for _ in $(seq 50); do [ -S /run/remsoundd/bridge.sock ] && break; sleep 0.2; done
[ -S /run/remsoundd/bridge.sock ] || fail "the bridge socket never appeared"
ls -l /run/remsoundd/bridge.sock
[ "$(stat -c '%a %G' /run/remsoundd/bridge.sock)" = "660 remsound" ] || fail "the socket must be mode 660, group remsound"
for _ in $(seq 50); do [ -S "$WORK/pc.sock" ] && break; sleep 0.2; done

say "a tone from the PC reaches the gateway user"
python3 - "$WORK/tone.wav" <<'EOF'
import math, struct, sys, wave
w = wave.open(sys.argv[1], "wb")
w.setnchannels(1); w.setsampwidth(2); w.setframerate(16000)
w.writeframes(b"".join(struct.pack("<h", int(10000 * math.sin(2 * math.pi * 450 * i / 16000))) for i in range(16000)))
w.close()
EOF
chmod 644 "$WORK/tone.wav"
runuser -u hermes -- python3 "$CLIENT" --out "$WORK/heard.wav" --seconds 6 > "$WORK/hermes.out" 2>&1 &
HERMES=$!
sleep 2.5  # time for the two daemons to find each other
python3 "$CLIENT" --socket "$WORK/pc.sock" --out "$WORK/pc-mic.wav" --seconds 3 --play "$WORK/tone.wav"
wait "$HERMES"
cat "$WORK/hermes.out"
python3 - "$WORK/heard.wav" <<'EOF'
import struct, sys, wave
w = wave.open(sys.argv[1]); n = w.getnframes()
s = struct.unpack("<%dh" % n, w.readframes(n))
loud = [i for i, v in enumerate(s) if abs(v) > 1500]
if not loud:
    sys.exit("FAILED: the gateway user heard nothing")
seg = s[loud[0]:loud[-1]]
secs = len(seg) / 16000
hz = sum(1 for i in range(1, len(seg)) if (seg[i - 1] < 0) != (seg[i] < 0)) / 2 / secs
print(f"heard {secs:.2f} s of tone at {hz:.0f} Hz")
if not (0.95 <= secs <= 1.05 and 440 <= hz <= 460):
    sys.exit("FAILED: expected about 1 s at 450 Hz")
EOF

say "other users cannot use the socket"
if runuser -u nobody -- python3 "$CLIENT" --seconds 1 --out /dev/null >/dev/null 2>&1; then
    fail "nobody connected to the bridge socket"
fi
echo "refused, as it should be"

say "logs"
if [ "$MODE" = systemd ]; then
    systemctl stop remsoundd
    journalctl -u remsoundd --no-pager -o cat
    if journalctl -u remsoundd --no-pager -o cat | grep -E ' (WARN|ERROR) '; then fail "warnings or errors in the journal"; fi
    [ ! -e /run/remsoundd ] || fail "the runtime directory should be gone after stopping"
else
    cat "$WORK/daemon.log"
    if grep -E ' (WARN|ERROR) ' "$WORK/daemon.log"; then fail "warnings or errors in the log"; fi
fi

say "smoke test passed"
