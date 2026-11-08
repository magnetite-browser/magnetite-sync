//! Magnetite sync protocol v2 ("sync chain"), shared by the client and
//! `magnetite-sync` (the server). Design notes: `docs/design.md` in the
//! repository.
//!
//! Zero-knowledge model:
//!
//! ```text
//! seed        = 32 random bytes, shown to the user as 24 words (the sync code)
//! signing key = Ed25519 from HKDF-SHA256(seed, "magnetite-sync/v2/sign")
//! account     = hex(public key)                       (all the server knows)
//! enc key     = HKDF-SHA256(seed, "magnetite-sync/v2/enc")  (XChaCha20-Poly1305)
//! collection  = hex(HKDF-SHA256(seed, "magnetite-sync/v2/collection/" + name))[..32]
//! ```
//!
//! The server stores a public key, opaque collection ids, versions and padded
//! ciphertexts. It never sees an email, a password, device names or content,
//! and nothing it stores helps to guess the seed (256 random bits, unlike a
//! password). Every request is signed over method, the server's host, path,
//! time and body, so a captured request works on no other server.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD as B64URL};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const API_VERSION: &str = "v2";
pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
/// Signed requests are accepted this far from the server's clock.
pub const MAX_CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;
/// Authorization scheme name.
pub const AUTH_SCHEME: &str = "Magnetite-Sig";
/// Header that carries the body's SHA-256 ahead of the body (RFC 9530), so
/// the server can check a signature before it reads what is uploaded.
/// Collection writes must send it; [`content_digest`] makes its value.
pub const CONTENT_DIGEST: &str = "content-digest";
/// Smallest padded payload; payloads grow in powers of two above it.
const MIN_PADDED: usize = 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CryptoError {
    #[error("this is not a valid sync code (24 words expected)")]
    BadCode,
    #[error("decryption failed (wrong sync code or corrupted data)")]
    Decrypt,
    #[error("encryption failed")]
    Encrypt,
    #[error("malformed base64")]
    Base64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("missing or malformed authorization")]
    Malformed,
    #[error("request time is too far from the server clock")]
    Expired,
    #[error("bad signature")]
    Signature,
}

/// The secret behind a sync chain. Shown to the user once as 24 words; every
/// device of the chain holds it.
#[derive(Clone, PartialEq, Eq)]
pub struct SyncCode([u8; KEY_LEN]);

impl std::fmt::Debug for SyncCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SyncCode(redacted)")
    }
}

impl SyncCode {
    pub fn generate() -> Self {
        let mut seed = [0u8; KEY_LEN];
        rand::fill(&mut seed);
        Self(seed)
    }

    /// Parse the 24 words; case, extra spaces and line breaks do not matter.
    pub fn from_words(words: &str) -> Result<Self, CryptoError> {
        let normalised = words
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
            .join(" ");
        let mnemonic = bip39::Mnemonic::parse_in_normalized(bip39::Language::English, &normalised)
            .map_err(|_| CryptoError::BadCode)?;
        let entropy = mnemonic.to_entropy();
        Ok(Self(entropy.try_into().map_err(|_| CryptoError::BadCode)?))
    }

    pub fn words(&self) -> String {
        bip39::Mnemonic::from_entropy_in(bip39::Language::English, &self.0)
            .expect("32 bytes are valid BIP39 entropy")
            .to_string()
    }

    /// Raw seed, for storing on this device only.
    pub fn to_base64(&self) -> String {
        B64.encode(self.0)
    }

    pub fn from_base64(s: &str) -> Result<Self, CryptoError> {
        let bytes = B64.decode(s).map_err(|_| CryptoError::Base64)?;
        Ok(Self(bytes.try_into().map_err(|_| CryptoError::BadCode)?))
    }

    pub fn keys(&self) -> ChainKeys {
        let hk = Hkdf::<Sha256>::new(Some(b"magnetite-sync/v2"), &self.0);
        let mut sign = [0u8; KEY_LEN];
        let mut enc = [0u8; KEY_LEN];
        hk.expand(b"magnetite-sync/v2/sign", &mut sign)
            .expect("32 bytes is a valid HKDF length");
        hk.expand(b"magnetite-sync/v2/enc", &mut enc)
            .expect("32 bytes is a valid HKDF length");
        ChainKeys {
            signing: SigningKey::from_bytes(&sign),
            enc_key: enc,
            hkdf: hk,
        }
    }
}

/// Everything a device derives from the sync code.
pub struct ChainKeys {
    signing: SigningKey,
    enc_key: [u8; KEY_LEN],
    hkdf: Hkdf<Sha256>,
}

