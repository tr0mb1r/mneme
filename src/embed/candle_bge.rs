//! `BAAI/bge-m3` — multilingual sentence embeddings, 1024-dim.
//!
//! BGE-M3 is the v1 production default per spec §10.3. It's an
//! XLM-RoBERTa-architecture model (not BERT) with a 1024-dim hidden
//! state. Candle's `xlm_roberta::XLMRobertaModel` matches the upstream
//! reference implementation including the padding-offset position
//! embeddings that distinguish XLM-RoBERTa from BERT.
//!
//! # Pooling: [CLS], not mean
//!
//! Critically, BGE models use the **[CLS] token's hidden state** for
//! dense retrieval — *not* mean pooling. This differs from the MiniLM
//! recipe in [`super::candle_minilm`]. Mixing the two would silently
//! halve recall quality for BGE-M3 because its training objective only
//! conditions the CLS position to be a sentence representation.
//!
//! # Threading
//!
//! Same temporary `Mutex`-guarded model + `spawn_blocking` pattern as
//! `MiniLm`. Phase 3 §4 replaces it with the dedicated worker.

use crate::embed::{BlockingEmbedder, Embedder, model_loader};
use crate::{MnemeError, Result};
use async_trait::async_trait;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::xlm_roberta::{Config, XLMRobertaModel};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams, TruncationStrategy};

/// Output dimensionality for BGE-M3. Pinned in source so HNSW + the
/// `MemoryItem` schema can be sized at compile time.
pub const BGE_M3_DIM: usize = 1024;

/// Truncation cap. BGE-M3 supports 8192 tokens but the recall quality
/// curve is flat past ~512 for the kinds of facts/decisions Mneme
/// stores. 512 keeps inference latency aligned with the spec §13
/// `remember < 150ms p95` budget.
const MAX_TOKENS: usize = 512;

pub struct BgeM3 {
    inner: Arc<Mutex<Inner>>,
    device: Device,
}

struct Inner {
    model: XLMRobertaModel,
    tokenizer: Tokenizer,
}

impl BgeM3 {
    /// Download (or load from cache) BGE-M3 under `cache_root` and
    /// return a ready-to-embed instance pinned to CPU.
    pub fn load_cpu(cache_root: &Path) -> Result<Self> {
        let files = model_loader::ensure_model(model_loader::BGE_M3, cache_root)?;
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
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..Default::default()
        }));
        // Truncation is a hard cap to bound the per-call work — see
        // MAX_TOKENS rationale above.
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_TOKENS,
                strategy: TruncationStrategy::LongestFirst,
                ..Default::default()
            }))
            .map_err(|e| MnemeError::Embedding(format!("set truncation: {e}")))?;

        let vb = build_var_builder(weights_path, &device)?;
        let model = XLMRobertaModel::new(&config, vb)
            .map_err(|e| MnemeError::Embedding(format!("XLMRobertaModel::new: {e}")))?;

        Ok(Self {
            inner: Arc::new(Mutex::new(Inner { model, tokenizer })),
            device,
        })
    }

    /// Synchronous batch entry point used by the dedicated worker
    /// thread in [`crate::embed::batch`]. Acquires the model mutex,
    /// runs forward + CLS + normalize, and returns row-major rows.
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
            .forward(
                &token_ids,
                &attention_mask,
                &token_type_ids,
                None,
                None,
                None,
            )
            .map_err(|e| MnemeError::Embedding(format!("forward: {e}")))?;

        let cls = cls_token(&hidden)?;
        let normalized = l2_normalize(&cls)?;

        let rows = normalized
            .to_vec2::<f32>()
            .map_err(|e| MnemeError::Embedding(format!("to_vec2: {e}")))?;
        Ok(rows)
    }
}

impl BlockingEmbedder for BgeM3 {
    fn dim(&self) -> usize {
        BGE_M3_DIM
    }
    fn embed_batch_blocking(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        Self::embed_batch_blocking(self, texts)
    }
}

#[async_trait]
impl Embedder for BgeM3 {
    fn dim(&self) -> usize {
        BGE_M3_DIM
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
            let me = BgeM3 { inner, device };
            me.embed_batch_blocking(owned)
        })
        .await
        .map_err(|e| MnemeError::Embedding(format!("join: {e}")))?
    }
}

// ---------- Pooling ----------

/// Slice the [CLS]-token hidden state out of a `(batch, seq, hidden)`
/// tensor — i.e. position 0 along the sequence axis.
fn cls_token(hidden: &Tensor) -> Result<Tensor> {
    hidden
        .narrow(1, 0, 1)
        .and_then(|t| t.squeeze(1))
        .map_err(|e| MnemeError::Embedding(format!("cls slice: {e}")))
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

/// Build a `VarBuilder` from whatever weight format the loader
/// resolved. BAAI/bge-m3 ships `pytorch_model.bin` only — no
/// safetensors at the pinned revision — so we branch on extension.
fn build_var_builder<'a>(weights_path: &Path, device: &Device) -> Result<VarBuilder<'a>> {
    let ext = weights_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    match ext {
        "safetensors" => unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], DType::F32, device).map_err(|e| {
                MnemeError::Embedding(format!("mmap safetensors {weights_path:?}: {e}"))
            })
        },
        "bin" => VarBuilder::from_pth(weights_path, DType::F32, device)
            .map_err(|e| MnemeError::Embedding(format!("load pth {weights_path:?}: {e}"))),
        other => Err(MnemeError::Embedding(format!(
            "unsupported weights extension `.{other}` at {weights_path:?}; \
             expected `.safetensors` or `.bin`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// End-to-end smoke. Network-dependent, ~2.3 GB download — only
    /// runs when explicitly invoked. Asserts dim, batch shape, and that
    /// semantic similarity ranks correctly, including across languages
    /// (BGE-M3's main selling point over MiniLM).
    ///
    /// Run with:
    ///   `cargo test --release -- --ignored embed::candle_bge::tests::embeds_multilingual`
    #[tokio::test]
    #[ignore]
    async fn embeds_multilingual() {
        let tmp = TempDir::new().unwrap();
        let m = BgeM3::load_cpu(tmp.path()).unwrap();
        assert_eq!(Embedder::dim(&m), BGE_M3_DIM);

        let xs = m
            .embed_batch(&[
                "The cat sat on the mat.".into(),
                "Le chat est assis sur le tapis.".into(),
                "Quantum chromodynamics describes the strong force.".into(),
            ])
            .await
            .unwrap();
        assert_eq!(xs.len(), 3);
        for v in &xs {
            assert_eq!(v.len(), BGE_M3_DIM);
        }

        let cross_lingual = cosine(&xs[0], &xs[1]);
        let unrelated = cosine(&xs[0], &xs[2]);
        assert!(
            cross_lingual > unrelated + 0.1,
            "EN/FR translation pair (sim={cross_lingual}) should outscore unrelated topic (sim={unrelated})"
        );
    }

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
    }
}
