//! Whity's signed statements about plugin packages, checked the way
//! whity-core checks them (`src/Core/Plugin/Trust/PluginTrust.php` and
//! `TrustedKeys.php`; docs/design/VERIFIED-PLUGINS-SECURITY.md sections 4
//! and 5).
//!
//! A signed document is an envelope `{key, payload, signature}`, all base64.
//! The Ed25519 signature covers `<format>\n<payload bytes>`, so an attestation
//! cannot pass as a revocation list or the reverse, and the payload's own
//! `format` field must agree. An attestation names one plugin, version and
//! package sha256 and expires; the revocation list carries a rising sequence
//! number, an issue and expiry time, and the entries that revoke a version
//! (or every version) of a plugin, some with `action: kill`.
//!
//! A check passes only when every one of these holds: a key is pinned, the
//! signature is good under a pinned key, plugin, version and sha256 match,
//! the attestation has not expired, the accepted revocation list is fresh,
//! and the version is not revoked. The reasons are core's, word for word.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const ATTESTATION: &str = "whity.attestation/1";
pub const REVOCATIONS: &str = "whity.revocations/1";

/// The public keys whose signatures count. Pinned in code on purpose, as
/// core does: the owner's public key reaches this list only through a
/// reviewed change, and nothing at run time can add to it. EMPTY UNTIL THE
/// OWNER SUPPLIES ONE; never put a placeholder here.
const PINNED: &[&str] = &[];

#[derive(Clone)]
pub struct TrustedKeys {
    keys: Vec<String>,
}

impl TrustedKeys {
    /// The keys this build trusts.
    pub fn pinned() -> Self {
        Self::with_keys(PINNED.iter().map(|k| k.to_string()).collect())
    }

    /// Tests pass their own; production uses [`TrustedKeys::pinned`].
    pub fn with_keys(keys: Vec<String>) -> Self {
        Self { keys }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The verifying key, when this base64 key is one of the pinned ones.
    fn matching(&self, base64_key: &str) -> Option<VerifyingKey> {
        let pinned = self
            .keys
            .iter()
            .find(|k| constant_time_eq(k.as_bytes(), base64_key.as_bytes()))?;
        let raw = BASE64.decode(pinned).ok()?;
        let bytes: [u8; 32] = raw.try_into().ok()?;
        VerifyingKey::from_bytes(&bytes).ok()
    }
}

/// Why a check answered as it did; core's reason codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Verified,
    NoKeys,
    BadSignature,
    Mismatch,
    Expired,
    ListStale,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub ok: bool,
    pub reason: Reason,
}

impl Check {
    fn refused(reason: Reason) -> Self {
        Check { ok: false, reason }
    }
}

/// Why a revocation list was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ListRefusal {
    BadSignature,
    Malformed,
    Replay,
}

/// The accepted revocation list, as core keeps it in `plugin_revocation_state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RevocationState {
    pub sequence: i64,
    pub issued_at: i64,
    pub expires_at: i64,
    /// The envelope as accepted, so the entries can be read back without keys.
    pub body: String,
}

impl RevocationState {
    fn entries(&self) -> Vec<Value> {
        let Ok(outer) = serde_json::from_str::<Value>(&self.body) else {
            return Vec::new();
        };
        let Some(payload) = outer
            .get("payload")
            .and_then(Value::as_str)
            .and_then(|p| BASE64.decode(p).ok())
        else {
            return Vec::new();
        };
        let Ok(list) = serde_json::from_slice::<Value>(&payload) else {
            return Vec::new();
        };
        list.get("revoked")
            .and_then(Value::as_array)
            .map(|entries| entries.iter().filter(|e| e.is_object()).cloned().collect())
            .unwrap_or_default()
    }
}