impl ChainKeys {
    /// The chain's id on the server: the hex public key.
    pub fn account_id(&self) -> String {
        hex(self.signing.verifying_key().as_bytes())
    }

    /// Opaque, stable id of a collection ("sidebar", "settings", …): the
    /// server cannot tell what a collection holds from its name.
    pub fn collection_id(&self, name: &str) -> String {
        let mut out = [0u8; 16];
        self.hkdf
            .expand(
                format!("magnetite-sync/v2/collection/{name}").as_bytes(),
                &mut out,
            )
            .expect("16 bytes is a valid HKDF length");
        hex(&out)
    }

    /// `Authorization` header value for a request to the server at `host`
    /// (the URL's host and port, see [`audience`]). `path` is the path below
    /// `/v2` (e.g. `/collections/ab12…`), so reverse proxies may mount the
    /// server under any prefix.
    pub fn authorization(
        &self,
        method: &str,
        host: &str,
        path: &str,
        body: &[u8],
        now_ms: i64,
    ) -> String {
        let msg = signed_message(method, &audience(host), path, now_ms, &BodyDigest::of(body));
        let sig = self.signing.sign(msg.as_bytes());
        format!(
            "{AUTH_SCHEME} {}.{now_ms}.{}",
            self.account_id(),
            B64URL.encode(sig.to_bytes())
        )
    }

    /// Encrypt `plaintext` for collection `id` at `version` (both bound as
    /// associated data). The plaintext is padded to a power-of-two size so the
    /// server cannot follow small changes such as one more tab.
    pub fn seal(&self, id: &str, version: u64, plaintext: &[u8]) -> Result<Sealed, CryptoError> {
        let cipher = XChaCha20Poly1305::new((&self.enc_key).into());
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce);
        let msg = pad(plaintext);
        let ciphertext = cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &msg,
                    aad: &aad(id, version),
                },
            )
            .map_err(|_| CryptoError::Encrypt)?;
        Ok(Sealed {
            nonce: B64.encode(nonce),
            ciphertext: B64.encode(ciphertext),
        })
    }

    pub fn open(&self, id: &str, version: u64, sealed: &Sealed) -> Result<Vec<u8>, CryptoError> {
        let cipher = XChaCha20Poly1305::new((&self.enc_key).into());
        let nonce: [u8; NONCE_LEN] = B64
            .decode(&sealed.nonce)
            .map_err(|_| CryptoError::Base64)?
            .try_into()
            .map_err(|_| CryptoError::Decrypt)?;
        let ct = B64
            .decode(&sealed.ciphertext)
            .map_err(|_| CryptoError::Base64)?;
        let padded = cipher
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &ct,
                    aad: &aad(id, version),
                },
            )
            .map_err(|_| CryptoError::Decrypt)?;
        unpad(&padded).ok_or(CryptoError::Decrypt)
    }
}

/// The `Content-Digest` value for `body`: `sha-256=:<base64>:`, the hash the
/// signature already covers.
pub fn content_digest(body: &[u8]) -> String {
    format!("sha-256=:{}:", B64.encode(BodyDigest::of(body).0))
}

/// A body's SHA-256, as the signature covers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyDigest([u8; 32]);

impl BodyDigest {
    pub fn of(body: &[u8]) -> Self {
        Self(Sha256::digest(body).into())
    }

    /// A `Content-Digest` value as [`content_digest`] writes it: one
    /// `sha-256` entry, nothing else.
    pub fn parse(value: &str) -> Option<Self> {
        let b64 = value.strip_prefix("sha-256=:")?.strip_suffix(':')?;
        B64.decode(b64).ok()?.try_into().ok().map(Self)
    }

    /// Whether `body` is what this digest names.
    pub fn matches(&self, body: &[u8]) -> bool {
        *self == Self::of(body)
    }
}

/// Server side: an `Authorization` header that is well formed and recent, its
/// signature not checked yet. Parsing needs no body. A collection write is
/// checked with [`SignedHeader::verify_digest`] against its `Content-Digest`
/// before its body is read; other requests with `verify` once it is in.
pub struct SignedHeader {
    key: VerifyingKey,
    ts: i64,
    sig: Signature,
}

