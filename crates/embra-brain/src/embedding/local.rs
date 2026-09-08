//! In-OS ONNX inference via `tract` — pure Rust, no C++ runtime.
//!
//! `ort` and `rust-bert` bind libonnxruntime, which cannot static-link into
//! the musl ship binaries; `tract` is self-contained Rust (its only C is
//! `tract-linalg`'s SIMD assembly, built by `cc`, which the tree already
//! pulls via ring/rustls). Everything below was established by measurement
//! against `BAAI/bge-small-en-v1.5`, not inferred from documentation.

use std::path::Path;
use std::sync::Arc;

use tokenizers::Tokenizer;
use tokio::sync::Semaphore;
use tract_onnx::prelude::*;

use super::{EmbeddingError, EmbeddingProvider, EMBEDDING_DIM, MAX_TOKENS, QUERY_PREFIX};

/// Concurrent inference slots. Inference is CPU-bound and single-threaded per
/// call; more slots than this just starve the async runtime on a QEMU vCPU.
/// Mirrors the media wave's 2-slot decode gate.
const INFERENCE_SLOTS: usize = 2;

/// `SimplePlan::run` takes `self: &Arc<Self>`, and `into_runnable()` hands
/// back the `Arc` — so the plan is stored Arc'd rather than owned.
type Plan = Arc<TypedRunnableModel>;

struct Inner {
    plan: Plan,
    tokenizer: Tokenizer,
    /// Declared ONNX input names, in graph order. Measured for bge-small:
    /// `["input_ids", "attention_mask", "token_type_ids"]`. Tensors are
    /// dispatched BY NAME, never by position — a re-export with a different
    /// order would otherwise feed the mask as ids and silently produce
    /// plausible garbage.
    input_names: Vec<String>,
}

pub struct LocalEmbeddingProvider {
    inner: Arc<Inner>,
    model_id: String,
    permits: Semaphore,
}

impl LocalEmbeddingProvider {
    /// Load and optimize. Blocking and expensive (~0.44 s measured, plus a
    /// 133 MB read) — callers run this on a blocking thread, once per process.
    pub fn load(dir: &Path, model_id: &str) -> Result<Self, EmbeddingError> {
        let tok_path = dir.join("tokenizer.json");
        let onnx_path = dir.join("model.onnx");

        let tokenizer = Tokenizer::from_file(&tok_path)
            .map_err(|e| EmbeddingError::ModelLoad(format!("{}: {e}", tok_path.display())))?;

        let mut model = tract_onnx::onnx()
            .model_for_path(&onnx_path)
            .map_err(|e| EmbeddingError::ModelLoad(format!("{}: {e}", onnx_path.display())))?;

        let input_names: Vec<String> = (0..model.inputs.len())
            .map(|i| model.node(model.inputs[i].node).name.clone())
            .collect();

        // SYMBOLIC sequence length: one optimized plan serves every input
        // length. Fixing the axis instead would force either padding every
        // input to 512 (a ~40x waste on a short query) or re-optimizing the
        // graph per length bucket. Verified working on this model.
        let sym = model.symbols.sym("S");
        for i in 0..input_names.len() {
            model
                .set_input_fact(
                    i,
                    InferenceFact::dt_shape(i64::datum_type(), tvec!(1.into(), sym.clone().to_dim())),
                )
                .map_err(|e| EmbeddingError::ModelLoad(format!("input fact {i}: {e}")))?;
        }

        let plan = model
            .into_optimized()
            .map_err(|e| EmbeddingError::ModelLoad(format!("optimize: {e}")))?
            .into_runnable()
            .map_err(|e| EmbeddingError::ModelLoad(format!("runnable: {e}")))?;

        Ok(Self {
            inner: Arc::new(Inner { plan, tokenizer, input_names }),
            model_id: model_id.to_string(),
            permits: Semaphore::new(INFERENCE_SLOTS),
        })
    }
}

impl Inner {
    /// One forward pass. Blocking.
    fn embed_blocking(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let enc = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| EmbeddingError::Tokenize(e.to_string()))?;

        let take = |v: &[u32]| -> Vec<i64> {
            v.iter().take(MAX_TOKENS).map(|&x| x as i64).collect()
        };
        let ids = take(enc.get_ids());
        let mask = take(enc.get_attention_mask());
        let types = take(enc.get_type_ids());
        let n = ids.len();
        if n == 0 {
            return Err(EmbeddingError::Tokenize("empty token sequence".into()));
        }

