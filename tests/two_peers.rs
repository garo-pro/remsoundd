//! Two transports on loopback, each the other's peer, passing audio both ways.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use remsoundd::config::{PeerSpec, SendMode};
use remsoundd::crypto::Credentials;
use remsoundd::engine::{self, Command, EngineConfig, EngineHandle, Event};
use tokio::time::timeout;
use uuid::Uuid;

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config(port: u16, peer_port: u16, credentials: &Credentials, mode: SendMode) -> EngineConfig {
    EngineConfig {
        name: format!("test-{port}"),
        instance: Uuid::new_v4(),
        credentials: credentials.clone(),
        peers: vec![PeerSpec::parse(&format!("127.0.0.1:{peer_port}"), 47830).unwrap()],
        mic_from: None,
        bind: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        send_mode: mode,
        bitrate: 64_000,
        jitter: Duration::from_millis(40),
        names: None,
        unicast: None,
    }
}

async fn pair(
    a_creds: &Credentials,
    b_creds: &Credentials,
    mode: SendMode,
) -> (EngineHandle, EngineHandle) {
    let (pa, pb) = (free_port(), free_port());
    let a = engine::start(config(pa, pb, a_creds, mode)).await.unwrap();
    let b = engine::start(config(pb, pa, b_creds, mode)).await.unwrap();
    (a, b)
}

async fn wait_connected(h: &mut EngineHandle) {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Event::PeerConnected { .. }) = h.events.recv().await {
                return;
            }
        }
    })
    .await
    .expect("the peers should connect within 5 seconds");
}

fn tone_i16(rate: u32, hz: f32, secs: f32) -> Vec<i16> {
    (0..(rate as f32 * secs) as usize)
        .map(|i| ((std::f32::consts::TAU * hz * i as f32 / rate as f32).sin() * 12_000.0) as i16)
        .collect()
}

/// Collect the received mic audio for a while, as mono.
async fn collect_mic(h: &mut EngineHandle, for_how_long: Duration) -> (Vec<f32>, Vec<String>) {
    let mut mono = Vec::new();
    let mut done = Vec::new();
    let _ = timeout(for_how_long, async {
        while let Some(e) = h.events.recv().await {
            match e {
                Event::Mic { samples, .. } => mono.extend(
                    samples
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|[l, r]| (l + r) / 2.0),
                ),
                Event::PlaybackDone { id } => done.push(id),
                _ => {}
            }
        }
    })
    .await;
    (mono, done)
}

