# remsoundd

[![CI](https://github.com/garo-pro/remsoundd/actions/workflows/ci.yml/badge.svg)](https://github.com/garo-pro/remsoundd/actions/workflows/ci.yml)

remsoundd is a small Linux daemon that speaks the RemSound protocol. It is a full-duplex, headless peer for the Windows RemSound app.

Its first job is to carry audio for a Hermes Agent gateway plugin:

- Your PC sends its microphone to remsoundd. remsoundd turns it into 16 kHz mono audio and hands it to the plugin over a Unix socket.
- The plugin sends speech back over the socket. remsoundd streams it to the PC, where you choose which output device plays it.

It runs as a systemd service, starts at boot, and needs no sound card on the Linux machine. Audio is always encrypted with your RemSound profile password.

## Contents

- Building and installing
- Setting up the Linux side
- Setting up the Windows side
- Testing
- Commands
- The configuration file
- Reading the logs
- Troubleshooting
- More documentation
- License

## Building and installing

Ready-made packages for amd64 are attached to each release on GitHub. They install on Debian 12 and 13 and on Ubuntu 22.04 and 24.04, which CI tests on every change:

```sh
sudo apt install ./remsoundd_*_amd64.deb
```

To build it yourself, you need Rust 1.89 or newer, a C compiler, CMake and the Opus development files. Debian's own Rust packages are older than that, so install Rust with rustup.

```sh
sudo apt install build-essential cmake pkg-config libopus-dev
curl https://sh.rustup.rs -sSf | sh
```

Build and install a Debian package:

```sh
cargo install cargo-deb
cargo deb
sudo apt install ./target/debian/remsoundd_*.deb
```

The package installs the program, the systemd service and an example config at `/etc/remsoundd/config.toml`. It creates a system user called `remsoundd` for the service to run as, and a group called `remsound` for the bridge socket. It does not start the service, because the service cannot start until you have set a password and a peer.

To build without packaging, run `cargo build --release`. The program is then at `target/release/remsoundd`.

## Setting up the Linux side

### Write the password

The password must be the same as your RemSound profile password on the PC. Keep it in a file that only root and the daemon can read:

```sh
sudo install -m 640 -o root -g remsoundd /dev/null /etc/remsoundd/password
sudo nano /etc/remsoundd/password
```

Type the password on the first line and save. A newline at the end is fine.

### Name your PC

Edit `/etc/remsoundd/config.toml` and set `peers` to your PC's address. Over Tailscale, use its Tailscale address or MagicDNS name:

```toml
peers = ["100.64.0.5"]
```

You can also set `name`, which is how this machine appears in RemSound on the PC. It defaults to the host name.

### Let the gateway use the socket

The bridge socket belongs to the `remsound` group. Add the user the Hermes gateway runs as to that group, then restart the gateway:

```sh
sudo adduser hermes remsound
```

### Check and start

Check the config. This reads the password, derives the key and prints the password fingerprint, without starting anything:

```sh
sudo remsoundd check
```

Then start the service, and have it start at boot:

```sh
sudo systemctl enable --now remsoundd
```

## Setting up the Windows side

On the PC, in RemSound:

1. Make sure the profile password matches the one in `/etc/remsoundd/password`.
2. Turn on Send my audio, and tick your microphone under WASAPI audio inputs to send.
3. Turn on Receive audio, and tick your headphones or speakers.
4. Tick the Linux machine. It appears under Discovered peers on the same network. Over Tailscale, add it by its Tailscale address.
5. Optionally, open Preferences, Connectivity, and set Accept connections from other peers to Automatically. remsoundd proves it has the password every 5 seconds, so the PC then ticks it back without asking you.

For the codec, choose Opus over Wi-Fi or Tailscale. PCM is fine on a wired network. remsoundd decodes both. It always sends Opus.

## Testing

### The loopback test

This tests the whole path without Hermes. remsoundd sends your PC's microphone straight back to it, one second late. The test needs the same UDP port as the service, so stop the service first:

```sh
sudo systemctl stop remsoundd
sudo remsoundd loopback-test
```

Speak into the PC's microphone. You should hear yourself about a second later. Press Ctrl+C to stop, then start the service again:

```sh
sudo systemctl start remsoundd
```

### Recording the microphone

To check what the PC sends, record it to a WAV file (48 kHz stereo). Stop the service first, as for the loopback test:

```sh
sudo remsoundd record mic.wav --seconds 10
```

### The test client

`tools/bridge_client.py` talks to a running daemon's bridge socket, exactly as the plugin would. The package installs it as `/usr/share/remsoundd/bridge_client.py`. It prints peer events and writes the microphone audio to a WAV file. It can also play a WAV file to the PC as speech:

```sh
python3 /usr/share/remsoundd/bridge_client.py --out mic.wav --seconds 20
python3 /usr/share/remsoundd/bridge_client.py --play hello.wav --cue listening
```

Run it as a user in the `remsound` group.

## Commands

All commands take `--config PATH`. The default is `/etc/remsoundd/config.toml`.

- `remsoundd run` runs the daemon. This is what the service runs.
- `remsoundd check` validates the config, derives the key and prints the password fingerprint.
- `remsoundd discover` lists the RemSound peers heard on the network for 10 seconds. Use `--seconds` to change that. Stop the service first for complete results.
- `remsoundd loopback-test` sends the PC's microphone back to it a second late. `--delay-ms` changes the delay.
- `remsoundd record FILE` writes the incoming microphone to a WAV file. `--seconds` stops it after that long; otherwise press Ctrl+C.

## The configuration file

The shipped `/etc/remsoundd/config.toml` explains every setting. In short:

- `name`: how this machine appears to other RemSounds.
- `password_file` or `password`: rarely needed. Without them, remsoundd reads `/etc/remsoundd/password`.
- `peers`: your PCs, by address or host name, with an optional port.
- `mic_from`: which peer's microphone goes to the plugin, when there is more than one.
- `discovery`: whether to announce this machine on the network. On by default.
- `log_level`: `info` by default, `debug` for detail.
- `[bridge] socket` and `group`: where the socket is, and which group may use it.
- `[send] mode`: `continuous` by default, which keeps a silent stream running between utterances so playback starts instantly and the PC never drops the connection. `burst` sends only while there is speech, at the cost of a short delay and fade-in at the start of each reply.
- `[send] bitrate`: the Opus bitrate, 96 kbps by default.
- `[receive] jitter_ms`: how long a late packet is waited for before it is replaced, 60 by default.

Unknown settings are errors, so a misspelt name is caught by `remsoundd check`.

## Reading the logs

Logs go to the journal as plain sentences, with no colour codes:

```sh
journalctl -u remsoundd -f
```

Typical lines:

- `PC-DESKTOP connected over 100.64.0.5, round trip 12 ms, password matches`
- `PC-DESKTOP at 100.64.0.5 is sending Opus, 48 kHz stereo, 2.5 ms frames`
- `PC-DESKTOP at 100.64.0.5 is unreachable: no answer to pings for 5 seconds`

## Troubleshooting

### Password mismatch

The log says `password mismatch`. The PC's RemSound profile has a different password. Make `/etc/remsoundd/password` the same, then restart the service.

### Peer needs update

The log says `peer needs update`. The PC runs a RemSound too old to encrypt audio. Update RemSound on the PC.

### The PC never connects

The log says the peer `has not answered pings yet`. Check that RemSound is running on the PC, that the Linux machine is ticked there, and that UDP port 47830 is open in both firewalls. For discovery on a local network, UDP port 47821 must be open too.

### The PC is sending but is ignored

The log says the PC `is not in its peers list`. Add the address it is sending from to `peers`. Over Tailscale, that is its Tailscale address.

### The service will not start

Run `sudo remsoundd check` for a plain explanation. If the journal says it cannot read `/etc/remsoundd/password`, the file is missing or has the wrong owner; repeat the step under Write the password.

## More documentation

- `docs/BRIDGE.md`: the bridge socket contract, shared with the Hermes plugin.
- `docs/PROTOCOL.md`: the RemSound wire protocol as verified against the RemSound sources, with citations.

## License

remsoundd is released under the MIT License; see `LICENSE`.

It is a clean-room implementation of the RemSound protocol, written for Linux. RemSound itself, by Ednunp, is also MIT-licensed: https://github.com/Ednunp/RemSound. The known-answer test vectors in `tests/known_answers.rs` come from RemSound's own self-tests, so the far end can be checked byte for byte.
