//! Soul hash verification.
//!
//! Reads soul.invariant from WardSONDB, computes SHA-256 of the canonical
//! JSON representation, and compares against the stored hash on STATE.

use anyhow::{Result, Context};
use sha2::{Sha256, Digest};
use tracing::debug;

/// What `verify` reports when WardSONDB holds no soul document and no seal
/// hash is stored.
///
/// embrad reads this TEXT to tell a first boot from a failed verification
/// (`embrad/src/supervisor.rs`, `is_first_run`): it compares the report with
/// this sentence exactly (`NO_SOUL_REPORT` there). Reword it there as well,
/// or the first boot halts. Every other report of this module is some other
/// sentence, so no failed verification can read as a first boot.
const NO_SOUL_ERROR: &str = "Soul document not found — no soul exists (first run or data loss)";

/// The report for a soul document that is gone while a seal hash file is
/// present, whatever it holds: tamper or data loss, never a first run. It is
/// not the first-run sentence, so embrad halts on it. Guards
/// `a_missing_soul_with_a_stored_hash_halts_and_without_one_is_a_first_run`,
/// `a_missing_soul_with_an_empty_or_unreadable_hash_file_halts`.
const SEALED_HASH_WITHOUT_SOUL_ERROR: &str = "Soul document missing but a sealed hash is stored";

/// The report for a soul document that carries no `soul` field: the seal
/// cannot be checked, and nothing is hashed in its place (#13). Not a
/// first run.
const SOUL_FIELD_MISSING_ERROR: &str =
    "The stored soul document lacks its soul field; the seal cannot be checked";

pub struct SoulVerifier {
    wardsondb_url: String,
    hash_path: std::path::PathBuf,
}

impl SoulVerifier {
    pub fn new(wardsondb_url: String, hash_path: std::path::PathBuf) -> Self {
        Self { wardsondb_url, hash_path }
    }

    /// Verify the soul.
    /// Returns (valid, computed_hash, stored_hash, error_message).
    pub async fn verify(&self) -> (bool, String, String, String) {
        match self.verify_inner().await {
            Ok((computed, stored)) => {
                if computed == stored {
                    (true, computed, stored, String::new())
                } else {
                    (false, computed, stored, "Hash mismatch".to_string())
                }
            }
            Err(e) => {
                let msg = format!("{}", e);
                (false, String::new(), String::new(), msg)
            }
        }
    }

    async fn verify_inner(&self) -> Result<(String, String)> {
        // Read soul from WardSONDB
        let soul_json = match self.read_soul_from_db().await? {
            Some(doc) => doc,
            None => {
                // A missing document is a first run only when no hash file
                // is there at all. Presence is the question, not content: an
                // empty or unreadable hash file beside a missing document is
                // tamper or data loss as much as a full one (#9), and a file
                // whose presence cannot be told halts too. Every one of these
                // reports is some sentence other than NO_SOUL_ERROR.
                match self.stored_hash_is_present() {
                    Ok(false) => anyhow::bail!(NO_SOUL_ERROR),
                    Ok(true) => anyhow::bail!(
                        "{SEALED_HASH_WITHOUT_SOUL_ERROR} at {} — restore DATA, or remove the hash to start over",
                        self.hash_path.display()
                    ),
                    Err(e) => anyhow::bail!(
                        "Soul document missing and the seal hash at {} could not be checked ({e}) — fix STATE, or remove the hash to start over",
                        self.hash_path.display()
                    ),
                }
            }
        };

        // Compute SHA-256
        let computed_hash = self.compute_hash(&soul_json)?;
        debug!("Computed soul hash: {}", computed_hash);

        // Read stored hash from STATE
        let stored_hash = self.read_stored_hash()?;
        debug!("Stored soul hash: {}", stored_hash);

        Ok((computed_hash, stored_hash))
    }

