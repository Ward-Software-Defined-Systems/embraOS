//! Local text embeddings for knowledge retrieval (KG-02).
//!
//! Runs entirely inside the OS: no API key, no per-query cost, no data egress,
//! and correct behaviour with the network down. The KG-02 spec assumed a
//! first-party Anthropic embeddings endpoint (`spec:33`) but flagged the claim
//! unverified at `:35-39` — and it is false: Anthropic publishes no embeddings
//! endpoint and points at Voyage AI. The spec's own §5.2 anticipated a local
//! provider; this is that branch, brought forward.
//!
//! Shape follows the `ImageProviderKind` precedent (`provider/image/mod.rs`):
//! its own kind enum, its own error type, its own trait — deliberately NOT an
//! arm on `ProviderKind`, which drives LLM construction and session-compat
//! checks. One divergence, and it is load-bearing: `resolve_image_provider`
//! constructs per tool call, which is free for an HTTP client. Loading 133 MB
//! of ONNX weights is not, so the provider is built ONCE into a process-wide
//! `OnceCell` and shared. Never "fix" this back to per-call construction.

pub mod backfill;
pub mod cache;
pub mod local;
pub mod write;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use tokio::sync::OnceCell;

use crate::config::SystemConfig;

/// Embedding width of the shipped model. Vectors of any other width are
/// rejected on read rather than silently scored against — a model change is a
/// re-embed, not a reinterpretation of existing bytes.
pub const EMBEDDING_DIM: usize = 384;

/// Hard token ceiling per embedding. The model's positional table is 512
/// (`config.json: max_position_embeddings`); longer inputs are truncated.
pub const MAX_TOKENS: usize = 512;

/// BGE is trained asymmetrically: queries carry a retrieval instruction,
/// documents carry none. Omitting it measurably degrades retrieval. This is
/// the local analogue of Voyage's `input_type` / Gemini's task-type prompt.
pub const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";

/// Baked into the rootfs by the `embra-embedding-model` Buildroot package.
const ROOTFS_MODEL_ROOT: &str = "/usr/share/embra/models";

/// Operator override, seeded onto STATE. Wins over the rootfs copy — the same
/// STATE-beats-rootfs idiom as seed-knowledge and imported-intelligence.
const STATE_MODEL_ROOT: &str = "/embra/state/models";

/// Dev override. EXCLUSIVE when set: if it names a directory without a usable
/// model, embeddings stay off rather than silently falling back to the baked
/// copy and reporting a model the developer did not choose.
const MODEL_DIR_ENV: &str = "EMBRA_EMBEDDING_MODEL_DIR";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingProviderKind {
    Local,
}

impl EmbeddingProviderKind {
    pub fn default_model(self) -> &'static str {
        match self {
            Self::Local => "bge-small-en-v1.5",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("embedding model failed to load: {0}")]
    ModelLoad(String),
    #[error("tokenization failed: {0}")]
    Tokenize(String),
    #[error("inference failed: {0}")]
    Inference(String),
}

#[async_trait::async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a search query. Implementations apply their own query-side
    /// instruction prefix; callers pass the bare text.
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;

    /// Embed documents. Returns one vector per input, in input order.
    async fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError>;

    fn model_id(&self) -> &str;

    fn dimensions(&self) -> usize;

    /// Embed a single document (no query instruction prefix). Convenience
    /// wrapper over `embed_documents` for the one-at-a-time write path.
    async fn embed_query_or_document(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let mut v = self.embed_documents(std::slice::from_ref(&text.to_string())).await?;
        v.pop().ok_or_else(|| EmbeddingError::Inference("no vector returned".into()))
    }
}

/// `true` unless the operator turned embeddings off. Default-on: a boot with
/// the model present should just work.
pub fn embedding_enabled(cfg: &SystemConfig) -> bool {
    cfg.embedding_enabled.unwrap_or(true)
}

/// The model identifier in effect (config override, else the kind's default).
pub fn model_id(cfg: &SystemConfig) -> String {
    cfg.embedding_model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| EmbeddingProviderKind::Local.default_model().to_string())
}

