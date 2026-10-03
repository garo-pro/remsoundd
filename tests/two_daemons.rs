//! End to end: two real `remsoundd run` processes on loopback, driven through their bridge
//! sockets exactly as the Hermes plugin drives them.
#![cfg(unix)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use remsoundd::bridge::{frame, msg, read_frame};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::time::timeout;

fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Daemon {
    child: Child,
    socket: std::path::PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(dir: &Path, name: &str, port: u16, peer_port: u16) -> Daemon {
    let socket = dir.join(format!("{name}.sock"));
    let config = dir.join(format!("{name}.toml"));
    std::fs::write(
        &config,
        format!(
            r#"
name = "{name}"
password = "end to end"
peers = ["127.0.0.1:{peer_port}"]
audio_port = {port}
discovery = false
state_dir = "{state}"
log_level = "info"
[bridge]
socket = "{socket}"
"#,
            state = dir.join(name).display(),
            socket = socket.display()
        ),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_remsoundd"))
        .args(["--config", config.to_str().unwrap(), "run"])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    Daemon { child, socket }
}

struct Client {
    stream: UnixStream,
}

impl Client {
    async fn connect(path: &Path) -> Self {
        for _ in 0..100 {
            if let Ok(stream) = UnixStream::connect(path).await {
                let mut c = Self { stream };
                let (kind, payload) = c.next().await;
                assert_eq!(kind, msg::HELLO);
                let hello: Value = serde_json::from_slice(&payload).unwrap();
                assert_eq!(
                    (
                        hello["proto"].as_u64(),
                        hello["mic_rate"].as_u64(),
                        hello["mic_channels"].as_u64()
                    ),
                    (Some(1), Some(16000), Some(1))
                );
                c.send(
                    msg::CLIENT_HELLO,
                    json!({"proto": 1, "client": "test-client 0.0.1"}),
                )
                .await;
                return c;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "the daemon's bridge socket never appeared at {}",
            path.display()
        );
    }

    async fn next(&mut self) -> (u8, Vec<u8>) {
        timeout(Duration::from_secs(10), read_frame(&mut self.stream))
            .await
            .expect("timed out waiting for a frame")
            .unwrap()
            .expect("socket closed")
    }

    async fn send(&mut self, kind: u8, body: Value) {
        self.send_raw(kind, body.to_string().as_bytes()).await;
    }

    async fn send_raw(&mut self, kind: u8, payload: &[u8]) {
        self.stream.write_all(&frame(kind, payload)).await.unwrap();
    }

    async fn until(&mut self, kind: u8) -> Vec<u8> {
        loop {
            let (k, p) = self.next().await;
            if k == kind {
                return p;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tts_from_one_daemon_arrives_as_mic_at_the_other() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (free_port(), free_port());
    let a = spawn(dir.path(), "alpha", pa, pb);
    let b = spawn(dir.path(), "bravo", pb, pa);
    let mut ca = Client::connect(&a.socket).await;
    let mut cb = Client::connect(&b.socket).await;

    let peer: Value = serde_json::from_slice(&ca.until(msg::PEER).await).unwrap();
    assert_eq!(peer["state"], "connected");
    assert_eq!(peer["addr"], "127.0.0.1");
    cb.until(msg::PEER).await;

    // Alpha's client speaks 0.8 s of 400 Hz at 22.05 kHz.
    ca.send(
        msg::TTS_BEGIN,
        json!({"id": "u1", "sample_rate": 22050, "channels": 1}),
    )
    .await;
    let pcm: Vec<u8> = (0..(22_050.0 * 0.8) as usize)
        .flat_map(|i| {
            (((std::f32::consts::TAU * 400.0 * i as f32 / 22_050.0).sin() * 12_000.0) as i16)
                .to_le_bytes()
        })
        .collect();
    for chunk in pcm.chunks(4410) {
        ca.send_raw(msg::TTS_PCM, chunk).await;
    }
    ca.send(msg::TTS_END, json!({"id": "u1"})).await;

    // Bravo's client hears it as 16 kHz mono microphone audio.
    let mut mono = Vec::new();
    let collect = async {
        loop {
            let (k, p) = cb.next().await;
            if k == msg::MIC {
                assert!(p.len().is_multiple_of(2));
                mono.extend(
                    p.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0),
                );
            }
        }
    };
    let _ = timeout(Duration::from_millis(1800), collect).await;
    let loud: Vec<f32> = {
        let start = mono
            .iter()
            .position(|s| s.abs() > 0.05)
            .expect("bravo heard no tone at all");
        let end = mono.iter().rposition(|s| s.abs() > 0.05).unwrap();
        mono[start..=end].to_vec()
    };
    let secs = loud.len() as f32 / 16_000.0;
    assert!(
        (0.75..0.9).contains(&secs),
        "bravo heard {secs:.2} s, expected about 0.8"
    );
    let crossings = loud
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count() as f32;
    let hz = crossings / 2.0 / secs;
    assert!(
        (hz - 400.0).abs() < 10.0,
        "bravo heard {hz:.0} Hz, expected 400"
    );

    let done: Value = serde_json::from_slice(&ca.until(msg::PLAYBACK_DONE).await).unwrap();
    assert_eq!(done["id"], "u1");

    // Mistakes come back as ERROR frames, and the link stays up.
    ca.send(msg::CUE, json!({"name": "fanfare"})).await;
    let err: Value = serde_json::from_slice(&ca.until(msg::ERROR).await).unwrap();
    assert!(err["message"].as_str().unwrap().contains("unknown cue"));
    ca.send(msg::CUE, json!({"name": "listening"})).await;

    // A second client replaces the first.
    let mut ca2 = Client::connect(&a.socket).await;
    let peer: Value = serde_json::from_slice(&ca2.until(msg::PEER).await).unwrap();
    assert_eq!(
        peer["state"], "connected",
        "a new client is told about peers already connected"
    );
    assert!(
        timeout(Duration::from_secs(2), async {
            loop {
                if read_frame(&mut ca.stream).await.unwrap().is_none() {
                    break;
                }
            }
        })
        .await
        .is_ok(),
        "the old client is disconnected"
    );

    drop(b);
    let lost: Value = serde_json::from_slice(&ca2.until(msg::PEER).await).unwrap();
    assert_eq!(
        lost["state"], "lost",
        "a peer that stops answering is reported lost"
    );
}