/// Verification only: holds public keys and the accepted revocation list,
/// and can sign nothing. The list is kept in a file under the app's data
/// directory when one is given, since a device has no database.
pub struct Trust {
    keys: TrustedKeys,
    clock: Box<dyn Fn() -> i64 + Send + Sync>,
    state: Mutex<Option<RevocationState>>,
    state_file: Option<PathBuf>,
}

impl Trust {
    pub fn new(keys: TrustedKeys) -> Self {
        Self {
            keys,
            clock: Box::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            }),
            state: Mutex::new(None),
            state_file: None,
        }
    }

    /// Keep the accepted revocation list in `path`, and load the one there.
    pub fn with_state_file(mut self, path: &Path) -> Self {
        self.state_file = Some(path.to_path_buf());
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(state) = serde_json::from_slice::<StoredState>(&bytes) {
                *self.state.lock().unwrap() = Some(RevocationState {
                    sequence: state.sequence,
                    issued_at: state.issued_at,
                    expires_at: state.expires_at,
                    body: state.body,
                });
            }
        }
        self
    }

    /// Tests move the clock. Seconds since the Unix epoch.
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    pub fn keys(&self) -> &TrustedKeys {
        &self.keys
    }

    /// Check a signed statement that this exact package passed review.
    pub fn check_attestation(
        &self,
        envelope: &str,
        plugin: &str,
        version: &str,
        package_sha256: &str,
    ) -> Check {
        if self.keys.is_empty() {
            return Check::refused(Reason::NoKeys);
        }
        let Some(payload) = self.verified(envelope, ATTESTATION) else {
            return Check::refused(Reason::BadSignature);
        };
        let sha_ok = payload
            .get("sha256")
            .and_then(Value::as_str)
            .is_some_and(|s| constant_time_eq(s.as_bytes(), package_sha256.as_bytes()));
        if payload.get("plugin").and_then(Value::as_str) != Some(plugin)
            || payload.get("version").and_then(Value::as_str) != Some(version)
            || !sha_ok
        {
            return Check::refused(Reason::Mismatch);
        }
        match payload.get("expires_at").and_then(Value::as_i64) {
            Some(expires_at) if expires_at > (self.clock)() => {}
            _ => return Check::refused(Reason::Expired),
        }
        if self.is_stale() {
            return Check::refused(Reason::ListStale);
        }
        if self.revocation_of(plugin, version).is_some() {
            return Check::refused(Reason::Revoked);
        }
        Check {
            ok: true,
            reason: Reason::Verified,
        }
    }

    /// Accept a newer signed revocation list. An older or equal sequence is a
    /// replay and is refused, so a list from before a revocation cannot be
    /// played back.
    pub fn apply_revocation_list(&self, envelope: &str) -> Result<RevocationState, ListRefusal> {
        let Some(list) = self.verified(envelope, REVOCATIONS) else {
            return Err(ListRefusal::BadSignature);
        };
        let (Some(sequence), Some(issued_at), Some(expires_at), Some(_revoked)) = (
            list.get("sequence").and_then(Value::as_i64),
            list.get("issued_at").and_then(Value::as_i64),
            list.get("expires_at").and_then(Value::as_i64),
            list.get("revoked").and_then(Value::as_array),
        ) else {
            return Err(ListRefusal::Malformed);
        };
        let mut state = self.state.lock().unwrap();
        if state
            .as_ref()
            .is_some_and(|current| sequence <= current.sequence)
        {
            return Err(ListRefusal::Replay);
        }
        let accepted = RevocationState {
            sequence,
            issued_at,
            expires_at,
            body: envelope.to_string(),
        };
        if let Some(path) = &self.state_file {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let stored = StoredState {
                sequence,
                issued_at,
                expires_at,
                body: envelope.to_string(),
            };
            if let Ok(json) = serde_json::to_vec_pretty(&stored) {
                let _ = std::fs::write(path, json);
            }
        }
        *state = Some(accepted.clone());
        Ok(accepted)
    }

    /// The accepted list is missing or past its expiry: nothing new installs.
    pub fn is_stale(&self) -> bool {
        let state = self.state.lock().unwrap();
        match state.as_ref() {
            None => true,
            Some(current) => current.expires_at <= (self.clock)(),
        }
    }

    /// A `kill` entry stands against this version.
    pub fn is_killed(&self, plugin: &str, version: &str) -> bool {
        self.entries().iter().any(|e| {
            e.get("action").and_then(Value::as_str) == Some("kill")
                && entry_names(e, plugin, Some(version))
        })
    }

    pub fn revocation_state(&self) -> Option<RevocationState> {
        self.state.lock().unwrap().clone()
    }

    fn revocation_of(&self, plugin: &str, version: &str) -> Option<Value> {
        self.entries()
            .into_iter()
            .find(|e| entry_names(e, plugin, Some(version)))
    }

    fn entries(&self) -> Vec<Value> {
        self.state
            .lock()
            .unwrap()
            .as_ref()
            .map(RevocationState::entries)
            .unwrap_or_default()
    }

    /// The payload of a signed document, or `None` unless a pinned key signed
    /// exactly `<format>\n<payload>` and the payload says it is that format.
    fn verified(&self, envelope: &str, format: &str) -> Option<Value> {
        let outer: Value = serde_json::from_str(envelope).ok()?;
        let key = self.keys.matching(outer.get("key")?.as_str()?)?;
        let payload = BASE64.decode(outer.get("payload")?.as_str()?).ok()?;
        let signature = BASE64.decode(outer.get("signature")?.as_str()?).ok()?;
        let signature = Signature::from_slice(&signature).ok()?;
        let mut message = Vec::with_capacity(format.len() + 1 + payload.len());
        message.extend_from_slice(format.as_bytes());
        message.push(b'\n');
        message.extend_from_slice(&payload);
        key.verify_strict(&message, &signature).ok()?;
        let body: Value = serde_json::from_slice(&payload).ok()?;
        (body.is_object() && body.get("format").and_then(Value::as_str) == Some(format))
            .then_some(body)
    }
}

