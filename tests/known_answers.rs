//! The cross-port contract and known answers, ported from RemSound's
//! `SelfTest.CrossPort.cs` and `SelfTest.KnownAnswers.cs` (commit 6ccec52).
//!
//! If one of these fails, do not change the bytes to match. Every other RemSound already in the
//! wild speaks these values, and a mismatch fails silently on the far end.

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce, Tag};
use remsoundd::crypto::{self, Cipher, NonceSequence};
use remsoundd::protocol::{self, pcm, AudioFormat, Codec, HeartbeatKind, PacketType};
use remsoundd::{discovery, tickproof};
use uuid::Uuid;

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn hex_of(b: &[u8]) -> String {
    crypto::hex(b)
}

#[test]
fn wire_constants_are_pinned() {
    assert_eq!(protocol::MAGIC, 0x444E4D52);
    assert_eq!(protocol::VERSION, 1);
    assert_eq!(protocol::HEADER_SIZE, 12);
    assert_eq!(protocol::DEFAULT_AUDIO_PORT, 47830);
    assert_eq!(protocol::DEFAULT_DISCOVERY_PORT, 47821);
    assert_eq!(protocol::MAX_AUDIO_PAYLOAD_BYTES, 1454);
    assert_eq!(PacketType::Format as u8, 1);
    assert_eq!(PacketType::Audio as u8, 2);
    assert_eq!(PacketType::KeepAlive as u8, 3);
    assert_eq!(PacketType::Heartbeat as u8, 4);
    assert_eq!(PacketType::Control as u8, 5);
    assert_eq!(PacketType::AddrCheck as u8, 10);
    assert_eq!(PacketType::TickProof as u8, 11);
    assert_eq!(PacketType::Metronome as u8, 12);
    assert_eq!(tickproof::SEALED_PAYLOAD_BYTES, 53);
    assert_eq!(protocol::FORMAT_PAYLOAD_SIZE, 32);
    assert_eq!(protocol::FORMAT_PAYLOAD_EXTENDED_SIZE, 36);
    assert_eq!(protocol::FORMAT_PAYLOAD_WITH_FINGERPRINT_SIZE, 44);
    assert_eq!(protocol::FORMAT_PAYLOAD_WITH_CAPTURE_SIZE, 46);
    assert_eq!(protocol::CAPTURE_LATENCY_TICKS_PER_MS, 10.0);
    assert_eq!(crypto::FINGERPRINT_BYTES, 8);
    assert_eq!(protocol::HEARTBEAT_PAYLOAD_SIZE, 9);
    assert_eq!(crypto::OVERHEAD_BYTES, 28);
    assert_eq!(Codec::Pcm as i32, 1);
    assert_eq!(Codec::Opus as i32, 2);
    assert_eq!(Codec::OpusCustom as i32, 3);
    assert_eq!(discovery::ANNOUNCE_INTERVAL.as_millis(), 1500);
    assert_eq!(discovery::PEER_EXPIRY.as_millis(), 8000);
    assert_eq!(tickproof::SEND_INTERVAL.as_secs(), 5);
}

#[test]
fn pbkdf2_iterations_are_100k() {
    // v5.6 raised this to 600k and every iPhone went silent.
    assert_eq!(crypto::PBKDF2_ITERATIONS, 100_000);
}

#[test]
fn golden_vector_key_and_fingerprint() {
    let password = "remsound cross-port vector";
    assert_eq!(hex_of(&crypto::derive_key(password)), "9CD07772496B22220FAC888EB0F5FBA005953FF2F71AABF391740FB1D9491B74");
    assert_eq!(hex_of(&crypto::fingerprint(password)), "A77BF56B9EF1266B");
}

#[test]
fn fingerprint_is_not_the_key_prefix() {
    // The fingerprint travels in the clear. Derived with the key's salt, it would leak the key.
    let password = "remsound cross-port vector";
    assert_ne!(&crypto::derive_key(password)[..8], &crypto::fingerprint(password)[..]);
}

#[test]
fn header_bytes() {
    let p = protocol::packet(PacketType::Audio, 0x1234, 0x0A0B0C0D, &[]);
    assert_eq!(hex_of(&p), "524D4E4401023412" .to_string() + "0D0C0B0A");
    let p = protocol::packet(PacketType::Audio, 0, 1, &[]);
    assert_eq!(hex_of(&p), "524D4E440102010001000000", "a stream id of 0 must go out as 1");
    let (h, _) = protocol::read_header(&hex("524D4E44 01 04 FFFF 07000000")).unwrap();
    assert_eq!((h.packet_type(), h.stream_id, h.sequence), (Some(PacketType::Heartbeat), 0xFFFF, 7));
}

const FORMAT_BYTES: &str =
    "80BB0000 02000000 10000000 01000000 04000000 00EE0200 02000000 78000000 02 00 0000 A0A1A2A3A4A5A6A7 2300";

#[test]
fn format_announcement_bytes() {
    let format = AudioFormat { lane: 2, capture_latency_ms: 3.5, ..AudioFormat::opus_48k_stereo(120) };
    let print: [u8; 8] = hex("A0A1A2A3A4A5A6A7").try_into().unwrap();
    let written = protocol::write_format_payload(&format, Some(&print));
    assert_eq!(written.len(), 46);
    assert_eq!(hex_of(&written), hex_of(&hex(FORMAT_BYTES)));

    let (read, fingerprint) = protocol::read_format_payload(&hex(FORMAT_BYTES)).unwrap();
    assert_eq!((read.sample_rate, read.channels, read.codec, read.frame_samples_per_channel, read.lane), (48000, 2, 2, 120, 2));
    assert!((read.capture_latency_ms - 3.5).abs() < 0.01);
    assert_eq!(fingerprint.map(|f| hex_of(&f)), Some("A0A1A2A3A4A5A6A7".into()));

    let (old, fingerprint) = protocol::read_format_payload(&hex(FORMAT_BYTES)[..32]).unwrap();
    assert_eq!((old.sample_rate, old.lane, fingerprint), (48000, 0, None), "the oldest 32-byte form must read, as the mixed lane");
}

