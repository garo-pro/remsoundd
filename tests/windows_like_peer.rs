//! The daemon against a peer built by hand to behave like the Windows app (see docs/PROTOCOL.md):
//! a listener on its audio port, but every packet sent from a separate ephemeral socket; Opus at
//! 2.5 ms frames (400 packets a second) with RESTRICTED_LOWDELAY at 192 kbps; and the exact Format
//! bytes SenderLane writes. Only the wire format is shared with the daemon's own code.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use aes_gcm::aead::{AeadInOut, KeyInit, Nonce, Tag};
use aes_gcm::Aes256Gcm;
use remsoundd::config::{PeerSpec, SendMode};
use remsoundd::crypto::Credentials;
use remsoundd::engine::{self, EngineConfig, Event};
use tokio::net::UdpSocket;
use tokio::time::{timeout, Instant};
use uuid::Uuid;

const PASSWORD: &str = "windows like";

fn header(kind: u8, stream: u16, seq: u32) -> Vec<u8> {
    let mut h = vec![0x52, 0x4D, 0x4E, 0x44, 1, kind];
    h.extend_from_slice(&stream.to_le_bytes());
    h.extend_from_slice(&seq.to_le_bytes());
    h
}

fn seal(key: &[u8; 32], counter: u64, plain: &[u8]) -> Vec<u8> {
    let mut nonce = [0xA5u8; 12];
    nonce[6..].copy_from_slice(&counter.to_le_bytes()[..6]);
    let mut body = plain.to_vec();
    let tag = Aes256Gcm::new_from_slice(key)
        .unwrap()
        .encrypt_inout_detached(
            &Nonce::<Aes256Gcm>::try_from(&nonce[..]).unwrap(),
            &[],
            body.as_mut_slice().into(),
        )
        .unwrap();
    [nonce.to_vec(), tag.to_vec(), body].concat()
}

fn open(key: &[u8; 32], sealed: &[u8]) -> Option<Vec<u8>> {
    let mut body = sealed.get(28..)?.to_vec();
    Aes256Gcm::new_from_slice(key)
        .unwrap()
        .decrypt_inout_detached(
            &Nonce::<Aes256Gcm>::try_from(&sealed[..12]).unwrap(),
            &[],
            body.as_mut_slice().into(),
            &Tag::<Aes256Gcm>::try_from(&sealed[12..28]).unwrap(),
        )
        .ok()?;
    Some(body)
}