/// Where the model lives, and how that was decided. Pure over its inputs so
/// the precedence is testable without a filesystem — the same split as
/// `resolve_image_key_inner`.
pub(crate) fn resolve_model_dir_inner(
    env_dir: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    rootfs_dir: Option<PathBuf>,
) -> Option<(PathBuf, &'static str)> {
    // The env override is EXCLUSIVE: when set it is the only candidate.
    if let Some(d) = env_dir {
        return Some((d, "env"));
    }
    state_dir
        .map(|d| (d, "state"))
        .or_else(|| rootfs_dir.map(|d| (d, "rootfs")))
}

/// A directory is usable only if it holds BOTH files inference needs.
fn model_dir_complete(dir: &Path) -> bool {
    dir.join("model.onnx").is_file() && dir.join("tokenizer.json").is_file()
}

pub fn resolve_model_dir(cfg: &SystemConfig) -> Option<(PathBuf, &'static str)> {
    let name = model_id(cfg);
    let env_dir = std::env::var(MODEL_DIR_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from);
    // The env override is exclusive — do not consider the other roots at all.
    if let Some(d) = env_dir {
        return resolve_model_dir_inner(Some(d), None, None);
    }
    let state = PathBuf::from(STATE_MODEL_ROOT).join(&name);
    let rootfs = PathBuf::from(ROOTFS_MODEL_ROOT).join(&name);
    resolve_model_dir_inner(
        None,
        model_dir_complete(&state).then_some(state),
        model_dir_complete(&rootfs).then_some(rootfs),
    )
}

/// Process-wide, built once. `None` means "unavailable on this instance" and
/// is a normal, quiet state: retrieval degrades to lexical matching.
static PROVIDER: OnceCell<Option<Arc<dyn EmbeddingProvider>>> = OnceCell::const_new();

/// The shared provider, loading it on first use. Loading reads 133 MB and
/// optimizes the graph (~0.44 s measured), so it happens on a blocking thread
/// and exactly once per process.
pub async fn provider(cfg: &SystemConfig) -> Option<Arc<dyn EmbeddingProvider>> {
    if !embedding_enabled(cfg) {
        return None;
    }
    let name = model_id(cfg);
    let dir = resolve_model_dir(cfg);
    PROVIDER
        .get_or_init(|| async move {
            let (dir, source) = dir?;
            let started = std::time::Instant::now();
            let loaded = tokio::task::spawn_blocking(move || local::LocalEmbeddingProvider::load(&dir, &name))
                .await
                .map_err(|e| EmbeddingError::ModelLoad(format!("load task panicked: {e}")))
                .and_then(|r| r);
            match loaded {
                Ok(p) => {
                    tracing::info!(
                        target: "kg::embedding",
                        model = p.model_id(),
                        dims = p.dimensions(),
                        source,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "embedding model loaded"
                    );
                    Some(Arc::new(p) as Arc<dyn EmbeddingProvider>)
                }
                Err(e) => {
                    // Not an error state for the OS: retrieval stays lexical.
                    tracing::warn!(target: "kg::embedding", "embeddings unavailable: {e}");
                    None
                }
            }
        })
        .await
        .clone()
}

// --- Wire codec -------------------------------------------------------------
// Vectors are stored as base64 of little-endian f32 bytes, NOT as a JSON
// number array. Measured on realistic L2-normalized values: 384 x 4 B =
// 1,536 B -> 2,048 base64 chars, against ~5,200 chars of JSON — 2.5x smaller
// on disk, and far cheaper to parse, which matters because the corpus-wide
// cache load reads every one of these.