/// An entry names this plugin's version when its plugin matches and it has no
/// version (every version) or the same version.
fn entry_names(entry: &Value, plugin: &str, version: Option<&str>) -> bool {
    entry.get("plugin").and_then(Value::as_str) == Some(plugin)
        && match entry.get("version") {
            None | Some(Value::Null) => true,
            Some(v) => version.is_some_and(|wanted| v.as_str() == Some(wanted)),
        }
}

#[derive(Serialize, serde::Deserialize)]
struct StoredState {
    sequence: i64,
    issued_at: i64,
    expires_at: i64,
    body: String,
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Signing, for tests and the cross-language vector only. The envelope is
/// what core's test helper produces: payload `{"format": ..., ...body}`,
/// signature over `<format>\n<payload>`.
#[cfg(any(test, feature = "signing"))]
pub mod signing {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};

    pub fn public_key_base64(key: &SigningKey) -> String {
        BASE64.encode(key.verifying_key().to_bytes())
    }

    pub fn sign(format: &str, body: &serde_json::Map<String, Value>, key: &SigningKey) -> String {
        let mut payload = serde_json::Map::new();
        payload.insert("format".into(), Value::String(format.into()));
        for (k, v) in body {
            payload.insert(k.clone(), v.clone());
        }
        let payload = serde_json::to_vec(&Value::Object(payload)).expect("json");
        let mut message = format.as_bytes().to_vec();
        message.push(b'\n');
        message.extend_from_slice(&payload);
        let signature = key.sign(&message);
        serde_json::json!({
            "key": public_key_base64(key),
            "payload": BASE64.encode(&payload),
            "signature": BASE64.encode(signature.to_bytes()),
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::signing::{public_key_base64, sign};
    use super::*;
    use ed25519_dalek::SigningKey;
    use serde_json::json;

    const NOW: i64 = 1_800_000_000;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn body(pairs: Value) -> serde_json::Map<String, Value> {
        pairs.as_object().unwrap().clone()
    }

    fn trust(signer: &SigningKey) -> Trust {
        Trust::new(TrustedKeys::with_keys(vec![public_key_base64(signer)])).with_clock(|| NOW)
    }

    fn attestation(
        signer: &SigningKey,
        plugin: &str,
        version: &str,
        sha: &str,
        expires_at: i64,
    ) -> String {
        sign(
            ATTESTATION,
            &body(
                json!({ "plugin": plugin, "version": version, "sha256": sha, "expires_at": expires_at }),
            ),
            signer,
        )
    }

    fn fresh_list(signer: &SigningKey, sequence: i64, revoked: Value) -> String {
        sign(
            REVOCATIONS,
            &body(
                json!({ "sequence": sequence, "issued_at": NOW - 60, "expires_at": NOW + 7 * 86_400, "revoked": revoked }),
            ),
            signer,
        )
    }

    #[test]
    fn with_no_pinned_key_nothing_verifies() {
        assert!(
            TrustedKeys::pinned().is_empty(),
            "the production list stays empty until the owner adds a key"
        );
        let trust = Trust::new(TrustedKeys::pinned()).with_clock(|| NOW);
        let envelope = attestation(&key(1), "Probe", "1.0.0", "ab", NOW + 100);
        assert_eq!(
            trust
                .check_attestation(&envelope, "Probe", "1.0.0", "ab")
                .reason,
            Reason::NoKeys
        );
    }

    #[test]
    fn a_good_attestation_with_a_fresh_list_is_verified() {
        let signer = key(1);
        let trust = trust(&signer);
        trust
            .apply_revocation_list(&fresh_list(&signer, 1, json!([])))
            .unwrap();
        let envelope = attestation(&signer, "Probe", "1.0.0", "ab", NOW + 100);
        assert_eq!(
            trust.check_attestation(&envelope, "Probe", "1.0.0", "ab"),
            Check {
                ok: true,
                reason: Reason::Verified
            }
        );
    }

    #[test]
    fn every_refusal_has_its_reason() {
        let signer = key(1);
        let trust = trust(&signer);
        let envelope = attestation(&signer, "Probe", "1.0.0", "ab", NOW + 100);

        // No list accepted yet: stale.
        assert_eq!(
            trust
                .check_attestation(&envelope, "Probe", "1.0.0", "ab")
                .reason,
            Reason::ListStale
        );
        trust
            .apply_revocation_list(&fresh_list(
                &signer,
                1,
                json!([{ "plugin": "Bad", "version": "2.0.0", "action": "kill", "reason": "x" }]),
            ))
            .unwrap();

        // Another key, a tampered payload, a tampered signature, a list passed as an attestation.
        let forged = attestation(&key(2), "Probe", "1.0.0", "ab", NOW + 100);
        assert_eq!(
            trust
                .check_attestation(&forged, "Probe", "1.0.0", "ab")
                .reason,
            Reason::BadSignature
        );
        let mut outer: Value = serde_json::from_str(&envelope).unwrap();
        outer["payload"] = Value::String(BASE64.encode(br#"{"format":"whity.attestation/1","plugin":"Probe","version":"1.0.0","sha256":"ab","expires_at":9999999999}"#));
        assert_eq!(
            trust
                .check_attestation(&outer.to_string(), "Probe", "1.0.0", "ab")
                .reason,
            Reason::BadSignature
        );
        let list_as_attestation = fresh_list(&signer, 9, json!([]));
        assert_eq!(
            trust
                .check_attestation(&list_as_attestation, "Probe", "1.0.0", "ab")
                .reason,
            Reason::BadSignature
        );
        assert_eq!(
            trust
                .check_attestation("not json", "Probe", "1.0.0", "ab")
                .reason,
            Reason::BadSignature
        );

        // Wrong plugin, version or hash.
        assert_eq!(
            trust
                .check_attestation(&envelope, "Other", "1.0.0", "ab")
                .reason,
            Reason::Mismatch
        );
        assert_eq!(
            trust
                .check_attestation(&envelope, "Probe", "1.0.1", "ab")
                .reason,
            Reason::Mismatch
        );
        assert_eq!(
            trust
                .check_attestation(&envelope, "Probe", "1.0.0", "cd")
                .reason,
            Reason::Mismatch
        );

        // Expired, and expiry missing.
        let old = attestation(&signer, "Probe", "1.0.0", "ab", NOW);
        assert_eq!(
            trust.check_attestation(&old, "Probe", "1.0.0", "ab").reason,
            Reason::Expired
        );
        let undated = sign(
            ATTESTATION,
            &body(json!({ "plugin": "Probe", "version": "1.0.0", "sha256": "ab" })),
            &signer,
        );
        assert_eq!(
            trust
                .check_attestation(&undated, "Probe", "1.0.0", "ab")
                .reason,
            Reason::Expired
        );

        // Revoked: this version, then every version.
        let bad = attestation(&signer, "Bad", "2.0.0", "ab", NOW + 100);
        assert_eq!(
            trust.check_attestation(&bad, "Bad", "2.0.0", "ab").reason,
            Reason::Revoked
        );
        assert!(trust.is_killed("Bad", "2.0.0"));
        assert!(!trust.is_killed("Bad", "2.0.1"));
        trust
            .apply_revocation_list(&fresh_list(
                &signer,
                2,
                json!([{ "plugin": "Bad", "reason": "all of it" }]),
            ))
            .unwrap();
        let bad_other = attestation(&signer, "Bad", "3.0.0", "ab", NOW + 100);
        assert_eq!(
            trust
                .check_attestation(&bad_other, "Bad", "3.0.0", "ab")
                .reason,
            Reason::Revoked
        );

        // The good one still passes.
        assert!(
            trust
                .check_attestation(&envelope, "Probe", "1.0.0", "ab")
                .ok
        );
    }

    #[test]
    fn a_revocation_list_is_refused_when_forged_malformed_or_replayed() {
        let signer = key(1);
        let trust = trust(&signer);
        assert_eq!(
            trust.apply_revocation_list(&fresh_list(&key(2), 1, json!([]))),
            Err(ListRefusal::BadSignature)
        );
        let malformed = sign(
            REVOCATIONS,
            &body(json!({ "sequence": "one", "revoked": [] })),
            &signer,
        );
        assert_eq!(
            trust.apply_revocation_list(&malformed),
            Err(ListRefusal::Malformed)
        );
        trust
            .apply_revocation_list(&fresh_list(&signer, 5, json!([])))
            .unwrap();
        assert_eq!(
            trust.apply_revocation_list(&fresh_list(&signer, 5, json!([]))),
            Err(ListRefusal::Replay)
        );
        assert_eq!(
            trust.apply_revocation_list(&fresh_list(&signer, 4, json!([]))),
            Err(ListRefusal::Replay)
        );
        assert!(trust
            .apply_revocation_list(&fresh_list(&signer, 6, json!([])))
            .is_ok());
    }

    #[test]
    fn an_expired_list_makes_everything_stale_and_the_state_survives_a_restart() {
        let signer = key(1);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("revocations.json");
        let trust = Trust::new(TrustedKeys::with_keys(vec![public_key_base64(&signer)]))
            .with_clock(|| NOW)
            .with_state_file(&file);
        let short = sign(
            REVOCATIONS,
            &body(
                json!({ "sequence": 1, "issued_at": NOW - 60, "expires_at": NOW + 10, "revoked": [] }),
            ),
            &signer,
        );
        trust.apply_revocation_list(&short).unwrap();
        assert!(!trust.is_stale());

        let later = Trust::new(TrustedKeys::with_keys(vec![public_key_base64(&signer)]))
            .with_clock(|| NOW + 11)
            .with_state_file(&file);
        assert_eq!(later.revocation_state().map(|s| s.sequence), Some(1));
        assert!(later.is_stale());
        let envelope = attestation(&signer, "Probe", "1.0.0", "ab", NOW + 100);
        assert_eq!(
            later
                .check_attestation(&envelope, "Probe", "1.0.0", "ab")
                .reason,
            Reason::ListStale
        );
    }

    /// The vector in tests/fixtures/attestation-vector.json: signed once, by
    /// this crate's test signer with a fixed seed, and verified here and by
    /// whity-core's PHP. Regenerate with
    /// `cargo test -p whity-plugins -- --ignored write_the_cross_language_vector`.
    #[test]
    fn the_cross_language_vector_verifies() {
        let vector: Value =
            serde_json::from_str(include_str!("../tests/fixtures/attestation-vector.json"))
                .unwrap();
        let keys = TrustedKeys::with_keys(vec![vector["publicKey"].as_str().unwrap().to_string()]);
        let now = vector["now"].as_i64().unwrap();
        let trust = Trust::new(keys).with_clock(move || now);
        trust
            .apply_revocation_list(vector["revocations"].as_str().unwrap())
            .unwrap();
        let expect = &vector["expect"];
        let check = trust.check_attestation(
            vector["attestation"].as_str().unwrap(),
            expect["plugin"].as_str().unwrap(),
            expect["version"].as_str().unwrap(),
            expect["sha256"].as_str().unwrap(),
        );
        assert_eq!(
            check,
            Check {
                ok: true,
                reason: Reason::Verified
            }
        );
        let revoked = trust.check_attestation(
            vector["revokedAttestation"].as_str().unwrap(),
            "RevokedPlugin",
            "1.0.0",
            expect["sha256"].as_str().unwrap(),
        );
        assert_eq!(revoked.reason, Reason::Revoked);
    }

    #[test]
    #[ignore]
    fn write_the_cross_language_vector() {
        let signer = key(7);
        let now: i64 = 1_800_000_000;
        let sha = "6ce0c17c6a6e0a5c3a8f15ee1e9f0ef3f1a9c1c0f1a7b4b4c6d8e0f2a4b6c8d0";
        let attestation = attestation(&signer, "ExamplePlugin", "1.2.3", sha, now + 365 * 86_400);
        let revoked_attestation =
            attestation_for(&signer, "RevokedPlugin", "1.0.0", sha, now + 365 * 86_400);
        let revocations = sign(
            REVOCATIONS,
            &body(json!({
                "sequence": 1,
                "issued_at": now - 3_600,
                "expires_at": now + 30 * 86_400,
                "revoked": [{ "plugin": "RevokedPlugin", "version": "1.0.0", "action": "kill", "reason": "test vector" }]
            })),
            &signer,
        );
        let vector = json!({
            "$comment": "Signed once by whity-plugins' test signer (seed 7, Ed25519). Verified by crates/plugins/src/trust.rs and by whity-core's PluginTrust in PHP. now is the clock both sides use.",
            "publicKey": public_key_base64(&signer),
            "now": now,
            "attestation": attestation,
            "revokedAttestation": revoked_attestation,
            "revocations": revocations,
            "expect": { "plugin": "ExamplePlugin", "version": "1.2.3", "sha256": sha, "reason": "verified" },
            "expectRevoked": { "plugin": "RevokedPlugin", "version": "1.0.0", "reason": "revoked" }
        });
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/attestation-vector.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&vector).unwrap() + "\n").unwrap();
    }

    fn attestation_for(
        signer: &SigningKey,
        plugin: &str,
        version: &str,
        sha: &str,
        expires_at: i64,
    ) -> String {
        attestation(signer, plugin, version, sha, expires_at)
    }
}