    /// The soul document, or `None` when the database has no such document.
    async fn read_soul_from_db(&self) -> Result<Option<serde_json::Value>> {
        let client = reqwest::Client::new();
        let url = format!("{}/soul.invariant/docs/soul", self.wardsondb_url);

        let response = client.get(&url).send().await
            .context("Failed to connect to WardSONDB")?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let envelope: serde_json::Value = response.json().await
            .context("Failed to parse WardSONDB response")?;

        if !envelope["ok"].as_bool().unwrap_or(false) {
            let err = envelope["error"]["message"].as_str().unwrap_or("unknown error");
            anyhow::bail!("WardSONDB error: {}", err);
        }

        let doc = envelope["data"].clone();
        if doc.is_null() {
            anyhow::bail!("Soul document is null");
        }

        Ok(Some(doc))
    }

    /// The hash of the soul as sealed: the `soul` field of the document
    /// (not the metadata around it), serialized the way the brain's
    /// `seal_soul` serializes it. A document without the field, or one that
    /// does not serialize, is a failed verification, never a hash over a
    /// stand-in such as `null` or the empty string (#13).
    fn compute_hash(&self, soul_json: &serde_json::Value) -> Result<String> {
        let soul_content = soul_json
            .get("soul")
            .ok_or_else(|| anyhow::anyhow!("{SOUL_FIELD_MISSING_ERROR}"))?;

        // Use to_string_pretty to match embra-brain's seal_soul() serialization
        let canonical = serde_json::to_string_pretty(soul_content)
            .context("The stored soul document does not serialize; the seal cannot be checked")?;

        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        Ok(format!("{:x}", hasher.finalize()))
    }

