//! remsoundd: a headless RemSound peer for Linux.
//!
//! The transport ([`engine`]) is independent of the bridge socket ([`bridge`]), so other front
//! ends (a PipeWire client, say) can drive it the same way.

pub mod audio;
#[cfg(unix)]
pub mod bridge;
pub mod config;
pub mod crypto;
pub mod discovery;
pub mod engine;
pub mod identity;
pub mod jitter;
pub mod player;
pub mod protocol;
pub mod session;
pub mod tickproof;
