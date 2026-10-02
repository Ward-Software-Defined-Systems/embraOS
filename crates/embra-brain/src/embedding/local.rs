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

        let mut tokenizer = Tokenizer::from_file(&tok_path)
            .map_err(|e| EmbeddingError::ModelLoad(format!("{}: {e}", tok_path.display())))?;
        // The shipped tokenizer.json carries `truncation: null`, so on its own
        // the tokenizer never truncates — and cutting the RAW token list at
        // MAX_TOKENS afterwards drops the trailing [SEP] on any node longer
        // than ~510 content tokens, which the model was trained to expect.
        // Configured here, the tokenizer trims CONTENT to fit and re-applies
        // [CLS]/[SEP] itself. Pinned by `long_input_truncates_to_window_and_keeps_sep`.
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| EmbeddingError::ModelLoad(format!("truncation config: {e}")))?;

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

        // Truncation is configured on the tokenizer at load, so this cap is a
        // guard that should never bind; it only matters if that config is lost.
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

        pool_cls(&arr)
    }
}

/// The model's output `[1, S, D]` as one vector: the row of the first token.
///
/// BGE pools on CLS (position 0), NOT mean. Mean pooling here produces
/// vectors that look fine — unit norm, plausible magnitudes — but rank
/// materially worse, so this is failure-silent if it drifts. Pinned by
/// `cls_pooling_reads_position_zero`.
fn pool_cls(arr: &tract_ndarray::ArrayViewD<'_, f32>) -> Result<Vec<f32>, EmbeddingError> {
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

    /// Needs no model: the pooling is a function of the output array. Three
    /// token rows that differ, so the first row, the mean and the last row
    /// are three different vectors.
    #[test]
    fn cls_pooling_reads_position_zero() {
        let rows = 3;
        let mut data = Vec::with_capacity(rows * EMBEDDING_DIM);
        for row in 0..rows {
            for d in 0..EMBEDDING_DIM {
                // Row 0 points along the first half of the axes, row 1 along
                // the second half, row 2 along all of them.
                let on = match row {
                    0 => d < EMBEDDING_DIM / 2,
                    1 => d >= EMBEDDING_DIM / 2,
                    _ => true,
                };
                data.push(if on { 1.0f32 } else { 0.0 });
            }
        }
        let out = tract_ndarray::ArrayD::from_shape_vec(vec![1, rows, EMBEDDING_DIM], data)
            .expect("shape");
        let v = pool_cls(&out.view()).expect("pools");

        assert_eq!(v.len(), EMBEDDING_DIM);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "must be L2-normalized, got {norm}");
        // The first token's row and nothing of the others: the second half of
        // the axes is zero. A mean over the rows would put weight there.
        assert!(v[..EMBEDDING_DIM / 2].iter().all(|x| *x > 0.0));
        assert!(v[EMBEDDING_DIM / 2..].iter().all(|x| *x == 0.0));
    }

    #[test]
    fn an_output_of_another_shape_is_refused() {
        let flat = tract_ndarray::ArrayD::from_shape_vec(vec![1, EMBEDDING_DIM], vec![0.0f32; EMBEDDING_DIM])
            .expect("shape");
        assert!(pool_cls(&flat.view()).is_err());
    }

    #[test]
    #[ignore]
    fn the_forward_pass_returns_a_unit_vector() {
        let Some(dir) = model_dir() else { return };
        let p = LocalEmbeddingProvider::load(&dir, "test").expect("loads");
        let v = p.inner.embed_blocking("hello world").expect("embeds");
        assert_eq!(v.len(), EMBEDDING_DIM);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "must be L2-normalized, got {norm}");
    }

    #[test]
    #[ignore]
    fn long_input_truncates_to_window_and_keeps_sep() {
        // A node longer than the window must be trimmed to exactly MAX_TOKENS
        // with [CLS] first and [SEP] last. Cutting the raw id list instead
        // silently drops the terminator — the defect this pins.
        let Some(dir) = model_dir() else { return };
        let p = LocalEmbeddingProvider::load(&dir, "test").expect("loads");
        let tok = &p.inner.tokenizer;
        let cls = tok.token_to_id("[CLS]").expect("[CLS] in vocab");
        let sep = tok.token_to_id("[SEP]").expect("[SEP] in vocab");
        let long = "knowledge graph retrieval ".repeat(400); // well over 512 tokens
        let enc = tok.encode(long.as_str(), true).expect("encodes");
        let ids = enc.get_ids();
        assert_eq!(ids.len(), MAX_TOKENS, "trimmed to the window, not beyond it");
        assert_eq!(ids[0], cls, "[CLS] first");
        assert_eq!(*ids.last().unwrap(), sep, "[SEP] last — content is trimmed, never the terminator");
        // A short input is untouched.
        let short = tok.encode("hello world", true).expect("encodes");
        assert!(short.get_ids().len() < MAX_TOKENS);
        assert_eq!(*short.get_ids().last().unwrap(), sep);
        // And the forward pass accepts exactly the window.
        assert_eq!(p.inner.embed_blocking(&long).expect("embeds").len(), EMBEDDING_DIM);
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

#[cfg(test)]
mod measure {
    //! The measurement harness behind the embedding-model decision: two or
    //! more models over one graph and one query set, through tract and CLS
    //! pooling exactly as the OS runs them, with the vector width read from
    //! the model's output instead of the compile-time constant, and the
    //! expansion of a weak query applied by the production rule. Ignored;
    //! run by hand in release — the recipe (a scratch WardSONDB on a copy of
    //! a backup, the config shape) is in `docs/KNOWLEDGE-GRAPH.md`,
    //! "Measuring an embedding model":
    //!
    //! `EMBRA_MEASURE=<config.json> cargo test -p embra-brain --release -- \
    //!    --ignored measure_models_over_the_graph --nocapture`
    //!
    //! Config: `{"db": "http://127.0.0.1:18090", "out": "<report.json>",
    //! "models": [{"name": "bge-small-en-v1.5", "dir": "<model dir>"}, …],
    //! "queries": [{"text": "…", "kind": "crafted|conversational",
    //! "truth": ["<node id or prefix>"], "context": ["<previous user turn>", …]}]}`.
    //! The report carries, per model and query, the top-10 with cosines, the
    //! rank of every truth node, the expansion terms and the expanded top-10
    //! when the rule fired, and the inference times.
    use super::*;
    use std::collections::HashSet;
    use std::time::Instant;

    /// `Inner::embed_blocking` with the width taken from the output.
    fn embed_any(inner: &Inner, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let enc = inner
            .tokenizer
            .encode(text, true)
            .map_err(|e| EmbeddingError::Tokenize(e.to_string()))?;
        let take = |v: &[u32]| -> Vec<i64> { v.iter().take(MAX_TOKENS).map(|&x| x as i64).collect() };
        let ids = take(enc.get_ids());
        let mask = take(enc.get_attention_mask());
        let types = take(enc.get_type_ids());
        let n = ids.len();
        let tensor = |v: Vec<i64>| -> TValue {
            tract_ndarray::Array2::from_shape_vec((1, n), v).expect("shape").into_tensor().into()
        };
        let mut inputs: Vec<TValue> = Vec::with_capacity(inner.input_names.len());
        for name in &inner.input_names {
            inputs.push(match name.as_str() {
                s if s.contains("attention") => tensor(mask.clone()),
                s if s.contains("token_type") => tensor(types.clone()),
                _ => tensor(ids.clone()),
            });
        }
        let out = inner
            .plan
            .run(inputs.into())
            .map_err(|e: TractError| EmbeddingError::Inference(e.to_string()))?;
        let arr = out[0]
            .to_plain_array_view::<f32>()
            .map_err(|e| EmbeddingError::Inference(e.to_string()))?;
        let width = arr.shape()[2];
        let mut v: Vec<f32> = (0..width).map(|d| arr[[0, 0, d]]).collect();
        super::super::l2_normalize(&mut v);
        Ok(v)
    }

    fn pct(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        sorted[((sorted.len() - 1) as f64 * p).round() as usize]
    }

    fn sorted(mut v: Vec<f64>) -> Vec<f64> {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v
    }

    #[tokio::test]
    #[ignore]
    async fn measure_models_over_the_graph() {
        let cfg_path = std::env::var("EMBRA_MEASURE").expect("EMBRA_MEASURE=<config.json>");
        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg_path).expect("read config")).expect("json");
        let db = crate::db::WardsonDbClient::from_url(cfg["db"].as_str().expect("db"));

        // The corpus: every node the OS embeds, with the text the OS embeds,
        // and the tag vocabulary the expansion draws from.
        let mut corpus: Vec<(String, String, String)> = Vec::new(); // (coll, id, text)
        let mut tag_vocab: HashSet<String> = HashSet::new();
        for coll in crate::embedding::cache::EMBEDDED_COLLECTIONS {
            let docs = db.fetch_recent(coll, crate::db::MEMORY_FETCH_WINDOW).await.expect("fetch");
            tag_vocab.extend(crate::knowledge::retrieval::tag_vocabulary(docs.iter()));
            for d in &docs {
                let id = d.get("_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let text = crate::embedding::write::embed_text(d, coll);
                if !text.trim().is_empty() {
                    corpus.push((coll.to_string(), id, text));
                }
            }
        }
        let token_sets: Vec<HashSet<String>> =
            corpus.iter().map(|(_, _, t)| crate::knowledge::text::content_tokens(t)).collect();
        eprintln!("corpus: {} nodes, tag vocabulary {} tokens", corpus.len(), tag_vocab.len());

        let queries = cfg["queries"].as_array().expect("queries").clone();
        let mut report = serde_json::json!({"corpus": corpus.len(), "models": []});

        for m in cfg["models"].as_array().expect("models") {
            let name = m["name"].as_str().expect("name").to_string();
            let dir = std::path::PathBuf::from(m["dir"].as_str().expect("dir"));
            let t0 = Instant::now();
            let provider = LocalEmbeddingProvider::load(&dir, &name).expect("load");
            let load_ms = t0.elapsed().as_millis();
            let inner = provider.inner.clone();

            let mut vecs: Vec<Vec<f32>> = Vec::with_capacity(corpus.len());
            let mut doc_ms: Vec<f64> = Vec::with_capacity(corpus.len());
            for (_, _, text) in &corpus {
                let t = Instant::now();
                vecs.push(embed_any(&inner, text).expect("embed doc"));
                doc_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            let width = vecs.first().map(|v| v.len()).unwrap_or(0);
            let mean_ms = doc_ms.iter().sum::<f64>() / doc_ms.len().max(1) as f64;
            let doc_sorted = sorted(doc_ms);

            let mut qreports = Vec::new();
            let mut query_ms: Vec<f64> = Vec::new();
            for q in &queries {
                let text = q["text"].as_str().expect("text").to_string();
                let truth: Vec<String> = q["truth"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                let context: Vec<&str> = q["context"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();

                let run = |qtext: &str| -> (Vec<(usize, f32)>, f64) {
                    let t = Instant::now();
                    let qv = embed_any(&inner, &format!("{QUERY_PREFIX}{qtext}")).expect("embed query");
                    let ms = t.elapsed().as_secs_f64() * 1000.0;
                    let mut scored: Vec<(usize, f32)> =
                        vecs.iter().enumerate().map(|(i, v)| (i, crate::embedding::cosine(&qv, v))).collect();
                    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                    (scored, ms)
                };
                let (raw, ms) = run(&text);
                query_ms.push(ms);

                // The expansion exactly as retrieval applies it.
                let query_tokens = crate::knowledge::text::content_tokens(&text);
                let ctx_tokens = crate::knowledge::retrieval::context_tokens(&context, &query_tokens);
                let df_tokens: HashSet<String> = query_tokens.union(&ctx_tokens).cloned().collect();
                let df = crate::knowledge::idf::DocFreq::build(&df_tokens, token_sets.iter().cloned());
                let terms = crate::knowledge::retrieval::expansion_terms(&ctx_tokens, &df, &tag_vocab);
                let top_raw = raw.first().map(|h| h.1);
                let expanded = if crate::knowledge::retrieval::should_expand(top_raw, !terms.is_empty()) {
                    Some(run(&crate::knowledge::retrieval::embedding_query_text(&text, &terms)).0)
                } else {
                    None
                };

                let describe = |scored: &[(usize, f32)]| -> serde_json::Value {
                    let top: Vec<serde_json::Value> = scored
                        .iter()
                        .take(10)
                        .map(|(i, c)| {
                            let (coll, id, text) = &corpus[*i];
                            serde_json::json!({
                                "coll": coll, "id": id,
                                "cos": (*c as f64 * 1000.0).round() / 1000.0,
                                "rel": (crate::knowledge::retrieval::similarity_strength(*c) * 1000.0).round() / 1000.0,
                                "text": text.chars().take(110).collect::<String>(),
                            })
                        })
                        .collect();
                    let truth_ranks: Vec<serde_json::Value> = truth
                        .iter()
                        .map(|tid| {
                            let pos = scored.iter().position(|(i, _)| corpus[*i].1.starts_with(tid.as_str()));
                            serde_json::json!({
                                "id": tid,
                                "rank": pos.map(|p| p + 1),
                                "cos": pos.map(|p| (scored[p].1 as f64 * 1000.0).round() / 1000.0),
                            })
                        })
                        .collect();
                    let cos: Vec<f64> = scored.iter().map(|(_, c)| *c as f64).collect();
                    let median_all = pct(&sorted(cos.clone()), 0.5);
                    serde_json::json!({
                        "top": top,
                        "truth": truth_ranks,
                        "top1": cos.first().copied().unwrap_or(0.0),
                        "top5_mean": cos.iter().take(5).sum::<f64>() / 5.0,
                        "top10_min": cos.get(9).copied().unwrap_or(0.0),
                        "n_ge_070": cos.iter().filter(|c| **c >= 0.70).count(),
                        "n_ge_050": cos.iter().filter(|c| **c >= 0.50).count(),
                        "median_all": median_all,
                    })
                };
                qreports.push(serde_json::json!({
                    "text": text, "kind": q["kind"], "context_turns": context.len(),
                    "raw": describe(&raw),
                    "expansion_terms": terms,
                    "expanded": expanded.as_deref().map(describe),
                }));
            }
            let q_sorted = sorted(query_ms);
            report["models"].as_array_mut().unwrap().push(serde_json::json!({
                "name": name, "width": width, "load_ms": load_ms,
                "doc_ms": {"mean": mean_ms, "p50": pct(&doc_sorted, 0.5), "p95": pct(&doc_sorted, 0.95)},
                "query_ms": {"p50": pct(&q_sorted, 0.5), "p95": pct(&q_sorted, 0.95)},
                "queries": qreports,
            }));
            eprintln!(
                "{name}: width {width}, load {load_ms} ms, doc mean {mean_ms:.1} ms, query p50 {:.1} ms",
                pct(&q_sorted, 0.5)
            );
        }
        std::fs::write(cfg["out"].as_str().expect("out"), serde_json::to_string_pretty(&report).unwrap())
            .expect("write report");
    }
}
