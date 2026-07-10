//! Sealing crypto for the secure-input channel.
//!
//! The frontend (browser WebCrypto) seals a small JSON object of typed secret
//! values to a **per-request ephemeral** P-256 public key that Lethe generates
//! and ships in the `secure_input.request` event. Lethe holds the matching
//! secret key only in memory, only for that one request, and unseals here.
//!
//! Scheme (must match `web/src/crypto.ts` byte-for-byte):
//!   1. ECDH P-256 between the browser's ephemeral keypair and our per-request
//!      ephemeral keypair. The shared secret is the 32-byte X coordinate
//!      (WebCrypto `deriveBits(256)` / p256 `SharedSecret::raw_secret_bytes`).
//!   2. HKDF-SHA256 over that shared secret, `salt` = a random 32 bytes chosen
//!      by the browser (sent in the clear alongside the ciphertext),
//!      `info` = b"lethe-secure-input-v1", output = a 32-byte AES-256 key.
//!   3. AES-256-GCM, `nonce` = a random 12-byte IV chosen by the browser,
//!      `aad` = utf8(request_id) ‖ server_pub (the raw 65-byte uncompressed
//!      point we published). Binding the request id + our public key as AAD is
//!      what stops a ciphertext sealed for one pending request from being
//!      spliced into another (defense in depth on top of the per-request key).
//!
//! Everything is standard (not URL-safe) base64 on the wire, matching the
//! JS `btoa`/`atob` and the Rust `base64` STANDARD engine.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hkdf::Hkdf;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use rand::RngCore;
use sha2::Sha256;
use zeroize::Zeroize;

pub const ALG: &str = "ECDH-P256+HKDF-SHA256+A256GCM";
const HKDF_INFO: &[u8] = b"lethe-secure-input-v1";

/// A per-request ephemeral server keypair. The secret never leaves the process
/// and is dropped (zeroized by `p256::SecretKey`) when the pending request is
/// resolved, cancelled, or expires.
pub struct ServerEphemeral {
    secret: SecretKey,
    /// Uncompressed SEC1 point (0x04 ‖ X ‖ Y), 65 bytes. Published to the client.
    public_point: Vec<u8>,
}

impl ServerEphemeral {
    /// Generate a fresh keypair from OS randomness. We build the scalar from 32
    /// random bytes (retrying on the astronomically rare out-of-range draw)
    /// rather than `SecretKey::random` to avoid coupling to a specific
    /// `rand_core` version across the crate's dependency graph.
    pub fn generate() -> Self {
        loop {
            let mut bytes = [0u8; 32];
            rand::rng().fill_bytes(&mut bytes);
            if let Ok(secret) = SecretKey::from_slice(&bytes) {
                bytes.zeroize();
                let public_point = secret
                    .public_key()
                    .to_encoded_point(false)
                    .as_bytes()
                    .to_vec();
                return Self {
                    secret,
                    public_point,
                };
            }
            bytes.zeroize();
        }
    }

    /// Base64 of the uncompressed public point, for the SSE event.
    pub fn public_b64(&self) -> String {
        B64.encode(&self.public_point)
    }

    pub fn public_point(&self) -> &[u8] {
        &self.public_point
    }
}

/// A sealed envelope as it arrives from `POST /secure-input` (all base64).
#[derive(Debug, Clone)]
pub struct SealedInput {
    pub client_pub: String,
    pub salt: String,
    pub iv: String,
    pub ciphertext: String,
}

#[derive(Debug)]
pub enum UnsealError {
    Base64(&'static str),
    ClientKey,
    BadLength(&'static str),
    Decrypt,
    Utf8,
}

impl std::fmt::Display for UnsealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnsealError::Base64(field) => write!(f, "invalid base64 in `{field}`"),
            UnsealError::ClientKey => write!(f, "invalid client public key"),
            UnsealError::BadLength(field) => write!(f, "wrong length for `{field}`"),
            UnsealError::Decrypt => write!(f, "authenticated decryption failed"),
            UnsealError::Utf8 => write!(f, "plaintext was not valid UTF-8"),
        }
    }
}

