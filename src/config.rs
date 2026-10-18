//! `/etc/remsoundd/config.toml`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use serde::Deserialize;

use crate::micselect::{SelectParams, DEFAULT_HOLD, DEFAULT_THRESHOLD_DB};
use crate::protocol::{DEFAULT_AUDIO_PORT, DEFAULT_DISCOVERY_PORT};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/remsoundd/config.toml";
/// Read when nothing else names a password; the service gets the same file as a credential.
pub const DEFAULT_PASSWORD_FILE: &str = "/etc/remsoundd/password";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SendMode {
    /// Keep one Opus stream running while a peer is connected, digital silence between utterances.
    Continuous,
    /// Send audio only while there is something to play.
    Burst,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MicMode {
    /// One peer's microphone: the first peer in `peers` that is sending audio.
    #[default]
    Single,
    /// Every sending peer is listened to; the one that is talking goes to the bridge.
    Active,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// What other RemSounds show this machine as. Defaults to the host name.
    pub name: Option<String>,
    /// The profile password. Prefer `password_file`, or a systemd credential called "password".
    pub password: Option<String>,
    pub password_file: Option<PathBuf>,
    /// Peers by IP or host name, optionally with a port: "100.64.0.5", "pc-desktop:47830".
    #[serde(default)]
    pub peers: Vec<String>,
    /// Which peer's microphone goes to the bridge. Defaults to the first connected peer in `peers`.
    pub mic_from: Option<String>,
    /// How the microphone is chosen when `mic_from` is not set.
    #[serde(default)]
    pub mic_mode: MicMode,
    /// With `mic_mode = "active"`: below this level, in dBFS, a peer is not talking.
    #[serde(default = "default_mic_threshold_db")]
    pub mic_threshold_db: f32,
    /// With `mic_mode = "active"`: how long the talking peer keeps the microphone after its
    /// last word, in milliseconds.
    #[serde(default = "default_mic_hold_ms")]
    pub mic_hold_ms: u64,
    #[serde(default = "default_audio_port")]
    pub audio_port: u16,
    #[serde(default = "default_discovery_port")]
    pub discovery_port: u16,
    /// Announce this machine and listen for others on the discovery port.
    #[serde(default = "yes")]
    pub discovery: bool,
    /// Where the stable instance id is kept.
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub bridge: BridgeConfig,
    #[serde(default)]
    pub send: SendConfig,
    #[serde(default)]
    pub receive: ReceiveConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    #[serde(default = "default_socket")]
    pub socket: PathBuf,
    /// Group given access to the socket (mode 0660). None leaves the daemon's own group.
    pub group: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendConfig {
    #[serde(default = "default_send_mode")]
    pub mode: SendMode,
    /// Opus bitrate in bits per second.
    #[serde(default = "default_bitrate")]
    pub bitrate: i32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiveConfig {
    /// How long a missing packet is waited for before it is concealed, in milliseconds.
    #[serde(default = "default_jitter_ms")]
    pub jitter_ms: u64,
}

fn default_audio_port() -> u16 {
    DEFAULT_AUDIO_PORT
}
fn default_discovery_port() -> u16 {
    DEFAULT_DISCOVERY_PORT
}
fn default_mic_threshold_db() -> f32 {
    DEFAULT_THRESHOLD_DB
}
fn default_mic_hold_ms() -> u64 {
    DEFAULT_HOLD.as_millis() as u64
}
fn yes() -> bool {
    true
}
fn default_state_dir() -> PathBuf {
    PathBuf::from("/var/lib/remsoundd")
}
fn default_log_level() -> String {
    "info".into()
}
fn default_socket() -> PathBuf {
    PathBuf::from("/run/remsoundd/bridge.sock")
}
fn default_send_mode() -> SendMode {
    SendMode::Continuous
}
fn default_bitrate() -> i32 {
    96_000
}
fn default_jitter_ms() -> u64 {
    60
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            socket: default_socket(),
            group: None,
        }
    }
}
impl Default for SendConfig {
    fn default() -> Self {
        Self {
            mode: default_send_mode(),
            bitrate: default_bitrate(),
        }
    }
}
impl Default for ReceiveConfig {
    fn default() -> Self {
        Self {
            jitter_ms: default_jitter_ms(),
        }
    }
}

/// A configured peer, before its host name is resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSpec {
    /// Exactly as written in the config, used in log lines when no better name is known.
    pub label: String,
    pub host: String,
    pub port: u16,
}

