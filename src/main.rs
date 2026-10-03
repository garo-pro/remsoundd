use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Parser, Subcommand};
use tokio::sync::{watch, Mutex};
use tracing::{error, info, warn};
use uuid::Uuid;

use remsoundd::config::{Config, DEFAULT_CONFIG_PATH};
use remsoundd::crypto::{hex, Credentials};
use remsoundd::discovery::{self, DiscoveryConfig, PeerTable, SharedPeerTable};
use remsoundd::engine::{self, Command, EngineConfig, EngineHandle, Event};
use remsoundd::identity;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "remsoundd",
    version,
    about = "A headless RemSound peer for Linux"
)]
struct Cli {
    /// Path to the config file.
    #[arg(long, global = true, default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon: the RemSound link plus the bridge socket.
    Run,
    /// Check the config, derive the key and print the password fingerprint.
    Check,
    /// List the RemSound peers heard on the network.
    Discover {
        /// How long to listen, in seconds.
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
    /// Send the PC's microphone straight back to it, a second late, to test the whole path.
    LoopbackTest {
        /// The delay before you hear yourself, in milliseconds.
        #[arg(long, default_value_t = 1000)]
        delay_ms: u64,
    },
    /// Record the incoming microphone audio to a WAV file (48 kHz stereo, 16-bit).
    Record {
        file: PathBuf,
        /// Stop after this many seconds; otherwise press Ctrl+C.
        #[arg(long)]
        seconds: Option<u64>,
    },
}

fn init_logging(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    // Plain lines, no colour codes, no module paths: read through journald with a screen reader.
    // journald adds its own timestamps, so ours are left out there.
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false);
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        builder.without_time().init();
    } else {
        builder.init();
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match &cli.command {
        Cmd::Check => check(&cli.config).await,
        cmd => {
            let config = match Config::load(&cli.config) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("remsoundd: {e:#}");
                    std::process::exit(2);
                }
            };
            init_logging(&config.log_level);
            match cmd {
                Cmd::Run => run(config).await,
                Cmd::Discover { seconds } => discover(config, *seconds).await,
                Cmd::LoopbackTest { delay_ms } => loopback_test(config, *delay_ms).await,
                Cmd::Record { file, seconds } => record(config, file, *seconds).await,
                Cmd::Check => unreachable!(),
            }
        }
    };
    if let Err(e) = result {
        error!("{e:#}");
        eprintln!("remsoundd: {e:#}");
        std::process::exit(1);
    }
}

/// What every link-running subcommand needs: identity, key, discovery and the engine.
struct Link {
    engine: EngineHandle,
    discovery: Option<tokio::task::JoinHandle<()>>,
}

async fn start_link(config: &Config) -> anyhow::Result<Link> {
    let (password, source) = config.resolve_password()?;
    let credentials =
        tokio::task::spawn_blocking(move || Credentials::from_password(&password)).await?;
    info!(
        "password read from {source}; its fingerprint is {}",
        hex(&credentials.fingerprint)
    );
    let (instance, created) = identity::load_or_create(&config.state_dir)?;
    if created {
        info!(
            "created a new instance id {instance} in {}",
            config.state_dir.display()
        );
    }
    let name = config.display_name();
    let peers = config.peer_specs()?;
    if peers.is_empty() {
        warn!("no peers are configured, so nothing will connect. Add your PC's address to peers in the config");
    }
    let names: SharedPeerTable = Arc::new(Mutex::new(PeerTable::default()));
    let (unicast_tx, unicast_rx) = watch::channel(Vec::<IpAddr>::new());
    let engine = engine::start(EngineConfig {
        name: name.clone(),
        instance,
        credentials,
        peers,
        mic_from: config.mic_from.clone(),
        bind: SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.audio_port)),
        send_mode: config.send.mode,
        bitrate: config.send.bitrate,
        jitter: config.jitter(),
        names: Some(names.clone()),
        unicast: Some(unicast_tx),
    })
    .await?;
    let discovery = if config.discovery {
        let dc = DiscoveryConfig {
            instance,
            name: name.clone(),
            audio_port: config.audio_port,
            discovery_port: config.discovery_port,
            can_send: true,
            can_receive: true,
            unicast_targets: unicast_rx,
            announce: true,
        };
        Some(tokio::spawn(async move {
            if let Err(e) = discovery::run(dc, names).await {
                warn!("discovery stopped: {e:#}. Audio still works with peers listed by address");
            }
        }))
    } else {
        None
    };
    info!("remsoundd {VERSION} is up as {name:?} (instance {instance})");
    Ok(Link { engine, discovery })
}