#[test]
fn our_opus_format_matches_what_windows_sends() {
    // SenderLane.cs:606: 48000, 2, 16, 1, 4, 192000, Opus, frame size; lane Mixed; labFlags 0.
    let f = AudioFormat::opus_48k_stereo(960);
    let bytes = protocol::write_format_payload(&f, Some(&[0; 8]));
    assert_eq!(hex_of(&bytes[..36]), hex_of(&hex("80BB0000 02000000 10000000 01000000 04000000 00EE0200 02000000 C0030000 00 00 0000")));
    assert_eq!(&bytes[44..], &[0, 0]);
}

#[test]
fn heartbeat_bytes() {
    let beat = protocol::write_heartbeat_payload(HeartbeatKind::Pong, 0x0102030405060708);
    assert_eq!(hex_of(&beat), "010807060504030201");
    assert_eq!(protocol::read_heartbeat_payload(&hex("00 0807060504030201")), Some((HeartbeatKind::Ping, 0x0102030405060708)));
}

fn seal_by_hand(key: &[u8; 32], plain: &[u8]) -> Vec<u8> {
    let nonce = hex("0102030405060708090A0B0C");
    let mut cipher = plain.to_vec();
    let tag = Aes256Gcm::new(key.into()).encrypt_in_place_detached(Nonce::from_slice(&nonce), &[], &mut cipher).unwrap();
    [nonce, tag.to_vec(), cipher].concat()
}

fn open_by_hand(key: &[u8; 32], sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < 28 {
        return None;
    }
    let mut plain = sealed[28..].to_vec();
    Aes256Gcm::new(key.into())
        .decrypt_in_place_detached(Nonce::from_slice(&sealed[..12]), &[], &mut plain, Tag::from_slice(&sealed[12..28]))
        .ok()?;
    Some(plain)
}

#[test]
fn sealed_envelope_both_ways() {
    let key = crypto::derive_key("known answer");
    let cipher = Cipher::new(&key);
    let plain = hex("00112233445566778899");

    let ours = cipher.seal_random(&plain);
    assert_eq!(ours.len(), 12 + 16 + plain.len());
    assert_eq!(open_by_hand(&key, &ours), Some(plain.clone()), "OUR ENVELOPE: nonce, then tag, then ciphertext");

    let theirs = seal_by_hand(&key, &plain);
    assert_eq!(cipher.open(&theirs), Some(plain.clone()), "THEIR ENVELOPE must open");

    let mut nonces = NonceSequence::new();
    let audio = cipher.seal_next(&mut nonces, &plain);
    assert_eq!(open_by_hand(&key, &audio), Some(plain), "OUR AUDIO: the counter-nonce path uses the same layout");
}

#[test]
fn tick_proof_bytes_both_ways() {
    let key = crypto::derive_key("known answer");
    let cipher = Cipher::new(&key);
    let id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
    const WHEN: i64 = 1_790_000_000;
    let tick_plain = hex("01 803BB16A00000000 00112233445566778899AABBCCDDEEFF");

    assert_eq!(open_by_hand(&key, &tickproof::seal(&cipher, id, WHEN)), Some(tick_plain.clone()),
        "OUR TICK PROOF: version 1, time little-endian, id big-endian");
    let proof = tickproof::unseal(&cipher, &seal_by_hand(&key, &tick_plain)).expect("THEIR TICK PROOF must open");
    assert_eq!((proof.instance, proof.unix_secs), (id, WHEN));

    let packet = protocol::packet(PacketType::TickProof, protocol::CONTROL_STREAM_ID, 1, &tickproof::seal(&cipher, id, WHEN));
    assert_eq!(packet.len(), 12 + 53);
    assert_eq!(&packet[6..8], &[0xFF, 0xFF]);
}

#[test]
fn pcm_24_bit_bytes() {
    let mut packed = Vec::new();
    pcm::float_to_int24le(&[0.5, -0.5, -1.0, 1.0, 2.0, 0.25], &mut packed);
    assert_eq!(hex_of(&packed), "FFFF3F0100C0010080FFFF7FFFFF7FFFFF1F");
    let mut unpacked = Vec::new();
    pcm::int24le_to_float(&hex("FFFF7F 0100C0 000080 000000"), &mut unpacked);
    assert_eq!(unpacked[0], 1.0);
    assert!((unpacked[1] + 0.5).abs() < 1e-6);
    assert!(unpacked[2] < -1.0 && unpacked[2] > -1.000001);
    assert_eq!(unpacked[3], 0.0);
}

#[test]
fn discovery_announcement_exact() {
    let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
    assert_eq!(
        discovery::announcement_json(id, "ED_DT", 47830, true, false),
        "{\"InstanceId\":\"11111111-2222-3333-4444-555555555555\",\"Name\":\"ED_DT\",\"AudioPort\":47830,\"CanSend\":true,\"CanReceive\":false}"
    );
    let theirs = b"{\"InstanceId\":\"66666666-7777-8888-9999-000000000000\",\"Name\":\"iPhone\",\"AudioPort\":47830,\"CanSend\":true,\"CanReceive\":true}";
    let heard = discovery::parse_announcement(theirs, "192.168.1.8".parse().unwrap(), id).unwrap();
    assert_eq!(format!("{}@{}", heard.name, heard.audio_port), "iPhone@47830");
}
