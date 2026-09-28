//! Soul hash verification.
//!
//! Reads soul.invariant from WardSONDB, computes SHA-256 of the canonical
//! JSON representation, and compares against the stored hash on STATE.

use anyhow::{Result, Context};
use sha2::{Sha256, Digest};
use tracing::debug;

/// What `verify` reports when WardSONDB holds no soul document.
///
/// embrad reads this TEXT to tell a first boot from a failed verification
/// (`embrad/src/supervisor.rs`, `is_first_run`): it looks for "no soul" and
/// for "not found". Reword it there as well, or the first boot halts. No
/// other report of this module may carry either phrase, or a failed
/// verification would read as a first boot.
const NO_SOUL_ERROR: &str = "Soul document not found — no soul exists (first run or data loss)";

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
        let soul_json = self.read_soul_from_db().await?;

        // Compute SHA-256
        let computed_hash = self.compute_hash(&soul_json);
        debug!("Computed soul hash: {}", computed_hash);

        // Read stored hash from STATE
        let stored_hash = self.read_stored_hash()?;
        debug!("Stored soul hash: {}", stored_hash);

        Ok((computed_hash, stored_hash))
    }

    async fn read_soul_from_db(&self) -> Result<serde_json::Value> {
        let client = reqwest::Client::new();
        let url = format!("{}/soul.invariant/docs/soul", self.wardsondb_url);

        let response = client.get(&url).send().await
            .context("Failed to connect to WardSONDB")?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            anyhow::bail!(NO_SOUL_ERROR);
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

        Ok(doc)
    }

    fn compute_hash(&self, soul_json: &serde_json::Value) -> String {
        // Extract the "soul" field — hash only the soul content, not metadata
        let soul_content = &soul_json["soul"];

        // Use to_string_pretty to match embra-brain's seal_soul() serialization
        let canonical = serde_json::to_string_pretty(soul_content)
            .unwrap_or_default();

        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    fn read_stored_hash(&self) -> Result<String> {
        if !self.hash_path.exists() {
            anyhow::bail!("No stored soul hash at {} — first boot or STATE partition issue",
                self.hash_path.display());
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
        assert_eq!(verifier("/nonexistent").compute_hash(&doc), PARITY_HASH);
    }

    #[test]
    fn the_no_soul_report_carries_the_words_embrad_looks_for() {
        assert!(NO_SOUL_ERROR.contains("no soul"));
        assert!(NO_SOUL_ERROR.contains("not found"));
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
