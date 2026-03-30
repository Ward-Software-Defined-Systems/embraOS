//! Soul hash verification.
//!
//! Reads soul.invariant from WardSONDB, computes SHA-256 of the canonical
//! JSON representation, and compares against the stored hash on STATE.

use anyhow::{Result, Context};
use sha2::{Sha256, Digest};
use tracing::{info, debug};

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
            anyhow::bail!("Soul document not found — no soul exists (first run or data loss)");
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

    /// Store the soul hash on the STATE partition.
    /// Called after Learning Mode seals the soul.
    pub fn store_hash(&self, hash: &str) -> Result<()> {
        // Create parent directories if needed
        if let Some(parent) = self.hash_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.hash_path, hash)?;
        info!("Soul hash stored at {}", self.hash_path.display());
        Ok(())
    }
}
