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
        "_id": "soul",
        "soul": soul,
        "sha256": hash,
        "sealed_at": chrono::Utc::now().to_rfc3339(),
        "sealed": true,
    });

    db.write("soul.invariant", &sealed_doc).await?;

    // Write hash to STATE partition for boot-time verification by embra-trustd
    let hash_path = "/embra/state/soul.sha256";
    if let Some(parent) = std::path::Path::new(hash_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(hash_path, &hash)
        .map_err(|e| anyhow::anyhow!("Failed to write soul hash to STATE: {}", e))?;
    tracing::info!("Soul hash written to {}", hash_path);

    Ok(())
}

pub async fn verify_soul(db: &WardsonDbClient) -> Result<bool> {
    let doc = match load_soul_doc(db).await? {
        Some(doc) => doc,
        None => return Ok(false),
    };

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
}

pub async fn is_soul_sealed(db: &WardsonDbClient) -> Result<bool> {
    db.collection_exists("soul.invariant").await
}

pub async fn load_soul(db: &WardsonDbClient) -> Result<Option<serde_json::Value>> {
    Ok(load_soul_doc(db)
        .await?
        .and_then(|doc| doc.get("soul").cloned()))
}

/// Load the soul document, trying direct GET first, falling back to query.
async fn load_soul_doc(db: &WardsonDbClient) -> Result<Option<serde_json::Value>> {
    if !db.collection_exists("soul.invariant").await? {
        return Ok(None);
    }
    // Try direct GET by well-known ID
    if let Ok(doc) = db.read("soul.invariant", "soul").await {
        return Ok(Some(doc));
    }
    // Fallback: query (pre-migration data)
    let results = db
        .query("soul.invariant", &serde_json::json!({}))
        .await?;
    Ok(results.into_iter().next())
}

fn compute_sha256(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Compute the SHA-256 hash of a soul value, using the same serialization
/// as `seal_soul` and `verify_soul`. Shared by startup validation and migrations.
pub fn compute_soul_hash(soul: &serde_json::Value) -> Result<String> {
    let soul_json = serde_json::to_string_pretty(soul)?;
    Ok(compute_sha256(&soul_json))
}
