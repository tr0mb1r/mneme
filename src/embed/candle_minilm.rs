//! `sentence-transformers/all-MiniLM-L6-v2` over candle.
//!
//! 384-dim sentence embeddings; ~80 MB model. The Phase 3 dev/test
//! default — small enough that CI can download it on demand.
//!
//! # Inference path
//!
//! Tokenize → forward through `BertModel` → mean-pool over the
//! attention mask → L2-normalize. This is the recipe the
//! sentence-transformers Python library uses; matching it byte-for-byte
//! lets users reuse retrieval pipelines built against the canonical
//! model without re-embedding.
//!
//! # Threading
//!
//! `Tokenizer` and `BertModel` are not `Sync` — so we keep them under
//! a `Mutex` and run forward passes on `tokio::task::spawn_blocking`.
//! Phase 3 §4 will replace this with a dedicated worker thread + mpsc
//! queue that batches inputs; the `Embedder` trait stays unchanged.
//!
//! # Repo id
//!
//! We pin to the bare repo name (no revision pin). Phase 3 §2 hardens
//! this with SHA256 verification per spec §8.5.

use crate::embed::{BlockingEmbedder, Embedder, model_loader};
use crate::{MnemeError, Result};
use async_trait::async_trait;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer};

/// Output dimensionality. Hard-coded so callers (HNSW, MemoryItem
/// schema) can size buffers without a runtime round-trip.
pub const MINILM_DIM: usize = 384;

pub struct MiniLm {
    inner: Arc<Mutex<Inner>>,
    device: Device,
}

struct Inner {
    model: BertModel,
    tokenizer: Tokenizer,
}

impl MiniLm {
    /// Download (or load from cache) the model under `cache_root` and
    /// return a ready-to-embed instance pinned to CPU.
    ///
    /// CPU is the only universally portable backend. Metal/CUDA
    /// acceleration lands later — opt-in via Cargo features so the
    /// default build stays single-binary across all 5 release targets.
    pub fn load_cpu(cache_root: &Path) -> Result<Self> {
        let files = model_loader::ensure_model(model_loader::MINILM_L6, cache_root)?;
        Self::load_from_files(&files.config, &files.tokenizer, &files.weights, Device::Cpu)
    }

    fn load_from_files(
        config_path: &Path,
        tokenizer_path: &Path,
        weights_path: &Path,
        device: Device,
    ) -> Result<Self> {
        let config_bytes = std::fs::read_to_string(config_path)
            .map_err(|e| MnemeError::Embedding(format!("read config: {e}")))?;
        let config: Config = serde_json::from_str(&config_bytes)
            .map_err(|e| MnemeError::Embedding(format!("parse config: {e}")))?;

        let mut tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| MnemeError::Embedding(format!("load tokenizer: {e}")))?;
        // Pad batches to the longest sequence so we can stack into one
        // tensor. The matching attention mask zeros out the padding
        // contribution during mean pooling.
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..Default::default()
        }));

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DTYPE, &device)
                .map_err(|e| MnemeError::Embedding(format!("mmap weights: {e}")))?
        };
        let model = BertModel::load(vb, &config)
            .map_err(|e| MnemeError::Embedding(format!("BertModel::load: {e}")))?;

        Ok(Self {
            inner: Arc::new(Mutex::new(Inner { model, tokenizer })),
            device,
        })
    }

    /// Synchronous batch entry point used by the dedicated worker
    /// thread in [`crate::embed::batch`]. Acquires the model mutex,
    /// runs forward + pool + normalize, and returns row-major rows.
    pub fn embed_batch_blocking(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let inner = self
            .inner
            .lock()
            .map_err(|e| MnemeError::Embedding(format!("model mutex poisoned: {e}")))?;

        let encodings = inner
            .tokenizer
            .encode_batch(texts, true)
            .map_err(|e| MnemeError::Embedding(format!("tokenize: {e}")))?;

        let token_ids: Vec<Tensor> = encodings
            .iter()
            .map(|enc| {
                let ids: Vec<u32> = enc.get_ids().to_vec();
                Tensor::new(ids.as_slice(), &self.device)
                    .map_err(|e| MnemeError::Embedding(format!("ids tensor: {e}")))
            })
            .collect::<Result<_>>()?;
        let attention_mask: Vec<Tensor> = encodings
            .iter()
            .map(|enc| {
                let mask: Vec<u32> = enc.get_attention_mask().to_vec();
                Tensor::new(mask.as_slice(), &self.device)
                    .map_err(|e| MnemeError::Embedding(format!("mask tensor: {e}")))
            })
            .collect::<Result<_>>()?;

        let token_ids = Tensor::stack(&token_ids, 0)
            .map_err(|e| MnemeError::Embedding(format!("stack ids: {e}")))?;
        let attention_mask = Tensor::stack(&attention_mask, 0)
            .map_err(|e| MnemeError::Embedding(format!("stack mask: {e}")))?;
        let token_type_ids = token_ids
            .zeros_like()
            .map_err(|e| MnemeError::Embedding(format!("token_type_ids: {e}")))?;

        let hidden = inner
            .model
            .forward(&token_ids, &token_type_ids, Some(&attention_mask))
            .map_err(|e| MnemeError::Embedding(format!("forward: {e}")))?;

        let pooled = mean_pool(&hidden, &attention_mask)?;
        let normalized = l2_normalize(&pooled)?;

        let rows = normalized
            .to_vec2::<f32>()
            .map_err(|e| MnemeError::Embedding(format!("to_vec2: {e}")))?;
        Ok(rows)
    }
}

