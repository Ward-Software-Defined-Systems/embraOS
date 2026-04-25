//! Gemini explicit Context Cache lifecycle manager (simplified per
//! Sprint 4 locked decision #2).
//!
//! Scope:
//! - One singleton handle per Brain process, stored in WardSONDB at
//!   `provider.gemini_cache:current` with `{cache_name, fingerprint,
//!   created_at, ttl_seconds, session_name}`.
//! - Fingerprint = `sha256(system_prompt || "\x00" || canonical
//!   tools JSON)`. A change invalidates the cache.
//! - Session change invalidates the cache (the spec scopes caches
//!   per session).
//! - Stale on TTL expiry → delete + recreate at the start of the
//!   next turn (no mid-turn refresh).
//! - On 4xx from `cachedContents`, return `Ok(None)` so callers fall
//!   back to per-request system+tools. Gemini 3.1 Pro's caching
//!   eligibility isn't documented in the model table; the caller
//!   keeps working uncached if create rejects.
//!
//! Cuts vs. spec:
//! - No mid-turn TTL PATCH (delete+recreate is fine for the
//!   operator's interactive cadence).
//! - No 4096-token threshold check (system prompt is 8-11k always).
//! - 4-event telemetry: `cache:create`, `cache:hit`, `cache:miss`,
//!   `cache:delete` (the spec's 5-variant taxonomy collapses
//!   `miss_stale` and `refresh` into `miss { reason: stale|... }`
//!   and the create event respectively).

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::db::WardsonDbClient;
use crate::provider::{ProviderError, SystemPromptBundle, ToolManifest};

const COLLECTION: &str = "provider.gemini_cache";
const HANDLE_ID: &str = "current";
const API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";
const DEFAULT_TTL_SECS: u32 = 3600;
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Live handle to a Gemini cached-content resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheHandle {
    /// Full resource path returned by Gemini, e.g.
    /// `cachedContents/abc123`. Used as-is in `cachedContent`
    /// references.
    pub cache_name: String,
    /// 16-hex-char sha256 prefix over (system_prompt || tools_json).
    pub fingerprint: String,
    pub created_at: DateTime<Utc>,
    pub ttl_seconds: u32,
    pub session_name: String,
}

impl CacheHandle {
    fn is_stale(&self, now: DateTime<Utc>) -> bool {
        let age = now.signed_duration_since(self.created_at);
        age.num_seconds() >= self.ttl_seconds as i64
    }
}

pub struct GeminiCacheManager {
    http: Client,
    api_key: String,
    db: Arc<WardsonDbClient>,
    model_id: String,
}

/// Compute the cache fingerprint over a system prompt and tools
/// manifest. Stable across identical inputs; changes when either
/// side mutates.
pub fn compute_fingerprint(system: &SystemPromptBundle, tools: &ToolManifest) -> String {
    let canonical_tools = serde_json::to_string(&tools.wire_json).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(system.text.as_bytes());
    hasher.update(b"\x00");
    hasher.update(canonical_tools.as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..8])
}

impl GeminiCacheManager {
    pub fn new(api_key: String, db: Arc<WardsonDbClient>, model_id: String) -> Self {
        Self {
            http: Client::new(),
            api_key,
            db,
            model_id,
        }
    }

