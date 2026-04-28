//! Deterministic stub embedder.
//!
//! Two callers:
//!
//! 1. **Tests.** Lets `memory::semantic` and the MCP tool tests build a
//!    `SemanticStore` without downloading a 90 MB model on every run.
//!
//! 2. **`MNEME_EMBEDDER=stub` boot path.** When the production binary
//!    can't reach Hugging Face (offline machine, CI without network)
//!    we'd rather start in a degraded mode than refuse to launch. The
//!    env-var hand-off in [`crate::cli::run`] reads this. Stored
//!    vectors from a stub run **are not interchangeable** with vectors
//!    from a real model — the env var exists for tests, not for
//!    production data. We log loudly when it fires.
//!
//! # Design
//!
//! Outputs an L2-normalized vector keyed off the input's first byte
//! and length. Same input → same vector → identical-content memories
//! collapse, exactly the property `recall` self-test assertions need.
//! Inputs that share a prefix score closer than inputs with disjoint
//! prefixes — enough structure to validate filter behaviour without
//! pretending to be semantic.

use crate::Result;
use crate::embed::{BlockingEmbedder, Embedder};
use async_trait::async_trait;

/// Default vector dimension for the boot-path stub. Chosen to match
/// no real model on purpose so anyone who accidentally writes data
/// against the stub will see a dim mismatch the moment they switch
/// back to a real model — that loud failure is preferable to silently
/// reusing nonsense vectors.
pub const STUB_DIM: usize = 32;

#[derive(Debug, Clone, Copy)]
pub struct StubEmbedder {
    dim: usize,
}

impl StubEmbedder {
    /// Stub with the default [`STUB_DIM`].
    pub fn new() -> Self {
        Self { dim: STUB_DIM }
    }

    /// Stub with a caller-chosen dimension. Tests use small dims (e.g.
    /// 4) to keep search times trivial; the boot path uses [`STUB_DIM`].
    pub fn with_dim(dim: usize) -> Self {
        assert!(dim >= 2, "StubEmbedder dim must be ≥2 (need both axes)");
        Self { dim }
    }

    fn vector_for(&self, text: &str) -> Vec<f32> {
        let first = text.bytes().next().unwrap_or(0) as f32;
        let len = text.len() as f32;
        // Spread the signal across all `dim` slots so the unit-norm
        // constraint doesn't collapse to a single nonzero dimension.
        // Phase shifts ensure neighbouring slots aren't trivially
        // correlated.
        let mut raw = Vec::with_capacity(self.dim);
        for i in 0..self.dim {
            let phase = (i as f32) * 0.61803;
            let v = if i % 2 == 0 {
                ((first * 0.07) + phase).sin()
            } else {
                ((len * 0.13) + phase).cos()
            };
            raw.push(v);
        }
        let norm: f32 = raw
            .iter()
            .map(|x| x * x)
            .sum::<f32>()
            .sqrt()
            .max(f32::EPSILON);
        raw.iter().map(|x| x / norm).collect()
    }
}

impl Default for StubEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Embedder for StubEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.vector_for(text))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.vector_for(t)).collect())
    }
}

impl BlockingEmbedder for StubEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed_batch_blocking(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.vector_for(t)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deterministic_for_same_input() {
        let s = StubEmbedder::with_dim(8);
        let a = s.embed("hello").await.unwrap();
        let b = s.embed("hello").await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn output_is_unit_norm() {
        let s = StubEmbedder::with_dim(16);
        let v = s.embed("anything").await.unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}");
    }

    #[tokio::test]
    async fn different_inputs_produce_different_vectors() {
        let s = StubEmbedder::with_dim(8);
        let a = s.embed("alpha").await.unwrap();
        let b = s.embed("zulu zulu zulu").await.unwrap();
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        assert!(dot < 0.999, "expected non-identical vectors, dot={dot}");
    }

    #[tokio::test]
    async fn batch_matches_individual() {
        let s = StubEmbedder::with_dim(8);
        let texts = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        let batch = s.embed_batch(&texts).await.unwrap();
        for (i, t) in texts.iter().enumerate() {
            let single = s.embed(t).await.unwrap();
            assert_eq!(batch[i], single);
        }
    }
}