impl PeerSpec {
    pub fn parse(text: &str, default_port: u16) -> anyhow::Result<Self> {
        let text = text.trim();
        if text.is_empty() {
            bail!("a peer entry is empty");
        }
        // A bare IPv6 address has colons but no port; this daemon is IPv4, so refuse it plainly.
        if text.parse::<std::net::Ipv6Addr>().is_ok() || text.starts_with('[') {
            bail!("peer {text:?} is IPv6; RemSound peers are IPv4");
        }
        let (host, port) = match text.rsplit_once(':') {
            Some((h, p)) => (
                h,
                p.parse::<u16>()
                    .with_context(|| format!("peer {text:?} has a bad port"))?,
            ),
            None => (text, default_port),
        };
        if port == 0 {
            bail!("peer {text:?} has port 0");
        }
        Ok(Self {
            label: text.to_string(),
            host: host.to_string(),
            port,
        })
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the config file {}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("the config file {} is not valid", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.password.is_some() && self.password_file.is_some() {
            bail!("set either password or password_file, not both");
        }
        if !(6_000..=510_000).contains(&self.send.bitrate) {
            bail!(
                "send.bitrate {} is outside Opus's range of 6000 to 510000",
                self.send.bitrate
            );
        }
        if !(10..=1000).contains(&self.receive.jitter_ms) {
            bail!(
                "receive.jitter_ms {} must be between 10 and 1000",
                self.receive.jitter_ms
            );
        }
        if !(-100.0..=0.0).contains(&self.mic_threshold_db) {
            bail!(
                "mic_threshold_db {} must be between -100 and 0",
                self.mic_threshold_db
            );
        }
        if !(100..=10_000).contains(&self.mic_hold_ms) {
            bail!(
                "mic_hold_ms {} must be between 100 and 10000",
                self.mic_hold_ms
            );
        }
        self.peer_specs()?;
        Ok(())
    }

    pub fn peer_specs(&self) -> anyhow::Result<Vec<PeerSpec>> {
        self.peers
            .iter()
            .map(|p| PeerSpec::parse(p, DEFAULT_AUDIO_PORT))
            .collect()
    }

    pub fn display_name(&self) -> String {
        match self.name.as_deref().map(str::trim) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => hostname(),
        }
    }

    pub fn jitter(&self) -> Duration {
        Duration::from_millis(self.receive.jitter_ms)
    }

    /// How `mic_mode = "active"` tells who is talking.
    pub fn mic_select(&self) -> SelectParams {
        SelectParams {
            threshold_db: self.mic_threshold_db,
            hold: Duration::from_millis(self.mic_hold_ms),
        }
    }

    /// The password, and a plain-words description of where it came from (never the password itself).
    ///
    /// Order: `password` in the config, `password_file`, a systemd credential named "password"
    /// (`LoadCredential=password:...`), then `/etc/remsoundd/password`. An empty password is
    /// refused: RemSound sends no audio without one.
    pub fn resolve_password(&self) -> anyhow::Result<(String, String)> {
        let (raw, source) = if let Some(p) = &self.password {
            (p.clone(), "the config file".to_string())
        } else if let Some(path) = &self.password_file {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read the password file {}", path.display()))?;
            (text, format!("the password file {}", path.display()))
        } else if let Some(dir) = std::env::var_os("CREDENTIALS_DIRECTORY") {
            let path = Path::new(&dir).join("password");
            let text = std::fs::read_to_string(&path).with_context(|| {
                format!("cannot read the systemd credential {}", path.display())
            })?;
            (text, "the systemd credential \"password\"".to_string())
        } else if Path::new(DEFAULT_PASSWORD_FILE).exists() {
            let text = std::fs::read_to_string(DEFAULT_PASSWORD_FILE).with_context(|| {
                format!("cannot read {DEFAULT_PASSWORD_FILE}; run this as root")
            })?;
            (text, format!("the password file {DEFAULT_PASSWORD_FILE}"))
        } else {
            bail!("no password is set: write it to {DEFAULT_PASSWORD_FILE}, or set password_file in the config");
        };
        // A file usually ends with a newline that is not part of the password.
        let password = raw
            .strip_suffix('\n')
            .map(|p| p.strip_suffix('\r').unwrap_or(p))
            .unwrap_or(&raw)
            .to_string();
        if password.is_empty() {
            bail!(
                "the password from {source} is empty; RemSound sends no audio without a password"
            );
        }
        Ok((password, source))
    }
}

