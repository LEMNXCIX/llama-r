//! Channel adapters: config loading and signature verification.

pub mod config;
pub mod hmac;

pub use config::load_channels;
pub use hmac::HmacSha256Verifier;
