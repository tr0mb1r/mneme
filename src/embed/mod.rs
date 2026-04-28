//! Embedder seam for converting text to vectors.
//!
//! Phase 3 ships two concrete implementations behind this trait:
//! [`candle_minilm::MiniLm`] (lightweight, 384-dim) and
//! [`candle_bge::BgeM3`] (multilingual production default, 1024-dim).
//! Selection is config-driven per spec §10.3 — see [`load_from_config`].
//!
//! Async via [`async_trait`] for parity with [`crate::storage::Storage`]:
//! the rest of the system stores `Arc<dyn Embedder>` so a test can swap
//! in a mock and a release build can swap embedding models without
//! re-monomorphizing every call site.

use crate::{MnemeError, Result};
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

pub mod batch;
pub mod candle_bge;
pub mod candle_minilm;
pub mod migrate;
pub mod model_loader;
pub mod stub;

#[async_trait]
pub trait Embedder: Send + Sync {
    /// Vector dimension produced by [`embed`](Self::embed). Must be
    /// stable for the lifetime of the embedder; HNSW is configured
    /// once at startup based on this value.
    fn dim(&self) -> usize;

    async fn embed(&self, text: &str) -> Result<Vec<f32>>;

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Sync sibling of [`Embedder`] consumed by [`batch::BatchedEmbedder`].
///
/// The batched-worker pattern uses a dedicated OS thread that owns the
/// model, so the trait it sees doesn't need to be async — and removing
/// the `async` requirement lets us call directly into candle's
/// synchronous forward pass without bouncing through `spawn_blocking`
/// or a nested runtime.
pub trait BlockingEmbedder: Send + Sync {
    fn dim(&self) -> usize;
    /// Suffixed `_blocking` so concrete embedders that implement both
    /// this trait and the async [`Embedder`] don't clash on
    /// `embed_batch`.
    fn embed_batch_blocking(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>>;
}

/// Build the right concrete `Embedder` for `model_short_name`,
/// downloading weights into `cache_root` (typically
/// `~/.mneme/models/`) if not already cached.
///
/// `model_short_name` matches [`config.embeddings.model`] —
/// `"minilm-l6"` or `"bge-m3"`. Unknown names error before any I/O.
pub fn load_from_config(model_short_name: &str, cache_root: &Path) -> Result<Arc<dyn Embedder>> {
    match model_short_name {
        model_loader::MINILM_L6 => {
            let m = candle_minilm::MiniLm::load_cpu(cache_root)?;
            Ok(Arc::new(m))
        }
        model_loader::BGE_M3 => {
            let m = candle_bge::BgeM3::load_cpu(cache_root)?;
            Ok(Arc::new(m))
        }
        other => Err(MnemeError::Embedding(format!(
            "unknown embedding model `{other}`; expected `{}` or `{}`",
            model_loader::MINILM_L6,
            model_loader::BGE_M3
        ))),
    }
}