impl Link {
    async fn stop(self) {
        if let Some(d) = self.discovery {
            d.abort();
        }
        self.engine.shutdown().await;
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

fn notify_ready() {
    #[cfg(unix)]
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]);
}

fn notify_stopping() {
    #[cfg(unix)]
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]);
}

#[cfg(unix)]
async fn run(config: Config) -> anyhow::Result<()> {
    use remsoundd::bridge;
    let settings = bridge::BridgeSettings {
        path: config.bridge.socket.clone(),
        group: config.bridge.group.clone(),
        daemon_version: format!("remsoundd {VERSION}"),
    };
    let listener = bridge::bind(&settings)?;
    let mut link = start_link(&config).await?;
    let events = std::mem::replace(&mut link.engine.events, tokio::sync::mpsc::channel(1).1);
    let commands = link.engine.commands.clone();
    let socket_path = settings.path.clone();
    let mut bridge_task = tokio::spawn(bridge::run(listener, settings, commands, events));
    notify_ready();
    let outcome = tokio::select! {
        _ = shutdown_signal() => { info!("stopping"); Ok(()) }
        r = &mut bridge_task => match r {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e.context("the bridge failed")),
            Err(e) => Err(anyhow::anyhow!("the bridge task failed: {e}")),
        },
    };
    notify_stopping();
    bridge_task.abort();
    link.stop().await;
    bridge::cleanup(&socket_path);
    outcome
}

#[cfg(not(unix))]
async fn run(_config: Config) -> anyhow::Result<()> {
    anyhow::bail!("the bridge socket needs a Unix system; use loopback-test or record here")
}

async fn check(path: &Path) -> anyhow::Result<()> {
    println!("Config file: {}", path.display());
    let config = Config::load(path)?;
    println!("The config is valid.");
    println!("Name shown to other RemSounds: {}", config.display_name());
    let (password, source) = config.resolve_password()?;
    println!("Password read from {source}.");
    let started = Instant::now();
    let credentials =
        tokio::task::spawn_blocking(move || Credentials::from_password(&password)).await?;
    println!(
        "Key derived in {} ms. Password fingerprint: {}",
        started.elapsed().as_millis(),
        hex(&credentials.fingerprint)
    );
    let id_path = config.state_dir.join("instance-id");
    match std::fs::read_to_string(&id_path) {
        Ok(text) => match Uuid::parse_str(text.trim()) {
            Ok(id) => println!("Instance id: {id}"),
            Err(_) => println!(
                "Problem: {} does not hold a valid instance id.",
                id_path.display()
            ),
        },
        Err(_) => println!(
            "No instance id yet; one is created in {} on first run.",
            config.state_dir.display()
        ),
    }
    println!(
        "Audio port {}, discovery port {} ({}).",
        config.audio_port,
        config.discovery_port,
        if config.discovery {
            "discovery on"
        } else {
            "discovery off"
        }
    );
    let peers = config.peer_specs()?;
    if peers.is_empty() {
        println!("Problem: no peers are configured. Add your PC's address to peers.");
    }
    for p in &peers {
        match tokio::net::lookup_host((p.host.as_str(), p.port)).await {
            Ok(addrs) => match addrs.into_iter().find(SocketAddr::is_ipv4) {
                Some(a) => println!("Peer {}: {}", p.label, a),
                None => println!("Problem: peer {} has no IPv4 address.", p.label),
            },
            Err(e) => println!("Problem: peer {} cannot be resolved: {e}", p.label),
        }
    }
    println!(
        "Sending Opus at {} kbps in {} mode; waiting up to {} ms for late packets.",
        config.send.bitrate / 1000,
        match config.send.mode {
            remsoundd::config::SendMode::Continuous => "continuous",
            remsoundd::config::SendMode::Burst => "burst",
        },
        config.receive.jitter_ms
    );
    println!("Bridge socket: {}", config.bridge.socket.display());
    #[cfg(unix)]
    if let Some(group) = &config.bridge.group {
        match nix::unistd::Group::from_name(group) {
            Ok(Some(_)) => println!("Bridge group: {group}"),
            _ => println!("Problem: the bridge group {group} does not exist."),
        }
    }
    Ok(())
}