pub fn encode_vector(v: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for x in v {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Decode a stored vector. Returns `None` for anything malformed or of the
/// wrong width — a vector from a different model must never be scored against
/// the current one.
pub fn decode_vector(s: &str, expect_dim: usize) -> Option<Vec<f32>> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    if bytes.len() != expect_dim * 4 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

/// Cosine similarity. Provider vectors are L2-normalized, so this is a plain
/// dot product — the reason no ANN index is needed at this corpus size.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// L2-normalize in place. No-op on a zero vector.
pub(crate) fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_round_trips_through_base64() {
        let v: Vec<f32> = (0..EMBEDDING_DIM).map(|i| (i as f32) * 0.001 - 0.19).collect();
        let enc = encode_vector(&v);
        let back = decode_vector(&enc, EMBEDDING_DIM).expect("decodes");
        assert_eq!(v.len(), back.len());
        for (a, b) in v.iter().zip(&back) {
            assert_eq!(a.to_bits(), b.to_bits(), "f32 must survive bit-exact");
        }
    }

    #[test]
    fn encoding_is_smaller_than_a_json_number_array() {
        // Realistic values matter here: an f32 like 0.0011 serializes to six
        // characters, but a real normalized embedding component needs ~12, so
        // a tidy synthetic fixture would flatter the encoding. Build vectors
        // with full-precision mantissas instead.
        let mut v: Vec<f32> = (0..EMBEDDING_DIM)
            .map(|i| ((i * 2_654_435_761usize) % 1000) as f32 / 997.0 - 0.5)
            .collect();
        l2_normalize(&mut v);
        let b64 = encode_vector(&v).len();
        let json = serde_json::to_string(&v).unwrap().len();
        assert_eq!(b64, 2048, "384 f32 -> 1536 bytes -> 2048 base64 chars");
        assert!(
            json > b64 * 2,
            "base64 {b64} must be at least 2x smaller than json {json}"
        );
    }

    #[test]
    fn decode_rejects_wrong_width_and_garbage() {
        let v: Vec<f32> = vec![0.5; 8];
        let enc = encode_vector(&v);
        assert!(decode_vector(&enc, 8).is_some());
        // A vector from a different model must never be scored against this one.
        assert!(decode_vector(&enc, EMBEDDING_DIM).is_none());
        assert!(decode_vector("not base64 !!", 8).is_none());
        // Right byte count is required exactly, not merely sufficient.
        let trunc = base64::engine::general_purpose::STANDARD.encode([0u8; 30]);
        assert!(decode_vector(&trunc, 8).is_none());
    }

    #[test]
    fn cosine_is_dot_product_and_length_guarded() {
        let mut a = vec![3.0f32, 4.0];
        let mut b = vec![3.0f32, 4.0];
        l2_normalize(&mut a);
        l2_normalize(&mut b);
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-6, "identical unit vectors");
        let mut c = vec![-4.0f32, 3.0];
        l2_normalize(&mut c);
        assert!(cosine(&a, &c).abs() < 1e-6, "orthogonal");
        // Mismatched widths score 0 rather than panicking or half-comparing.
        assert_eq!(cosine(&a, &[1.0, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn l2_normalize_leaves_a_zero_vector_alone() {
        let mut z = vec![0.0f32; 4];
        l2_normalize(&mut z);
        assert!(z.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn model_dir_env_override_is_exclusive() {
        // When the dev override is set it is the ONLY candidate — it must not
        // fall through to state or rootfs and report a model nobody chose.
        let got = resolve_model_dir_inner(Some("/dev/override".into()), None, None);
        assert_eq!(got, Some(("/dev/override".into(), "env")));
    }

    #[test]
    fn state_wins_over_rootfs_and_rootfs_is_the_fallback() {
        let s: PathBuf = "/embra/state/models/m".into();
        let r: PathBuf = "/usr/share/embra/models/m".into();
        assert_eq!(
            resolve_model_dir_inner(None, Some(s.clone()), Some(r.clone())),
            Some((s, "state")),
            "operator drop-in beats the baked copy"
        );
        assert_eq!(
            resolve_model_dir_inner(None, None, Some(r.clone())),
            Some((r, "rootfs"))
        );
        assert_eq!(resolve_model_dir_inner(None, None, None), None);
    }
}

