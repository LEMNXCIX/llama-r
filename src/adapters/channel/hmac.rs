//! HMAC-SHA256 signature verifier.
//!
//! `sha2` is the only crypto crate in the dependency tree; `hmac`, `hex` and
//! `subtle` are not available, so the HMAC construction, hex decoding and the
//! constant-time comparison are implemented here rather than adding a new
//! dependency. The construction follows RFC 2104 with SHA-256's 64-byte block
//! size, and the comparison is a full-width XOR accumulator with no early exit.

use crate::ports::channel::ChannelSignatureVerifier;
use sha2::{Digest, Sha256};

const BLOCK_SIZE: usize = 64;
const DIGEST_LEN: usize = 32;

/// Verifier for `sha256=<hex>` (and bare `<hex>`) signature headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct HmacSha256Verifier;

impl ChannelSignatureVerifier for HmacSha256Verifier {
    fn verify(&self, body: &[u8], header: &str, secret: &str) -> bool {
        // A missing secret or header is a refusal, never a match.
        if secret.is_empty() || header.is_empty() {
            return false;
        }
        let Some(expected) = decode_signature_hex(header) else {
            return false;
        };
        let actual = hmac_sha256(secret.as_bytes(), body);
        constant_time_eq(&actual, &expected)
    }
}

/// HMAC-SHA256 over `data` with `key`, per RFC 2104.
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; DIGEST_LEN] {
    // Keys longer than the block size are first hashed; short keys are padded
    // with zero bytes to the full block.
    let mut key_block = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let hashed = Sha256::digest(key);
        key_block[..DIGEST_LEN].copy_from_slice(&hashed);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; BLOCK_SIZE];
    let mut outer_pad = [0x5cu8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        inner_pad[i] ^= key_block[i];
        outer_pad[i] ^= key_block[i];
    }

    let mut inner = Vec::with_capacity(BLOCK_SIZE + data.len());
    inner.extend_from_slice(&inner_pad);
    inner.extend_from_slice(data);
    let inner_hash = Sha256::digest(&inner);

    let mut outer = Vec::with_capacity(BLOCK_SIZE + DIGEST_LEN);
    outer.extend_from_slice(&outer_pad);
    outer.extend_from_slice(&inner_hash);

    let mut out = [0u8; DIGEST_LEN];
    out.copy_from_slice(&Sha256::digest(&outer));
    out
}

/// Accept `sha256=<hex>` (case-insensitive prefix) or a bare `<hex>` string,
/// then decode exactly 32 bytes. Any other shape is `None`.
fn decode_signature_hex(header: &str) -> Option<[u8; DIGEST_LEN]> {
    const PREFIX: &str = "sha256=";
    let hex_part = header
        .get(..PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
        .map(|_| &header[PREFIX.len()..])
        .unwrap_or(header);

    let bytes = hex_part.as_bytes();
    if bytes.len() != DIGEST_LEN * 2 {
        return None;
    }
    let mut out = [0u8; DIGEST_LEN];
    for (slot, pair) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
        *slot = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Compare two byte strings in constant time: every byte of the shorter input
/// is visited and OR-accumulated, so the running time does not depend on where
/// (or whether) the first differing byte is. A length mismatch can never match
/// — it is folded into the same accumulator rather than checked with an early
/// `return`, so only the (non-secret) lengths can be inferred from timing.
///
/// Shared with the admin-token middleware: a token must be compared the same
/// way as an HMAC digest, never with `==`, which short-circuits on the first
/// differing byte.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().min(b.len()) {
        diff |= (a[i] ^ b[i]) as usize;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4231_vector_matches_published_digest() {
        // RFC 4231 test case 2.
        let digest = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn rfc4231_long_key_vector_matches_published_digest() {
        // RFC 4231 test case 4: a 131-byte key exercises the > block-size path.
        let key = [0xaa; 131];
        let digest = hmac_sha256(
            &key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
}