        let tensor = |v: Vec<i64>| -> Result<TValue, EmbeddingError> {
            tract_ndarray::Array2::from_shape_vec((1, n), v)
                .map(|a| a.into_tensor().into())
                .map_err(|e| EmbeddingError::Inference(format!("tensor shape: {e}")))
        };

        let mut inputs: Vec<TValue> = Vec::with_capacity(self.input_names.len());
        for name in &self.input_names {
            inputs.push(match name.as_str() {
                s if s.contains("attention") => tensor(mask.clone())?,
                s if s.contains("token_type") => tensor(types.clone())?,
                _ => tensor(ids.clone())?,
            });
        }

        let out = self
            .plan
            .run(inputs.into())
            .map_err(|e: TractError| EmbeddingError::Inference(e.to_string()))?;
        let arr = out[0]
            .to_plain_array_view::<f32>()
            .map_err(|e| EmbeddingError::Inference(format!("output view: {e}")))?;

        // BGE pools on CLS (position 0), NOT mean. Mean pooling here produces
        // vectors that look fine — unit norm, plausible magnitudes — but rank
        // materially worse, so this is failure-silent if it drifts. Pinned by
        // `cls_pooling_reads_position_zero`.
        let shape = arr.shape();
        if shape.len() != 3 || shape[2] != EMBEDDING_DIM {
            return Err(EmbeddingError::Inference(format!(
                "unexpected output shape {shape:?}, want [1, S, {EMBEDDING_DIM}]"
            )));
        }
        let mut v: Vec<f32> = (0..EMBEDDING_DIM).map(|d| arr[[0, 0, d]]).collect();
        super::l2_normalize(&mut v);
        Ok(v)
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for LocalEmbeddingProvider {
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let prefixed = format!("{QUERY_PREFIX}{text}");
        self.run_one(prefixed).await
    }

    async fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.run_one(t.clone()).await?);
        }
        Ok(out)
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dimensions(&self) -> usize {
        EMBEDDING_DIM
    }
}

impl LocalEmbeddingProvider {
    /// Bounded, off the async runtime. `Inner` lives behind an `Arc` precisely
    /// so `spawn_blocking`'s `'static` bound is satisfiable without copying
    /// the model.
    async fn run_one(&self, text: String) -> Result<Vec<f32>, EmbeddingError> {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|e| EmbeddingError::Inference(format!("semaphore closed: {e}")))?;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.embed_blocking(&text))
            .await
            .map_err(|e| EmbeddingError::Inference(format!("inference task panicked: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    //! The model itself is a 133 MB rootfs artifact, so inference tests are
    //! `#[ignore]`d and run by hand against a real model directory (the same
    //! discipline as the wardsondb-dependent measurements: nothing that needs
    //! a large external artifact runs in the default suite).
    use super::*;

    fn model_dir() -> Option<std::path::PathBuf> {
        std::env::var("EMBRA_EMBEDDING_MODEL_DIR").ok().map(Into::into)
    }

    #[test]
    #[ignore]
    fn cls_pooling_reads_position_zero() {
        let Some(dir) = model_dir() else { return };
        let p = LocalEmbeddingProvider::load(&dir, "test").expect("loads");
        let v = p.inner.embed_blocking("hello world").expect("embeds");
        assert_eq!(v.len(), EMBEDDING_DIM);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "must be L2-normalized, got {norm}");
    }

    #[test]
    #[ignore]
    fn paraphrase_outranks_unrelated_text() {
        // The semantic check that actually catches wrong pooling: a query and
        // its paraphrase must score above an unrelated sentence.
        let Some(dir) = model_dir() else { return };
        let p = LocalEmbeddingProvider::load(&dir, "test").expect("loads");
        let e = |s: &str| p.inner.embed_blocking(s).expect("embeds");
        let q = e(&format!("{QUERY_PREFIX}Why did the terminal go blank after reload?"));
        let related = e("SIGWINCH repaint on browser attach: a fresh tab starts with an empty xterm and its same-size resize is a kernel no-op.");
        let unrelated = e("The Mediterranean diet emphasizes fish, olive oil, and vegetables.");
        let (a, b) = (super::super::cosine(&q, &related), super::super::cosine(&q, &unrelated));
        assert!(a > b, "paraphrase {a} must outrank unrelated {b}");
    }
}
