//! Port for verifying inbound webhook/channel signatures.
//!
//! The check is deliberately pure: it takes the RAW request body, the raw
//! signature header and the resolved secret, and answers `bool`. No I/O, no
//! environment lookup and no state — the caller resolves `auth_env` to a value
//! and passes it in, so this port can be unit-tested against published vectors.

/// Verifies a channel's signature header against the raw request body.
pub trait ChannelSignatureVerifier: Send + Sync {
    /// `body` is the unmodified bytes the sender signed. `header` is the raw
    /// signature header value (e.g. `sha256=<hex>` or a bare `<hex>`). `secret`
    /// is the already-resolved shared secret, never an env var name.
    ///
    /// Returns `false` on a missing/malformed header, a missing secret, or a
    /// signature mismatch. The comparison must be constant-time.
    fn verify(&self, body: &[u8], header: &str, secret: &str) -> bool;
}
