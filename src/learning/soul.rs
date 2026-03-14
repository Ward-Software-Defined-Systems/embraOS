use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::db::WardsonDbClient;

pub async fn seal_soul(db: &WardsonDbClient, soul: &serde_json::Value) -> Result<()> {
    if !db.collection_exists("soul.invariant").await? {
        db.create_collection("soul.invariant").await?;
    }

    // Compute hash of the soul document for integrity verification
    let soul_json = serde_json::to_string_pretty(soul)?;
    let hash = compute_sha256(&soul_json);

    let sealed_doc = serde_json::json!({
        "soul": soul,
        "sha256": hash,
        "sealed_at": chrono::Utc::now().to_rfc3339(),
        "sealed": true,
    });

    db.write("soul.invariant", &sealed_doc).await?;
    Ok(())
}

pub async fn verify_soul(db: &WardsonDbClient) -> Result<bool> {
    let results = db
        .query("soul.invariant", &serde_json::json!({}))
        .await?;

    if let Some(doc) = results.into_iter().next() {
        let soul = doc
            .get("soul")
            .ok_or_else(|| anyhow::anyhow!("Soul document missing 'soul' field"))?;
        let stored_hash = doc
            .get("sha256")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Soul document missing hash"))?;

        let soul_json = serde_json::to_string_pretty(soul)?;
        let computed_hash = compute_sha256(&soul_json);

        Ok(computed_hash == stored_hash)
    } else {
        Ok(false)
    }
}

pub async fn is_soul_sealed(db: &WardsonDbClient) -> Result<bool> {
    db.collection_exists("soul.invariant").await
}

pub async fn load_soul(db: &WardsonDbClient) -> Result<Option<serde_json::Value>> {
    if !db.collection_exists("soul.invariant").await? {
        return Ok(None);
    }
    let results = db
        .query("soul.invariant", &serde_json::json!({}))
        .await?;
    Ok(results
        .into_iter()
        .next()
        .and_then(|doc| doc.get("soul").cloned()))
}

fn compute_sha256(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    format!("{:x}", hasher.finalize())
}