pub fn hostname() -> String {
    #[cfg(unix)]
    if let Some(name) = nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|h| !h.is_empty())
    {
        return name;
    }
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "remsoundd".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_gets_defaults() {
        let c: Config = toml::from_str(r#"password = "x""#).unwrap();
        c.validate().unwrap();
        assert_eq!(c.audio_port, 47830);
        assert_eq!(c.discovery_port, 47821);
        assert_eq!(c.send.mode, SendMode::Continuous);
        assert_eq!(c.bridge.socket, PathBuf::from("/run/remsoundd/bridge.sock"));
    }

    #[test]
    fn the_microphone_defaults_to_single_mode() {
        let c: Config = toml::from_str(r#"password = "x""#).unwrap();
        assert_eq!(c.mic_from, None);
        assert_eq!(c.mic_mode, MicMode::Single);
        assert_eq!(c.mic_select(), SelectParams::default());
    }

    #[test]
    fn active_microphone_mode_parses() {
        let c: Config = toml::from_str(
            r#"
            password = "x"
            mic_mode = "active"
            mic_threshold_db = -45.0
            mic_hold_ms = 2000
            "#,
        )
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.mic_mode, MicMode::Active);
        assert_eq!(
            c.mic_select(),
            SelectParams {
                threshold_db: -45.0,
                hold: Duration::from_secs(2),
            }
        );
        assert!(toml::from_str::<Config>("mic_mode = 'mix'").is_err());
        let c: Config = toml::from_str("password = 'x'\nmic_threshold_db = 3.0").unwrap();
        assert!(c.validate().is_err());
        let c: Config = toml::from_str("password = 'x'\nmic_hold_ms = 0").unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn full_config_parses() {
        let c: Config = toml::from_str(
            r#"
            name = "hermes"
            password_file = "/etc/remsoundd/password"
            peers = ["100.64.0.5", "pc-desktop:47831"]
            mic_from = "100.64.0.5"
            log_level = "debug"
            [bridge]
            socket = "/tmp/b.sock"
            group = "remsound"
            [send]
            mode = "burst"
            bitrate = 64000
            [receive]
            jitter_ms = 80
            "#,
        )
        .unwrap();
        c.validate().unwrap();
        let peers = c.peer_specs().unwrap();
        assert_eq!(
            peers[1],
            PeerSpec {
                label: "pc-desktop:47831".into(),
                host: "pc-desktop".into(),
                port: 47831
            }
        );
        assert_eq!(c.send.mode, SendMode::Burst);
    }

    #[test]
    fn mistakes_are_caught() {
        assert!(
            toml::from_str::<Config>("pasword = 'x'").is_err(),
            "a misspelt key is an error, not silently ignored"
        );
        let c: Config = toml::from_str("password = 'x'\npassword_file = '/p'").unwrap();
        assert!(c.validate().is_err());
        assert!(PeerSpec::parse("host:0", 47830).is_err());
        assert!(PeerSpec::parse("host:x", 47830).is_err());
        assert!(PeerSpec::parse("::1", 47830).is_err());
    }

    #[test]
    fn password_sources() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw");
        std::fs::write(&file, "secret\n").unwrap();
        let c: Config =
            toml::from_str(&format!("password_file = {:?}", file.to_str().unwrap())).unwrap();
        assert_eq!(c.resolve_password().unwrap().0, "secret");
        std::fs::write(&file, "\n").unwrap();
        assert!(
            c.resolve_password().is_err(),
            "an empty password is refused"
        );
        let c: Config = toml::from_str("password = ''").unwrap();
        assert!(c.resolve_password().is_err());
    }
}