    /// Boot self-heal: if a stored handle's `cache_name` returns 404
    /// from Gemini, clear the WardSONDB doc so the next turn creates
    /// fresh. Costs one round-trip per Brain boot.
    pub async fn boot_self_heal(&self) {
        let stored = match self.read_stored().await {
            Some(s) => s,
            None => return,
        };
        let probe_client = match Client::builder().timeout(PROBE_TIMEOUT).build() {
            Ok(c) => c,
            Err(_) => return,
        };
        let url = format!("{}/{}", API_BASE, stored.cache_name);
        match probe_client
            .get(&url)
            .header("x-goog-api-key", &self.api_key)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => {
                info!(
                    target: "gemini::cache",
                    cache_name = %stored.cache_name,
                    "boot self-heal: existing handle still valid"
                );
            }
            Ok(r) if r.status().as_u16() == 404 => {
                warn!(
                    target: "gemini::cache",
                    cache_name = %stored.cache_name,
                    "boot self-heal: handle missing on server, clearing local doc"
                );
                let _ = self.db.delete(COLLECTION, HANDLE_ID).await;
            }
            Ok(r) => {
                warn!(
                    target: "gemini::cache",
                    status = r.status().as_u16(),
                    "boot self-heal: probe returned unexpected status; leaving handle"
                );
            }
            Err(e) => {
                warn!(target: "gemini::cache", "boot self-heal probe failed: {e}");
            }
        }
    }

    /// Reuse an existing valid handle, invalidate stale/wrong-session
    /// ones, or create a fresh cache. Returns `Ok(None)` when the
    /// model rejects caching (4xx from create) — the caller falls
    /// back to per-request system+tools.
    pub async fn ensure_cache(
        &self,
        system: &SystemPromptBundle,
        tools: &ToolManifest,
    ) -> Result<Option<CacheHandle>, ProviderError> {
        let target_fingerprint = compute_fingerprint(system, tools);
        let now = Utc::now();

        if let Some(stored) = self.read_stored().await {
            let session_match = stored.session_name == system.session_name;
            let fingerprint_match = stored.fingerprint == target_fingerprint;
            let stale = stored.is_stale(now);
            if session_match && fingerprint_match && !stale {
                info!(
                    target: "gemini::cache",
                    cache_name = %stored.cache_name,
                    fingerprint = %stored.fingerprint,
                    "cache:hit"
                );
                return Ok(Some(stored));
            }
            // Different session, fingerprint, or expired — invalidate.
            let reason = if !session_match {
                "session_changed"
            } else if !fingerprint_match {
                "stale"
            } else {
                "expired"
            };
            info!(
                target: "gemini::cache",
                old_fingerprint = %stored.fingerprint,
                new_fingerprint = %target_fingerprint,
                reason,
                "cache:miss"
            );
            // Best-effort delete on Gemini side; ignore errors.
            let _ = self.delete(&stored.cache_name).await;
            // Best-effort local clear; the upcoming write will
            // overwrite anyway.
            let _ = self.db.delete(COLLECTION, HANDLE_ID).await;
        } else {
            info!(target: "gemini::cache", "cache:miss reason=absent");
        }

        // Create fresh.
        match self
            .create_remote(system, tools, &target_fingerprint, now)
            .await?
        {
            Some(handle) => {
                info!(
                    target: "gemini::cache",
                    cache_name = %handle.cache_name,
                    fingerprint = %handle.fingerprint,
                    ttl_seconds = handle.ttl_seconds,
                    "cache:create"
                );
                self.write_stored(&handle).await;
                Ok(Some(handle))
            }
            None => {
                info!(
                    target: "gemini::cache",
                    "cache:miss reason=ineligible (model returned 4xx on create)"
                );
                Ok(None)
            }
        }
    }

    /// Drop only the locally-stored handle without any remote
    /// DELETE. Used by `stream_turn` when the server returns
    /// 403/404 on a cached_content reference — the server-side
    /// resource is already gone, so attempting a remote DELETE
    /// would just produce another 404. The next ensure_cache will
    /// recreate fresh.
    pub async fn invalidate_local(&self) {
        let _ = self.db.delete(COLLECTION, HANDLE_ID).await;
        info!(target: "gemini::cache", "cache:delete reason=server_stale (local only)");
    }

    /// Delete a cache handle remote-side. Tolerant of 404 (already
    /// gone).
    pub async fn delete(&self, cache_name: &str) -> Result<(), ProviderError> {
        let url = format!("{}/{}", API_BASE, cache_name);
        match self
            .http
            .delete(&url)
            .header("x-goog-api-key", &self.api_key)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() || r.status().as_u16() == 404 => {
                info!(target: "gemini::cache", cache_name, "cache:delete");
                Ok(())
            }
            Ok(r) => {
                let status = r.status().as_u16();
                let body = r.text().await.unwrap_or_default();
                Err(ProviderError::Http { status, body })
            }
            Err(e) => Err(ProviderError::Network(e.to_string())),
        }
    }

    async fn read_stored(&self) -> Option<CacheHandle> {
        let doc = self.db.read(COLLECTION, HANDLE_ID).await.ok()?;
        serde_json::from_value(doc).ok()
    }

    async fn write_stored(&self, handle: &CacheHandle) {
        let mut doc = serde_json::to_value(handle).unwrap_or(serde_json::json!({}));
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("_id".into(), serde_json::json!(HANDLE_ID));
        }
        // Ensure the collection exists. 409 (idempotent) is fine.
        let _ = self.db.create_collection(COLLECTION).await;
        if self.db.write(COLLECTION, &doc).await.is_err() {
            // Conflict — fall back to update.
            let _ = self.db.update(COLLECTION, HANDLE_ID, &doc).await;
        }
    }

    /// POST `cachedContents` and parse the returned `name`. Returns
    /// `Ok(None)` for any 4xx (caller treats as "model not eligible
    /// for caching"); errors only on 5xx / network issues.
    async fn create_remote(
        &self,
        system: &SystemPromptBundle,
        tools: &ToolManifest,
        fingerprint: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<CacheHandle>, ProviderError> {
        let url = format!("{}/cachedContents", API_BASE);
        let body = serde_json::json!({
            "model": format!("models/{}", self.model_id),
            "systemInstruction": {
                "parts": [{"text": system.text}]
            },
            "tools": tools.wire_json,
            // Sentinel `contents` to satisfy the field; some doc
            // examples include it explicitly. (Q3 — empty contents
            // field is not confirmed-legal in the docs.)
            "contents": [{
                "role": "user",
                "parts": [{"text": "(init)"}]
            }],
            "ttl": format!("{}s", DEFAULT_TTL_SECS),
        });

        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        let status = resp.status();
        if status.is_success() {
            let json: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| ProviderError::Decode(format!("create response: {e}")))?;
            let cache_name = json
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    ProviderError::Decode(format!(
                        "create response missing `name`: {json}"
                    ))
                })?
                .to_string();
            return Ok(Some(CacheHandle {
                cache_name,
                fingerprint: fingerprint.to_string(),
                created_at: now,
                ttl_seconds: DEFAULT_TTL_SECS,
                session_name: system.session_name.clone(),
            }));
        }
        let code = status.as_u16();
        if (400..500).contains(&code) {
            // 4xx → ineligible; caller falls back.
            let body_text = resp.text().await.unwrap_or_default();
            warn!(
                target: "gemini::cache",
                status = code,
                body = %body_text,
                "cachedContents create rejected; caching disabled for this turn"
            );
            return Ok(None);
        }
        // 5xx — bubble.
        let body_text = resp.text().await.unwrap_or_default();
        Err(ProviderError::Http {
            status: code,
            body: body_text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn bundle(text: &str) -> SystemPromptBundle {
        SystemPromptBundle {
            text: text.to_string(),
            fingerprint: "ignored".into(),
            session_name: "main".into(),
        }
    }

    fn manifest(value: serde_json::Value) -> ToolManifest {
        ToolManifest {
            wire_json: value,
            fingerprint: "ignored".into(),
        }
    }

    #[test]
    fn fingerprint_stable_for_same_inputs() {
        let s = bundle("you are an assistant");
        let t = manifest(serde_json::json!([{"functionDeclarations": []}]));
        let a = compute_fingerprint(&s, &t);
        let b = compute_fingerprint(&s, &t);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn fingerprint_changes_on_system_prompt_edit() {
        let t = manifest(serde_json::json!([]));
        let a = compute_fingerprint(&bundle("v1"), &t);
        let b = compute_fingerprint(&bundle("v2"), &t);
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_changes_on_tools_edit() {
        let s = bundle("hi");
        let t1 = manifest(serde_json::json!([{"functionDeclarations": [{"name": "a"}]}]));
        let t2 = manifest(serde_json::json!([{"functionDeclarations": [{"name": "a"}, {"name": "b"}]}]));
        let a = compute_fingerprint(&s, &t1);
        let b = compute_fingerprint(&s, &t2);
        assert_ne!(a, b);
    }

    #[test]
    fn handle_is_stale_after_ttl_expiry() {
        let created = Utc::now() - Duration::seconds(100);
        let h = CacheHandle {
            cache_name: "cachedContents/x".into(),
            fingerprint: "abc".into(),
            created_at: created,
            ttl_seconds: 60,
            session_name: "main".into(),
        };
        assert!(h.is_stale(Utc::now()));
    }

    #[test]
    fn handle_is_fresh_within_ttl() {
        let created = Utc::now() - Duration::seconds(30);
        let h = CacheHandle {
            cache_name: "cachedContents/x".into(),
            fingerprint: "abc".into(),
            created_at: created,
            ttl_seconds: 3600,
            session_name: "main".into(),
        };
        assert!(!h.is_stale(Utc::now()));
    }
}
