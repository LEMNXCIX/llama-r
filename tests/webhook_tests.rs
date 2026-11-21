//! Fase 8 Task 1: channel config, the signature-verifier port and its HMAC
//! adapter.
//!
//! The verifier is pure: it compares a signature over the RAW request body with
//! the value in a header, in constant time, and never trusts a string equality
//! on the hex form. Credentials live in `auth_env` as the NAME of an environment
//! variable — never the value — so a `ChannelConfig` has no secret field to
//! leak through `Debug`.

use llama_r::adapters::channel::{load_channels, HmacSha256Verifier};
use llama_r::domain::channel::{ChannelConfig, ChannelMode};
use llama_r::ports::channel::ChannelSignatureVerifier;
use std::io::Write;
use std::sync::{Arc, Mutex};

// ── HMAC verifier ────────────────────────────────────────────────────────────

/// RFC 4231 test case 2: key = "Jefe", data = "what do ya want for nothing?".
/// Using a published vector proves the implementation, not a second copy of it.
const VECTOR_SECRET: &str = "Jefe";
const VECTOR_BODY: &[u8] = b"what do ya want for nothing?";
const VECTOR_HEX: &str = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";

#[test]
fn hmac_accepts_prefixed_and_bare_hex() {
    let verifier = HmacSha256Verifier;

    // `sha256=<hex>`, bare `<hex>` and uppercase hex all decode to the same
    // 32 bytes and must be accepted.
    assert!(verifier.verify(VECTOR_BODY, &format!("sha256={VECTOR_HEX}"), VECTOR_SECRET));
    assert!(verifier.verify(VECTOR_BODY, VECTOR_HEX, VECTOR_SECRET));
    assert!(verifier.verify(VECTOR_BODY, &VECTOR_HEX.to_ascii_uppercase(), VECTOR_SECRET));
}

#[test]
fn hmac_rejects_missing_malformed_and_wrong_signature() {
    let verifier = HmacSha256Verifier;

    // A one-byte body mutation with an otherwise valid header must fail: the
    // signature is over the RAW bytes, not a parsed/normalised representation.
    let mut mutated = VECTOR_BODY.to_vec();
    mutated[0] = b'X';
    assert!(
        !verifier.verify(&mutated, &format!("sha256={VECTOR_HEX}"), VECTOR_SECRET),
        "a one-byte body mutation must invalidate an otherwise valid header"
    );

    // Wrong digest, too-short hex, non-hex characters, empty header and empty
    // secret are all refusals, never a panic or an accidental match.
    let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
    assert!(!verifier.verify(VECTOR_BODY, &format!("sha256={wrong}"), VECTOR_SECRET));
    assert!(!verifier.verify(VECTOR_BODY, "sha256=deadbeef", VECTOR_SECRET));
    assert!(!verifier.verify(
        VECTOR_BODY,
        "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        VECTOR_SECRET
    ));
    assert!(!verifier.verify(VECTOR_BODY, "", VECTOR_SECRET));
    assert!(!verifier.verify(VECTOR_BODY, VECTOR_HEX, ""));
}

// ── config loader ────────────────────────────────────────────────────────────

#[test]
fn load_channels_missing_file_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let channels = load_channels(tmp.path());
    assert!(
        channels.is_empty(),
        "a missing channels.toml is not an error"
    );
}

/// A shared buffer usable as a tracing writer, read back as text.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn malformed_channels_toml_is_warned_and_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("channels.toml");
    std::fs::write(&path, "this is = not [[[ valid toml").unwrap();

    let buf = SharedBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .without_time()
        .with_target(false)
        .finish();
    let channels = tracing::subscriber::with_default(subscriber, || load_channels(tmp.path()));

    assert!(
        channels.is_empty(),
        "malformed config must not produce channels"
    );
    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains(&path.display().to_string()),
        "the warning must name the full path: {log:?}"
    );
}

/// Sets an env var for the duration of a test and restores its prior value on
/// drop, so a sentinel never leaks into another test in this binary.
struct EnvGuard {
    name: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(name: &'static str, value: &str) -> Self {
        let previous = std::env::var(name).ok();
        std::env::set_var(name, value);
        Self { name, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

#[test]
fn auth_env_is_a_name_not_a_value() {
    const AUTH_ENV: &str = "WEBHOOK_SECRET_TASK1_AUTH_ENV";
    const SENTINEL: &str = "super-secret-value-TASK1-SENTINEL";

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("channels.toml"),
        format!(
            r#"
[[channels]]
id = "stripe"
agent_id = "billing"
mode = "async"
signature_header = "X-Signature"
auth_env = "{AUTH_ENV}"
enabled = true
"#
        ),
    )
    .unwrap();

    // Actually populate the variable named by `auth_env` with a distinctive
    // sentinel before formatting, so the no-leak assertion below is real: if a
    // secret field were ever added and captured this value, it would fail.
    let _guard = EnvGuard::set(AUTH_ENV, SENTINEL);

    let channels = load_channels(tmp.path());
    assert_eq!(channels.len(), 1);
    let cfg: &ChannelConfig = &channels[0];
    assert_eq!(cfg.auth_env, AUTH_ENV);
    assert_eq!(cfg.mode, ChannelMode::Async);
    assert!(cfg.enabled);

    // The struct holds the variable NAME and has no secret field: the sentinel
    // value cannot appear in Debug even though the env var now holds it.
    let debug = format!("{cfg:?}");
    assert!(debug.contains(AUTH_ENV));
    assert!(
        !debug.contains(SENTINEL),
        "Debug leaked the secret value from the env var: {debug:?}"
    );
}