impl SignedHeader {
    pub fn parse(header: &str, now_ms: i64) -> Result<Self, AuthError> {
        let rest = header
            .strip_prefix(AUTH_SCHEME)
            .and_then(|r| r.strip_prefix(' '))
            .ok_or(AuthError::Malformed)?;
        let mut parts = rest.splitn(3, '.');
        let (Some(key_hex), Some(ts), Some(sig_b64)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(AuthError::Malformed);
        };
        let ts: i64 = ts.parse().map_err(|_| AuthError::Malformed)?;
        if now_ms.abs_diff(ts) > MAX_CLOCK_SKEW_MS.unsigned_abs() {
            return Err(AuthError::Expired);
        }
        let key: [u8; KEY_LEN] = unhex(key_hex)
            .and_then(|k| k.try_into().ok())
            .ok_or(AuthError::Malformed)?;
        let key = VerifyingKey::from_bytes(&key).map_err(|_| AuthError::Malformed)?;
        // Small-order keys verify signatures that no secret key made.
        if key.is_weak() {
            return Err(AuthError::Malformed);
        }
        let sig: [u8; 64] = B64URL
            .decode(sig_b64)
            .ok()
            .and_then(|s| s.try_into().ok())
            .ok_or(AuthError::Malformed)?;
        Ok(Self {
            key,
            ts,
            sig: Signature::from_bytes(&sig),
        })
    }

    /// The chain that claims to have signed (hex public key, lowercase).
    pub fn account_id(&self) -> String {
        hex(self.key.as_bytes())
    }

    /// Check the signature over method, the host the request was sent to (its
    /// `Host` header), path (below `/v2`), time and body. Strict: no
    /// non-canonical signatures.
    pub fn verify(
        &self,
        method: &str,
        host: &str,
        path: &str,
        body: &[u8],
    ) -> Result<(), AuthError> {
        self.verify_digest(method, host, path, &BodyDigest::of(body))
    }

    /// `verify` for a body known only by its digest (`Content-Digest`): the
    /// body must then be checked against it ([`BodyDigest::matches`]).
    pub fn verify_digest(
        &self,
        method: &str,
        host: &str,
        path: &str,
        digest: &BodyDigest,
    ) -> Result<(), AuthError> {
        self.key
            .verify_strict(
                signed_message(method, &audience(host), path, self.ts, digest).as_bytes(),
                &self.sig,
            )
            .map_err(|_| AuthError::Signature)
    }
}

/// The server a request is meant for, as both sides spell it: the host and
/// port of the URL (or of the `Host` header), lowercase, without the default
/// ports 443 and 80.
pub fn audience(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    match host
        .strip_suffix(":443")
        .or_else(|| host.strip_suffix(":80"))
    {
        Some(bare) => bare.to_string(),
        None => host,
    }
}

fn signed_message(
    method: &str,
    audience: &str,
    path: &str,
    ts: i64,
    digest: &BodyDigest,
) -> String {
    format!(
        "magnetite-sync/{API_VERSION}\n{}\n{audience}\n{path}\n{ts}\n{}",
        method.to_ascii_uppercase(),
        hex(&digest.0)
    )
}

fn aad(id: &str, version: u64) -> Vec<u8> {
    format!("{API_VERSION}:{id}:{version}").into_bytes()
}

/// `len (u32 LE) | data | zeros` up to the next power of two (min 1 KiB).
fn pad(data: &[u8]) -> Vec<u8> {
    let len = u32::try_from(data.len()).expect("collections are far below 4 GiB");
    let size = (data.len() + 4).next_power_of_two().max(MIN_PADDED);
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(data);
    out.resize(size, 0);
    out
}

fn unpad(padded: &[u8]) -> Option<Vec<u8>> {
    let len = u32::from_le_bytes(padded.get(..4)?.try_into().ok()?) as usize;
    padded.get(4..4 + len).map(<[u8]>::to_vec)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Is `s` a collection id as produced by `ChainKeys::collection_id`?
pub fn is_collection_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ---- API types -------------------------------------------------------------

/// An encrypted collection payload as it travels over the API.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Sealed {
    /// Base64 `XChaCha20` nonce (24 bytes).
    pub nonce: String,
    /// Base64 ciphertext with Poly1305 tag.
    pub ciphertext: String,
}

impl Sealed {
    /// Server side: does this look like something `ChainKeys::seal` made (a
    /// 24-byte nonce, a base64 ciphertext of at most `max_ciphertext`
    /// characters)? The server cannot decrypt, but stores nothing else.
    pub fn is_well_formed(&self, max_ciphertext: usize) -> bool {
        let base64 = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=');
        self.nonce.len() == NONCE_LEN.div_ceil(3) * 4
            && B64.decode(&self.nonce).is_ok()
            && !self.ciphertext.is_empty()
            && self.ciphertext.len() <= max_ciphertext
            && self.ciphertext.len() % 4 == 0
            && self.ciphertext.bytes().all(base64)
    }
}

// The request types are parsed by the server from untrusted bodies: unknown
// fields are refused, and no type uses `#[serde(flatten)]`, which would keep
// every unknown field in memory while parsing (a padded body could cost many
// times its size).