/// Unseal the envelope into the plaintext JSON bytes (the values object). The
/// caller is responsible for zeroizing the returned buffer once consumed.
pub fn unseal(
    server: &ServerEphemeral,
    sealed: &SealedInput,
    request_id: &str,
) -> Result<Vec<u8>, UnsealError> {
    let client_pub_bytes = B64
        .decode(sealed.client_pub.as_bytes())
        .map_err(|_| UnsealError::Base64("client_pub"))?;
    let salt = B64
        .decode(sealed.salt.as_bytes())
        .map_err(|_| UnsealError::Base64("salt"))?;
    let iv = B64
        .decode(sealed.iv.as_bytes())
        .map_err(|_| UnsealError::Base64("iv"))?;
    let ciphertext = B64
        .decode(sealed.ciphertext.as_bytes())
        .map_err(|_| UnsealError::Base64("ciphertext"))?;

    if iv.len() != 12 {
        return Err(UnsealError::BadLength("iv"));
    }

    let client_pub =
        PublicKey::from_sec1_bytes(&client_pub_bytes).map_err(|_| UnsealError::ClientKey)?;

    // ECDH → 32-byte shared X coordinate.
    let shared =
        p256::ecdh::diffie_hellman(server.secret.to_nonzero_scalar(), client_pub.as_affine());
    let mut ikm = shared.raw_secret_bytes().to_vec();

    // HKDF-SHA256 → 32-byte AES key.
    let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut key = [0u8; 32];
    // `expand` only fails for absurd output lengths; 32 bytes never does.
    hk.expand(HKDF_INFO, &mut key)
        .map_err(|_| UnsealError::Decrypt)?;
    ikm.zeroize();

    // AES-256-GCM with AAD = request_id ‖ server_pub.
    let mut aad = Vec::with_capacity(request_id.len() + server.public_point.len());
    aad.extend_from_slice(request_id.as_bytes());
    aad.extend_from_slice(&server.public_point);

    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| UnsealError::Decrypt)?;
    key.zeroize();
    let nonce = Nonce::from_slice(&iv);
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| UnsealError::Decrypt)?;

    // Validate UTF-8 up front so callers get a clean error, not a panic.
    if std::str::from_utf8(&plaintext).is_err() {
        let mut p = plaintext;
        p.zeroize();
        return Err(UnsealError::Utf8);
    }
    Ok(plaintext)
}

/// Seal exactly as `web/src/crypto.ts` does — test-only, but shared between the
/// crypto roundtrip tests and the secure-prompt socket integration test to prove
/// the Rust unseal is wire-compatible with the browser.
#[cfg(test)]
pub(crate) fn seal_for_test(
    server_pub_b64: &str,
    request_id: &str,
    values_json: &[u8],
) -> SealedInput {
    use aes_gcm::aead::Aead;

    let server_pub_bytes = B64.decode(server_pub_b64).unwrap();
    let server_pub = PublicKey::from_sec1_bytes(&server_pub_bytes).unwrap();

    // Client ephemeral keypair.
    let client_secret = loop {
        let mut b = [0u8; 32];
        rand::rng().fill_bytes(&mut b);
        if let Ok(s) = SecretKey::from_slice(&b) {
            break s;
        }
    };
    let client_pub_point = client_secret
        .public_key()
        .to_encoded_point(false)
        .as_bytes()
        .to_vec();

    let shared =
        p256::ecdh::diffie_hellman(client_secret.to_nonzero_scalar(), server_pub.as_affine());

    let mut salt = [0u8; 32];
    rand::rng().fill_bytes(&mut salt);
    let hk = Hkdf::<Sha256>::new(Some(&salt), shared.raw_secret_bytes());
    let mut key = [0u8; 32];
    hk.expand(HKDF_INFO, &mut key).unwrap();

    let mut aad = Vec::new();
    aad.extend_from_slice(request_id.as_bytes());
    aad.extend_from_slice(&server_pub_bytes);

    let mut iv = [0u8; 12];
    rand::rng().fill_bytes(&mut iv);
    let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&iv),
            Payload {
                msg: values_json,
                aad: &aad,
            },
        )
        .unwrap();

    SealedInput {
        client_pub: B64.encode(&client_pub_point),
        salt: B64.encode(salt),
        iv: B64.encode(iv),
        ciphertext: B64.encode(&ct),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seal_like_browser(
        server_pub_b64: &str,
        request_id: &str,
        values_json: &[u8],
    ) -> SealedInput {
        seal_for_test(server_pub_b64, request_id, values_json)
    }

    #[test]
    fn seal_roundtrip() {
        let server = ServerEphemeral::generate();
        let request_id = "11111111-2222-3333-4444-555555555555";
        let values = br#"{"password":"hunter2","totp":"123456"}"#;
        let sealed = seal_like_browser(&server.public_b64(), request_id, values);
        let out = unseal(&server, &sealed, request_id).unwrap();
        assert_eq!(&out, values);
    }

    #[test]
    fn wrong_request_id_is_rejected_by_aad() {
        let server = ServerEphemeral::generate();
        let sealed = seal_like_browser(&server.public_b64(), "correct-id", br#"{"x":"y"}"#);
        // A ciphertext sealed under one request id must not unseal under another.
        assert!(matches!(
            unseal(&server, &sealed, "different-id"),
            Err(UnsealError::Decrypt)
        ));
    }

    #[test]
    fn wrong_server_key_is_rejected() {
        let server_a = ServerEphemeral::generate();
        let server_b = ServerEphemeral::generate();
        let sealed = seal_like_browser(&server_a.public_b64(), "id", br#"{"x":"y"}"#);
        // Sealed to A; B (a different pending request's key) cannot open it.
        assert!(unseal(&server_b, &sealed, "id").is_err());
    }
}