async fn discover(config: Config, seconds: u64) -> anyhow::Result<()> {
    let instance = identity::load_or_create(&config.state_dir)
        .map(|(id, _)| id)
        .unwrap_or_else(|_| Uuid::new_v4());
    let table: SharedPeerTable = Arc::new(Mutex::new(PeerTable::default()));
    // Announce to the configured peers too: over Tailscale a PC only answers someone it has heard.
    let mut targets = Vec::new();
    for p in config.peer_specs()? {
        if let Ok(addrs) = tokio::net::lookup_host((p.host.as_str(), p.port)).await {
            targets.extend(addrs.filter(SocketAddr::is_ipv4).map(|a| a.ip()).take(1));
        }
    }
    let (_tx, rx) = watch::channel(targets);
    let dc = DiscoveryConfig {
        instance,
        name: config.display_name(),
        audio_port: config.audio_port,
        discovery_port: config.discovery_port,
        can_send: true,
        can_receive: true,
        unicast_targets: rx,
        announce: true,
    };
    println!("Listening for RemSound peers for {seconds} seconds...");
    let task = tokio::spawn(discovery::run(dc, table.clone()));
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    task.abort();
    if let Ok(Err(e)) = task.await {
        anyhow::bail!("discovery could not start: {e:#}");
    }
    let heard = table.lock().await.list();
    if heard.is_empty() {
        println!("No peers heard. Check that RemSound is running on the PC, and add its address to peers if it is on Tailscale or another network.");
    }
    for p in heard {
        let a = &p.announcement;
        let roles = match (a.can_send, a.can_receive) {
            (true, true) => "sends and receives audio",
            (true, false) => "sends audio",
            (false, true) => "receives audio",
            (false, false) => "neither sends nor receives",
        };
        println!(
            "{} at {}, audio port {}, {roles}. Instance {}",
            a.name, p.address, a.audio_port, a.instance
        );
    }
    Ok(())
}

async fn loopback_test(config: Config, delay_ms: u64) -> anyhow::Result<()> {
    let mut link = start_link(&config).await?;
    println!("Loopback test: speak into the PC's microphone and you should hear yourself about {delay_ms} ms later. Press Ctrl+C to stop.");
    let commands = link.engine.commands.clone();
    let mut last_mic: Option<Instant> = None;
    let delay_samples = (delay_ms as usize * 48) * 2;
    loop {
        tokio::select! {
            _ = shutdown_signal() => break,
            event = link.engine.events.recv() => match event {
                Some(Event::Mic { samples, .. }) => {
                    // After a pause the delay line has drained: put the delay back in front.
                    if last_mic.is_none_or(|t| t.elapsed() > Duration::from_millis(500)) {
                        let _ = commands.send(Command::Stream(vec![0.0; delay_samples]));
                    }
                    last_mic = Some(Instant::now());
                    let _ = commands.send(Command::Stream(samples));
                }
                Some(Event::PeerConnected { name, .. }) => println!("{name} connected."),
                Some(Event::PeerLost { name, .. }) => println!("{name} was lost."),
                Some(_) => {}
                None => break,
            }
        }
    }
    link.stop().await;
    Ok(())
}

async fn record(config: Config, file: &Path, seconds: Option<u64>) -> anyhow::Result<()> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(file, spec)
        .with_context(|| format!("cannot create {}", file.display()))?;
    let mut link = start_link(&config).await?;
    println!(
        "Recording incoming microphone audio to {}. {}",
        file.display(),
        match seconds {
            Some(s) => format!("Stopping after {s} seconds."),
            None => "Press Ctrl+C to stop.".into(),
        }
    );
    let deadline = seconds.map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    let mut frames = 0u64;
    loop {
        let sleep = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = shutdown_signal() => break,
            _ = sleep => break,
            event = link.engine.events.recv() => match event {
                Some(Event::Mic { samples, .. }) => {
                    for s in &samples {
                        writer.write_sample(remsoundd::audio::f32_to_i16(*s))?;
                    }
                    frames += samples.len() as u64 / 2;
                }
                Some(Event::PeerConnected { name, .. }) => println!("{name} connected."),
                Some(Event::PeerLost { name, .. }) => println!("{name} was lost."),
                Some(_) => {}
                None => break,
            }
        }
    }
    link.stop().await;
    writer.finalize()?;
    println!(
        "Saved {:.1} seconds of audio to {}.",
        frames as f64 / 48_000.0,
        file.display()
    );
    Ok(())
}