/// `POST /v2/chain`: create the chain for the signing key.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateChainRequest {
    /// Required when the server runs with an invite code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<String>,
}

/// `GET /v2/collections/{id}`: the stored version and its ciphertext.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollectionResponse {
    pub version: u64,
    /// Base64 `XChaCha20` nonce (24 bytes).
    pub nonce: String,
    /// Base64 ciphertext with Poly1305 tag.
    pub ciphertext: String,
}

impl CollectionResponse {
    pub fn new(version: u64, sealed: Sealed) -> Self {
        Self {
            version,
            nonce: sealed.nonce,
            ciphertext: sealed.ciphertext,
        }
    }

    pub fn sealed(&self) -> Sealed {
        Sealed {
            nonce: self.nonce.clone(),
            ciphertext: self.ciphertext.clone(),
        }
    }
}

/// `PUT /v2/collections/{id}`: write the version after `base_version`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutCollectionRequest {
    /// Version the client based this write on (0 for a new collection).
    pub base_version: u64,
    /// Base64 `XChaCha20` nonce (24 bytes).
    pub nonce: String,
    /// Base64 ciphertext with Poly1305 tag.
    pub ciphertext: String,
}

impl PutCollectionRequest {
    pub fn new(base_version: u64, sealed: Sealed) -> Self {
        Self {
            base_version,
            nonce: sealed.nonce,
            ciphertext: sealed.ciphertext,
        }
    }