impl BlockingEmbedder for MiniLm {
    fn dim(&self) -> usize {
        MINILM_DIM
    }
    fn embed_batch_blocking(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        Self::embed_batch_blocking(self, texts)
    }
}

#[async_trait]
impl Embedder for MiniLm {
    fn dim(&self) -> usize {
        MINILM_DIM
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed_batch(&[text.to_owned()]).await?;
        v.pop()
            .ok_or_else(|| MnemeError::Embedding("empty embed_batch result".into()))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let owned: Vec<String> = texts.to_vec();
        let inner = Arc::clone(&self.inner);
        let device = self.device.clone();
        tokio::task::spawn_blocking(move || {
            // Reconstruct a temporary view to call the blocking impl.
            // Avoids cloning the model; only the Arc<Mutex<Inner>> moves.
            let me = MiniLm { inner, device };
            me.embed_batch_blocking(owned)
        })
        .await
        .map_err(|e| MnemeError::Embedding(format!("join: {e}")))?
    }
}

// ---------- Pooling ----------

/// Mean-pool over the sequence axis, masking padding tokens.
///
/// `hidden`        : (batch, seq_len, hidden_size) f32
/// `attention_mask`: (batch, seq_len) u32
fn mean_pool(hidden: &Tensor, attention_mask: &Tensor) -> Result<Tensor> {
    let mask = attention_mask
        .to_dtype(DType::F32)
        .map_err(|e| MnemeError::Embedding(format!("mask dtype: {e}")))?
        .unsqueeze(2)
        .map_err(|e| MnemeError::Embedding(format!("mask unsqueeze: {e}")))?;
    let masked = hidden
        .broadcast_mul(&mask)
        .map_err(|e| MnemeError::Embedding(format!("masked mul: {e}")))?;
    let summed = masked
        .sum(1)
        .map_err(|e| MnemeError::Embedding(format!("sum: {e}")))?;
    let denom = mask
        .sum(1)
        .map_err(|e| MnemeError::Embedding(format!("denom: {e}")))?;
    summed
        .broadcast_div(&denom)
        .map_err(|e| MnemeError::Embedding(format!("mean div: {e}")))
}

fn l2_normalize(v: &Tensor) -> Result<Tensor> {
    let norm = v
        .sqr()
        .and_then(|x| x.sum_keepdim(1))
        .and_then(|x| x.sqrt())
        .map_err(|e| MnemeError::Embedding(format!("norm: {e}")))?;
    v.broadcast_div(&norm)
        .map_err(|e| MnemeError::Embedding(format!("normalize: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// End-to-end smoke: download the model, embed two related sentences
    /// + one unrelated one, assert dimensionality and that semantic
    /// similarity is in the right ballpark.
    ///
    /// Network-dependent; ignored by default. Run with:
    ///   `cargo test -- --ignored embed::candle_minilm::tests::embeds_and_ranks`
    #[tokio::test]
    #[ignore]
    async fn embeds_and_ranks() {
        let tmp = TempDir::new().unwrap();
        let m = MiniLm::load_cpu(tmp.path()).unwrap();
        assert_eq!(Embedder::dim(&m), MINILM_DIM);

        let xs = m
            .embed_batch(&[
                "The cat sat on the mat.".into(),
                "A feline rested on the rug.".into(),
                "Quantum chromodynamics describes the strong force.".into(),
            ])
            .await
            .unwrap();
        assert_eq!(xs.len(), 3);
        for v in &xs {
            assert_eq!(v.len(), MINILM_DIM);
        }

        let sim_close = cosine(&xs[0], &xs[1]);
        let sim_far = cosine(&xs[0], &xs[2]);
        assert!(
            sim_close > sim_far + 0.1,
            "expected related sentences (sim={sim_close}) to score above unrelated (sim={sim_far})"
        );
    }

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        // Vectors come out L2-normalized, so cosine == dot.
        a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
    }
}