fn loud_part(mono: &[f32]) -> Vec<f32> {
    mono.iter()
        .copied()
        .skip_while(|s| s.abs() < 0.05)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .skip_while(|s| s.abs() < 0.05)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn zero_crossing_hz(mono: &[f32]) -> f32 {
    let crossings = mono
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    crossings as f32 / 2.0 / (mono.len() as f32 / 48_000.0)
}

async fn drain(h: &mut EngineHandle) -> Vec<String> {
    let mut done = Vec::new();
    while let Ok(e) = h.events.try_recv() {
        if let Event::PlaybackDone { id } = e {
            done.push(id);
        }
    }
    done
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_flows_both_ways() {
    let creds = Credentials::from_password("two peers on loopback");
    let (mut a, mut b) = pair(&creds, &creds, SendMode::Continuous).await;
    wait_connected(&mut a).await;
    wait_connected(&mut b).await;

    // A speaks a second of 440 Hz, as TTS at 24 kHz mono; B hears it as microphone audio.
    a.commands
        .send(Command::TtsBegin {
            id: "hello".into(),
            sample_rate: 24_000,
            channels: 1,
        })
        .unwrap();
    a.commands
        .send(Command::TtsPcm(tone_i16(24_000, 440.0, 1.0)))
        .unwrap();
    a.commands
        .send(Command::TtsEnd { id: "hello".into() })
        .unwrap();
    let (heard_by_b, _) = collect_mic(&mut b, Duration::from_millis(1800)).await;
    let tone = loud_part(&heard_by_b);
    let secs = tone.len() as f32 / 48_000.0;
    assert!(
        (0.95..1.1).contains(&secs),
        "B heard {secs:.2} s of tone, expected about 1 s"
    );
    let hz = zero_crossing_hz(&tone);
    assert!(
        (hz - 440.0).abs() < 10.0,
        "B heard {hz:.0} Hz, expected 440"
    );
    let done = drain(&mut a).await;
    assert_eq!(done, ["hello"], "A reports the utterance done exactly once");

    // B sends 660 Hz back as a raw stream (the loopback-test path); A hears it.
    let stereo: Vec<f32> = tone_i16(48_000, 660.0, 0.5)
        .iter()
        .flat_map(|&s| [s as f32 / 32768.0; 2])
        .collect();
    b.commands.send(Command::Stream(stereo)).unwrap();
    let (heard_by_a, _) = collect_mic(&mut a, Duration::from_millis(1200)).await;
    let tone = loud_part(&heard_by_a);
    assert!(
        (0.45..0.6).contains(&(tone.len() as f32 / 48_000.0)),
        "A heard {} samples",
        tone.len()
    );
    let hz = zero_crossing_hz(&tone);
    assert!(
        (hz - 660.0).abs() < 10.0,
        "A heard {hz:.0} Hz, expected 660"
    );

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn burst_mode_still_delivers_every_utterance() {
    let creds = Credentials::from_password("burst");
    let (mut a, mut b) = pair(&creds, &creds, SendMode::Burst).await;
    wait_connected(&mut a).await;
    wait_connected(&mut b).await;
    for (id, hz) in [("one", 300.0), ("two", 500.0)] {
        a.commands
            .send(Command::TtsBegin {
                id: id.into(),
                sample_rate: 16_000,
                channels: 1,
            })
            .unwrap();
        a.commands
            .send(Command::TtsPcm(tone_i16(16_000, hz, 0.4)))
            .unwrap();
        a.commands.send(Command::TtsEnd { id: id.into() }).unwrap();
        let (heard, _) = collect_mic(&mut b, Duration::from_millis(900)).await;
        let tone = loud_part(&heard);
        assert!(
            tone.len() as f32 / 48_000.0 > 0.35,
            "{id}: heard only {} samples",
            tone.len()
        );
        assert!((zero_crossing_hz(&tone) - hz).abs() < 10.0);
        // Long enough for the far end to prune the session, so the next burst must reopen it.
        tokio::time::sleep(Duration::from_millis(4500)).await;
    }
    let done = drain(&mut a).await;
    assert_eq!(done, ["one", "two"]);
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_password_connects_but_hears_nothing() {
    let (mut a, mut b) = pair(
        &Credentials::from_password("one"),
        &Credentials::from_password("two"),
        SendMode::Continuous,
    )
    .await;
    wait_connected(&mut a).await;
    wait_connected(&mut b).await;
    a.commands
        .send(Command::TtsBegin {
            id: "x".into(),
            sample_rate: 48_000,
            channels: 1,
        })
        .unwrap();
    a.commands
        .send(Command::TtsPcm(tone_i16(48_000, 440.0, 0.5)))
        .unwrap();
    a.commands.send(Command::TtsEnd { id: "x".into() }).unwrap();
    let (heard, _) = collect_mic(&mut b, Duration::from_millis(1000)).await;
    assert!(
        heard.is_empty(),
        "audio sealed with another password must never be played"
    );
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_reports_done_and_nothing_plays() {
    let creds = Credentials::from_password("abort");
    let (mut a, mut b) = pair(&creds, &creds, SendMode::Continuous).await;
    wait_connected(&mut a).await;
    wait_connected(&mut b).await;
    a.commands
        .send(Command::TtsBegin {
            id: "long".into(),
            sample_rate: 48_000,
            channels: 1,
        })
        .unwrap();
    a.commands
        .send(Command::TtsPcm(tone_i16(48_000, 440.0, 5.0)))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.commands
        .send(Command::TtsAbort { id: "long".into() })
        .unwrap();
    let (_, done) = collect_mic(&mut a, Duration::from_millis(300)).await;
    assert_eq!(done, ["long"]);
    let (heard, _) = collect_mic(&mut b, Duration::from_millis(800)).await;
    // Allow for what was already in flight when the abort landed.
    let loud = heard.iter().filter(|s| s.abs() > 0.05).count();
    assert!(
        loud < 48_000 * 6 / 10,
        "audio kept playing after the abort ({loud} loud samples)"
    );
    a.shutdown().await;
    b.shutdown().await;
}