    pub fn sealed(&self) -> Sealed {
        Sealed {
            nonce: self.nonce.clone(),
            ciphertext: self.ciphertext.clone(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PutCollectionResponse {
    pub version: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_version: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_roundtrips_through_words_and_tolerates_formatting() {
        let code = SyncCode::generate();
        let words = code.words();
        assert_eq!(words.split(' ').count(), 24);
        let messy = format!("  {}\n", words.to_uppercase().replace(' ', "   "));
        assert_eq!(SyncCode::from_words(&messy).unwrap(), code);
        assert_eq!(
            SyncCode::from_words("not a sync code"),
            Err(CryptoError::BadCode)
        );
        // Valid words with a wrong checksum: all-zero entropy ends in "art".
        let zero = SyncCode([0; KEY_LEN]);
        assert!(zero.words().ends_with(" abandon art"));
        assert_eq!(
            SyncCode::from_words(&["abandon"; 24].join(" ")),
            Err(CryptoError::BadCode)
        );
    }

    #[test]
    fn keys_are_deterministic_and_collection_ids_opaque() {
        let code = SyncCode::generate();
        let (a, b) = (code.keys(), code.keys());
        assert_eq!(a.account_id(), b.account_id());
        assert_eq!(a.account_id().len(), 64);
        let id = a.collection_id("sidebar");
        assert!(is_collection_id(&id) && !id.contains("sidebar"));
        assert_ne!(id, a.collection_id("settings"));
        assert_ne!(id, SyncCode::generate().keys().collection_id("sidebar"));
    }

    #[test]
    fn content_digests_carry_the_signed_hash_ahead_of_the_body() {
        // RFC 9530's form, with the SHA-256 the signature covers.
        assert_eq!(
            content_digest(b""),
            "sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:"
        );
        let keys = SyncCode::generate().keys();
        let now = 1_800_000_000_000;
        let h = keys.authorization("PUT", "sync.example.org", "/collections/x", b"body", now);
        let parsed = SignedHeader::parse(&h, now).unwrap();
        let digest = BodyDigest::parse(&content_digest(b"body")).unwrap();
        // Checked against the digest alone, before the body is read ...
        assert_eq!(
            parsed.verify_digest("PUT", "sync.example.org", "/collections/x", &digest),
            Ok(())
        );
        // ... and the body against the digest once it is.
        assert!(digest.matches(b"body") && !digest.matches(b"other"));
        let other = BodyDigest::parse(&content_digest(b"other")).unwrap();
        assert_eq!(
            parsed.verify_digest("PUT", "sync.example.org", "/collections/x", &other),
            Err(AuthError::Signature)
        );
        // Only the form above.
        for bad in [
            "",
            "sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=",
            "sha-512=:AAAA:",
            "sha-256=:AAAA:",
            "sha-256=:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=:, sha-512=:AA==:",
        ] {
            assert_eq!(BodyDigest::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn signatures_bind_method_server_path_body_and_time() {
        let keys = SyncCode::generate().keys();
        let now = 1_800_000_000_000;
        let h = keys.authorization("PUT", "sync.example.org", "/collections/x", b"body", now);
        let on = |host: &str, method: &str, path: &str, body: &[u8], at: i64| {
            let parsed = SignedHeader::parse(&h, at)?;
            parsed.verify(method, host, path, body)?;
            Ok(parsed.account_id())
        };
        let check = |method: &str, path: &str, body: &[u8], at: i64| {
            on("sync.example.org", method, path, body, at)
        };
        assert_eq!(
            check("PUT", "/collections/x", b"body", now + 1000),
            Ok(keys.account_id())
        );
        // The Host header may carry the default port or other case.
        assert_eq!(
            on(
                "Sync.Example.org:443",
                "PUT",
                "/collections/x",
                b"body",
                now
            ),
            Ok(keys.account_id())
        );
        // Relayed to another server: refused.
        assert_eq!(
            on("sync.other.org", "PUT", "/collections/x", b"body", now),
            Err(AuthError::Signature)
        );
        assert_eq!(
            check("GET", "/collections/x", b"body", now),
            Err(AuthError::Signature)
        );
        assert_eq!(
            check("PUT", "/collections/y", b"body", now),
            Err(AuthError::Signature)
        );
        assert_eq!(
            check("PUT", "/collections/x", b"other", now),
            Err(AuthError::Signature)
        );
        assert_eq!(
            check(
                "PUT",
                "/collections/x",
                b"body",
                now + MAX_CLOCK_SKEW_MS + 1
            ),
            Err(AuthError::Expired)
        );
        assert!(matches!(
            SignedHeader::parse("Bearer abc", now),
            Err(AuthError::Malformed)
        ));
        // A small-order public key (here the identity point) would accept a
        // forged signature for any message: refused when parsed.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let weak = format!(
            "{AUTH_SCHEME} {}.{now}.{}",
            hex(&identity),
            B64URL.encode([0u8; 64])
        );
        assert!(matches!(
            SignedHeader::parse(&weak, now),
            Err(AuthError::Malformed)
        ));
        // Extreme timestamps are refused, not an arithmetic overflow.
        let far = format!("{AUTH_SCHEME} {}.{}.x", keys.account_id(), i64::MIN);
        assert!(matches!(
            SignedHeader::parse(&far, now),
            Err(AuthError::Expired)
        ));
    }

    #[test]
    fn request_bodies_with_unknown_fields_are_refused() {
        let body = r#"{"base_version":0,"nonce":"n","ciphertext":"c","pad":[0,0,0]}"#;
        assert!(serde_json::from_str::<PutCollectionRequest>(body).is_err());
        let ok = r#"{"base_version":0,"nonce":"n","ciphertext":"c"}"#;
        assert!(serde_json::from_str::<PutCollectionRequest>(ok).is_ok());
        assert!(serde_json::from_str::<CreateChainRequest>(r#"{"pad":1}"#).is_err());
    }

    #[test]
    fn seal_roundtrip_binding_and_padding() {
        let keys = SyncCode::generate().keys();
        let id = keys.collection_id("sidebar");
        let sealed = keys.seal(&id, 3, b"hello").unwrap();
        assert_eq!(keys.open(&id, 3, &sealed).unwrap(), b"hello");
        assert!(keys.open(&id, 4, &sealed).is_err(), "version is bound");
        assert!(
            keys.open(&keys.collection_id("settings"), 3, &sealed)
                .is_err(),
            "id is bound"
        );
        assert!(
            SyncCode::generate().keys().open(&id, 3, &sealed).is_err(),
            "other chain"
        );
        // Small and slightly larger payloads look the same size.
        let a = keys.seal(&id, 1, &[1u8; 10]).unwrap();
        let b = keys.seal(&id, 1, &[1u8; 900]).unwrap();
        assert_eq!(a.ciphertext.len(), b.ciphertext.len());
    }

    #[test]
    fn well_formed_sealed_payloads() {
        let sealed = SyncCode::generate().keys().seal("id", 1, b"x").unwrap();
        assert!(sealed.is_well_formed(sealed.ciphertext.len()));
        assert!(
            !sealed.is_well_formed(sealed.ciphertext.len() - 4),
            "too big"
        );
        let with = |nonce: &str, ciphertext: &str| Sealed {
            nonce: nonce.into(),
            ciphertext: ciphertext.into(),
        };
        let long_nonce = format!("{}AAAA", sealed.nonce);
        assert!(!with(&long_nonce, &sealed.ciphertext).is_well_formed(usize::MAX));
        assert!(!with(&sealed.nonce, "not base64!").is_well_formed(usize::MAX));
        assert!(!with(&sealed.nonce, "").is_well_formed(usize::MAX));
    }
}