/// Windows' Opus format announcement (SenderLane.cs:606), 46 bytes, with a capture latency of 10 ms.
fn windows_format(fingerprint: &[u8; 8]) -> Vec<u8> {
    let mut p = Vec::new();
    for v in [48000i32, 2, 16, 1, 4, 192_000, 2, 120] {
        p.extend_from_slice(&v.to_le_bytes());
    }
    p.extend_from_slice(&[0, 0, 0, 0]);
    p.extend_from_slice(fingerprint);
    p.extend_from_slice(&100u16.to_le_bytes());
    p
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn talks_to_a_windows_like_peer() {
    let creds = Credentials::from_password(PASSWORD);
    let key = creds.key;
    // The "PC": a listener on its audio port, and an ephemeral socket everything is sent from.
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let pc_port = listener.local_addr().unwrap().port();
    let daemon_port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let daemon_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, daemon_port));

    let mut daemon = engine::start(EngineConfig {
        name: "linux".into(),
        instance: Uuid::new_v4(),
        credentials: creds.clone(),
        peers: vec![PeerSpec::parse(&format!("127.0.0.1:{pc_port}"), 47830).unwrap()],
        mic_from: None,
        bind: daemon_addr,
        send_mode: SendMode::Continuous,
        bitrate: 96_000,
        jitter: Duration::from_millis(60),
        names: None,
        unicast: None,
    })
    .await
    .unwrap();

    // 1. The PC pings from its ephemeral port; the pong must come back there, stamp echoed.
    let mut ping = header(4, 0xFFFF, 1);
    ping.push(0);
    ping.extend_from_slice(&123_456i64.to_le_bytes());
    sender.send_to(&ping, daemon_addr).await.unwrap();
    let mut buf = [0u8; 2048];
    let (n, _) = timeout(Duration::from_secs(2), sender.recv_from(&mut buf))
        .await
        .expect("no pong")
        .unwrap();
    assert_eq!(&buf[..6], &[0x52, 0x4D, 0x4E, 0x44, 1, 4]);
    assert_eq!(&buf[6..8], &[0xFF, 0xFF], "heartbeats use stream 0xFFFF");
    assert_eq!(buf[12], 1, "a pong");
    assert_eq!(
        i64::from_le_bytes(buf[13..21].try_into().unwrap()),
        123_456,
        "the stamp is echoed verbatim"
    );
    assert_eq!(n, 21, "a 9-byte heartbeat payload, no flags byte");

    // 2. The daemon pings the PC's listener; the PC answers from its ephemeral port. That must count.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (n, from) = timeout(deadline - Instant::now(), listener.recv_from(&mut buf))
            .await
            .expect("no ping")
            .unwrap();
        assert_eq!(
            from, daemon_addr,
            "the daemon sends from its bound audio port"
        );
        if buf[5] == 11 {
            // A tick proof, if one comes first: 12 + 53 bytes, version 1, our time, a GUID.
            assert_eq!(n, 65);
            let plain =
                open(&key, &buf[12..n]).expect("the tick proof must open with the shared key");
            assert_eq!(plain[0], 1);
        }
        if buf[5] == 4 && buf[12] == 0 {
            let mut pong = header(4, 0xFFFF, 2);
            pong.push(1);
            pong.extend_from_slice(&buf[13..21]);
            sender.send_to(&pong, daemon_addr).await.unwrap();
            break;
        }
    }
    let connected = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(Event::PeerConnected { addr, .. }) = daemon.events.recv().await {
                return addr;
            }
        }
    })
    .await
    .expect("a pong from the PC's ephemeral port must connect it");
    assert_eq!(connected.to_string(), "127.0.0.1");

    // 3. The daemon now streams to the PC's listener: Format first, as Windows reads it, then Opus.
    let mut format_seen = false;
    let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
    let mut decoded = 0usize;
    let mut stream_id = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && decoded < 48_000 / 2 {
        let Ok(Ok((n, _))) = timeout(deadline - Instant::now(), listener.recv_from(&mut buf)).await
        else {
            break;
        };
        let stream = u16::from_le_bytes([buf[6], buf[7]]);
        match buf[5] {
            1 => {
                let p = &buf[12..n];
                assert_eq!(
                    p.len(),
                    46,
                    "with a fingerprint the format payload is 46 bytes"
                );
                let i = |at: usize| i32::from_le_bytes(p[at..at + 4].try_into().unwrap());
                assert_eq!((i(0), i(4), i(24), i(28)), (48000, 2, 2, 960));
                assert_eq!(
                    (p[32], p[33]),
                    (0, 0),
                    "lane Mixed, labFlags 0: never ask for custom Opus"
                );
                assert_eq!(&p[36..44], &creds.fingerprint);
                format_seen = true;
                stream_id = Some(stream);
            }
            2 => {
                assert!(
                    format_seen,
                    "a Format packet must come before the first audio packet"
                );
                assert_eq!(Some(stream), stream_id);
                let opus_packet =
                    open(&key, &buf[12..n]).expect("audio must open with the shared key");
                let mut pcm = vec![0f32; 5760 * 2];
                decoded += decoder.decode_float(&opus_packet, &mut pcm, false).unwrap();
            }
            _ => {}
        }
    }
    assert!(
        decoded >= 48_000 / 2,
        "the daemon must stream continuously to a connected PC ({decoded} samples)"
    );

    // 4. The PC sends its microphone: 2.5 ms Opus frames, 400 a second, a fresh random stream id.
    //    One packet in fifty is lost and pairs are swapped now and then, as on Wi-Fi.
    let mut enc =
        opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::LowDelay).unwrap();
    enc.set_bitrate(opus::Bitrate::Bits(192_000)).unwrap();
    enc.set_inband_fec(true).unwrap();
    enc.set_packet_loss_perc(10).unwrap();
    let stream: u16 = 0x5A5A;
    let mut format = header(1, stream, 1);
    format.extend(windows_format(&creds.fingerprint));
    sender.send_to(&format, daemon_addr).await.unwrap();
    let frames = 400; // one second
    let mut packets = Vec::new();
    for seq in 1..=frames as u32 {
        let pcm: Vec<f32> = (0..120)
            .flat_map(|i| {
                let t = (seq as usize - 1) * 120 + i;
                let s = (std::f32::consts::TAU * 700.0 * t as f32 / 48_000.0).sin() * 0.4;
                [s, s]
            })
            .collect();
        let mut out = [0u8; 1500];
        let len = enc.encode_float(&pcm, &mut out).unwrap();
        let mut p = header(2, stream, seq);
        p.extend(seal(&key, seq as u64, &out[..len]));
        packets.push(p);
    }
    let mut order: Vec<usize> = (0..packets.len()).collect();
    for i in (10..order.len() - 1).step_by(37) {
        order.swap(i, i + 1);
    }
    let start = Instant::now();
    for (n, &i) in order.iter().enumerate() {
        if i % 50 != 7 {
            sender.send_to(&packets[i], daemon_addr).await.unwrap();
        }
        tokio::time::sleep_until(start + Duration::from_micros(2500 * (n as u64 + 1))).await;
    }

    let mut mono = Vec::new();
    let _ = timeout(Duration::from_millis(600), async {
        while let Some(e) = daemon.events.recv().await {
            if let Event::Mic { samples, .. } = e {
                mono.extend(
                    samples
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|[l, r]| (l + r) / 2.0),
                );
            }
        }
    })
    .await;
    let start = mono
        .iter()
        .position(|s| s.abs() > 0.05)
        .expect("no microphone audio arrived");
    let end = mono.iter().rposition(|s| s.abs() > 0.05).unwrap();
    let tone = &mono[start..=end];
    let secs = tone.len() as f32 / 48_000.0;
    assert!(
        (0.98..1.02).contains(&secs),
        "one second sent, {secs:.3} s received: lost packets must be concealed, not skipped"
    );
    let crossings = tone
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count() as f32;
    let hz = crossings / 2.0 / secs;
    assert!((hz - 700.0).abs() < 10.0, "{hz:.0} Hz received, 700 sent");

    daemon.shutdown().await;
}