    /// Whether a seal hash file is present: `Ok(true)` for a file of any
    /// content, `Ok(false)` only when nothing is there, and the error when
    /// that cannot be told. The 404 path asks this and never reads the file.
    fn stored_hash_is_present(&self) -> std::io::Result<bool> {
        match std::fs::metadata(&self.hash_path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn read_stored_hash(&self) -> Result<String> {
        if !self.hash_path.exists() {
            // A soul document exists (the 404 path checks presence with
            // `stored_hash_is_present` and never reads the file), so a
            // missing hash is a lost or tampered STATE, and embrad halts on
            // this report (#14: the recovery is named).
            anyhow::bail!(
                "No stored soul hash at {} while a soul document exists — restore STATE, or remove the soul with seed-state.sh --wipe data to start over",
                self.hash_path.display()
            );
        }

        let hash = std::fs::read_to_string(&self.hash_path)
            .context("Failed to read stored soul hash")?
            .trim()
            .to_string();

        if hash.is_empty() {
            anyhow::bail!("Stored soul hash is empty");
        }

        Ok(hash)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn verifier(hash_path: &str) -> SoulVerifier {
        SoulVerifier::new("http://127.0.0.1:1".to_string(), hash_path.into())
    }

    /// The same value and the same hash stand in embra-brain's tests
    /// (`embra-brain/src/learning/soul.rs`, `parity_tests`). The brain seals
    /// with its hash and this service computes it again at every boot: the
    /// two have to serialize a value the same way, byte for byte.
    const PARITY_SOUL: &str = r#"{"name":"Parity","format":"graph.v1","nodes":[{"id":"self","type":"self","text":"Ünïcode — “quoted”\nsecond line","weight":1},{"id":"a","type":"value","text":"t","tags":[],"meta":{}}],"edges":[{"src":"self","dst":"a","relation":"holds"}],"n":42,"neg":-7,"flag":true,"nothing":null}"#;
    const PARITY_HASH: &str = "2e825f06286345aa0e369f9107d34b5d9e77ddd4bdd693408f7a8a997a170f51";

    #[test]
    fn the_hash_is_the_one_the_brain_seals_with() {
        let soul: serde_json::Value = serde_json::from_str(PARITY_SOUL).unwrap();
        // As stored: the soul inside its document. Only the soul is hashed.
        let doc = serde_json::json!({
            "_id": "soul",
            "soul": soul,
            "sha256": "not read here",
            "sealed_at": "2026-01-01T00:00:00Z",
            "sealed": true,
        });
        assert_eq!(verifier("/nonexistent").compute_hash(&doc).unwrap(), PARITY_HASH);
    }

    /// A document without its `soul` field is a failed verification, not a
    /// hash over `null` (#13), in words that are not a first run.
    #[test]
    fn a_document_without_its_soul_field_does_not_hash() {
        let doc = serde_json::json!({ "_id": "soul", "sha256": "x" });
        let err = verifier("/nonexistent").compute_hash(&doc).unwrap_err().to_string();
        assert_eq!(err, SOUL_FIELD_MISSING_ERROR);
        assert!(!err.contains("no soul") && !err.contains("not found"), "{err}");
    }

    /// embrad compares the first-run report with this sentence, exactly
    /// (`is_first_run` in `embrad/src/supervisor.rs`); every other report
    /// of this module is some other sentence.
    #[test]
    fn the_no_soul_report_is_the_sentence_embrad_compares() {
        assert_eq!(NO_SOUL_ERROR, "Soul document not found — no soul exists (first run or data loss)");
    }

    #[test]
    fn the_no_soul_report_carries_the_words_embrad_looks_for() {
        assert!(NO_SOUL_ERROR.contains("no soul"));
        assert!(NO_SOUL_ERROR.contains("not found"));
        assert!(
            !SEALED_HASH_WITHOUT_SOUL_ERROR.contains("no soul")
                && !SEALED_HASH_WITHOUT_SOUL_ERROR.contains("not found"),
            "embrad must halt on it"
        );
    }

    /// A 404 for the soul document is a first run only without a stored
    /// hash. With one it is tamper or data loss, reported in words embrad
    /// halts on (#9).
    #[tokio::test]
    async fn a_missing_soul_with_a_stored_hash_halts_and_without_one_is_a_first_run() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/soul.invariant/docs/soul"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let dir = std::env::temp_dir().join(format!("embra-trustd-h5-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let hash = dir.join("soul.sha256");
        std::fs::write(&hash, "abc123\n").unwrap();

        let (valid, _, _, err) = SoulVerifier::new(server.uri(), hash.clone()).verify().await;
        assert!(!valid);
        assert!(err.starts_with(SEALED_HASH_WITHOUT_SOUL_ERROR), "{err}");
        assert!(err.contains(hash.to_str().unwrap()), "{err}");
        assert!(!err.contains("no soul") && !err.contains("not found"), "embrad must halt on it: {err}");

        let (valid, _, _, err) = SoulVerifier::new(server.uri(), dir.join("absent.sha256")).verify().await;
        assert!(!valid);
        assert_eq!(err, NO_SOUL_ERROR, "no hash: a first run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The verification of 2026-10-24: `read_stored_hash` errs on an empty
    /// or unreadable file, so keying the 404 branch on it read "present but
    /// empty" as "never sealed" and started Learning Mode. The branch keys
    /// on presence now; what the file holds does not matter.
    #[tokio::test]
    async fn a_missing_soul_with_an_empty_or_unreadable_hash_file_halts() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/soul.invariant/docs/soul"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let dir = std::env::temp_dir().join(format!("embra-trustd-h5-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let empty = dir.join("empty.sha256");
        std::fs::write(&empty, "  \n").unwrap();
        let (valid, _, _, err) = SoulVerifier::new(server.uri(), empty.clone()).verify().await;
        assert!(!valid);
        assert!(err.starts_with(SEALED_HASH_WITHOUT_SOUL_ERROR), "an empty hash file is still a stored hash: {err}");
        assert_ne!(err, NO_SOUL_ERROR);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let unreadable = dir.join("unreadable.sha256");
            std::fs::write(&unreadable, "abc123\n").unwrap();
            std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
            let (valid, _, _, err) = SoulVerifier::new(server.uri(), unreadable.clone()).verify().await;
            assert!(!valid);
            assert!(err.starts_with(SEALED_HASH_WITHOUT_SOUL_ERROR), "an unreadable hash file is present: {err}");
            let _ = std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o600));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn soul_server_doc() -> serde_json::Value {
        let soul: serde_json::Value = serde_json::from_str(PARITY_SOUL).unwrap();
        serde_json::json!({ "ok": true, "data": { "_id": "soul", "soul": soul, "sealed": true } })
    }

    async fn verify_against(body: serde_json::Value, stored: Option<&str>) -> (bool, String, String, String) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/soul.invariant/docs/soul"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "embra-trustd-verify-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let hash = dir.join("soul.sha256");
        if let Some(h) = stored {
            std::fs::write(&hash, format!("{h}\n")).unwrap();
        }
        let out = SoulVerifier::new(server.uri(), hash).verify().await;
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    /// The verdict end to end: the stored hash equal to the computed one
    /// verifies; any other stored hash is a mismatch, with both hashes in
    /// the report for the operator (#52).
    #[tokio::test]
    async fn a_hash_mismatch_is_reported_and_a_match_verifies() {
        let (valid, computed, stored, err) = verify_against(soul_server_doc(), Some(PARITY_HASH)).await;
        assert!(valid, "{err}");
        assert_eq!((computed.as_str(), stored.as_str(), err.as_str()), (PARITY_HASH, PARITY_HASH, ""));

        let other = "0".repeat(64);
        let (valid, computed, stored, err) = verify_against(soul_server_doc(), Some(&other)).await;
        assert!(!valid);
        assert_eq!(err, "Hash mismatch");
        assert_eq!(computed, PARITY_HASH);
        assert_eq!(stored, other);
    }

    /// No report of a failing database or a malformed document is the
    /// first-run sentence, whatever the database says: a verification that
    /// could not run halts the boot, it never re-seals (#52).
    #[tokio::test]
    async fn the_reports_of_a_failing_database_never_read_as_a_first_run() {
        let soul: serde_json::Value = serde_json::from_str(PARITY_SOUL).unwrap();
        let cases = [
            serde_json::json!({ "ok": false, "error": { "message": "collection not found" } }),
            serde_json::json!({ "ok": false, "error": { "message": "no soul here, boom" } }),
            serde_json::json!({ "ok": true, "data": null }),
            serde_json::json!({ "ok": true, "data": { "_id": "soul", "sealed": true } }),
            serde_json::json!({ "ok": true, "data": { "_id": "soul", "soul": soul } }),
        ];
        for body in cases {
            let (valid, _, _, err) = verify_against(body.clone(), None).await;
            assert!(!valid, "{body}");
            assert_ne!(err, NO_SOUL_ERROR, "{body}: {err}");
            assert!(!err.is_empty(), "{body}");
        }
        // The last case is the honest soul with no stored hash: the report
        // names the recovery and is not a first run either.
        let (_, _, _, err) = verify_against(soul_server_doc(), None).await;
        assert!(err.starts_with("No stored soul hash at") && err.contains("seed-state.sh --wipe data"), "{err}");
        // A database that cannot be reached at all.
        let (valid, _, _, err) = verifier("/nonexistent/soul.sha256").verify().await;
        assert!(!valid);
        assert!(err.starts_with("Failed to connect to WardSONDB"), "{err}");
    }

    /// A soul without its stored hash is a failed verification, and embrad
    /// halts on it. The report must not read as a first boot.
    #[test]
    fn a_missing_or_empty_hash_does_not_read_as_a_missing_soul() {
        let err = verifier("/nonexistent/soul.sha256")
            .read_stored_hash()
            .unwrap_err()
            .to_string();
        assert!(!err.contains("no soul") && !err.contains("not found"), "{err}");

        let dir = std::env::temp_dir().join(format!("embra-trustd-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = dir.join("soul.sha256");
        std::fs::write(&empty, "  \n").unwrap();
        let err = verifier(empty.to_str().unwrap())
            .read_stored_hash()
            .unwrap_err()
            .to_string();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!err.contains("no soul") && !err.contains("not found"), "{err}");
    }
}
