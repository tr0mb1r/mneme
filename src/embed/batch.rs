//! Dedicated worker thread that owns the embedding model.
//!
//! [`BatchedEmbedder`] is the production wrapper around a
//! [`BlockingEmbedder`]. One long-lived OS thread owns the model;
//! tokio tasks send `(text, oneshot::Sender<Result<Vec<f32>>>)` jobs
//! over a bounded mpsc channel. The worker drains as much as is ready
//! up to `max_batch_size`, runs a single forward pass, then fans the
//! per-row outputs back through the oneshots.
//!
//! # Why a dedicated OS thread
//!
//! Candle's forward pass is CPU-bound and synchronous. If we ran it
//! through `tokio::task::spawn_blocking` per call, we'd lose two
//! things:
//!
//! 1. **Batching.** Per-call dispatch never amortizes the per-token
//!    overhead — a forward pass on 8 strings is materially cheaper
//!    than 8 forwards on 1 string each. The whole point of having a
//!    batch worker is to coalesce concurrent recall traffic.
//!
//! 2. **Lock contention.** A shared `Arc<Mutex<Model>>` serialises
//!    callers anyway; a single-threaded owner just makes that
//!    explicit and skips the mutex.
//!
//! # Lifecycle
//!
//! [`BatchedEmbedder::spawn`] returns an `Arc<Self>`; cloning is
//! cheap. When the last `Arc` is dropped, `Drop` closes the sender
//! channel, the worker observes EOF on its receiver, drains any
//! in-flight jobs, and exits. The thread `JoinHandle` is `take()`-ed
//! and joined so the process doesn't shut down while a forward pass
//! is mid-flight.

use crate::embed::{BlockingEmbedder, Embedder};
use crate::{MnemeError, Result};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use tokio::sync::{mpsc, oneshot};

/// Channel capacity. 256 is enough headroom that bursty arrivals
/// (e.g. an orchestrator running a parallel auto-context query)
/// don't block on `send`, but small enough that backpressure surfaces
/// quickly if the worker is genuinely overloaded.
const CHANNEL_CAP: usize = 256;

struct Job {
    text: String,
    reply: oneshot::Sender<Result<Vec<f32>>>,
}

pub struct BatchedEmbedder {
    dim: usize,
    /// Optional so `Drop` can `take()` it and explicitly drop the
    /// sender *before* joining the worker — otherwise `blocking_recv`
    /// blocks forever and the join hangs the process.
    sender: Mutex<Option<mpsc::Sender<Job>>>,
    /// Same `take()` pattern: the `Mutex` keeps `BatchedEmbedder: Sync`
    /// despite owning a non-Sync `JoinHandle`.
    join: Mutex<Option<JoinHandle<()>>>,
}

impl BatchedEmbedder {
    /// Spawn the worker thread that owns `inner` and return a handle
    /// that can be cloned freely across tasks.
    ///
    /// `max_batch_size` is the per-forward upper bound — the worker
    /// will batch up to this many concurrently-ready jobs but never
    /// stalls waiting to fill a batch (latency over throughput).
    pub fn spawn(inner: Box<dyn BlockingEmbedder>, max_batch_size: usize) -> Arc<Self> {
        assert!(max_batch_size >= 1, "max_batch_size must be ≥ 1");
        let dim = inner.dim();
        let (sender, mut receiver) = mpsc::channel::<Job>(CHANNEL_CAP);

        let worker = std::thread::Builder::new()
            .name("mneme-embed-worker".into())
            .spawn(move || {
                run_worker(inner, &mut receiver, max_batch_size);
            })
            .expect("spawn embed worker thread");

        Arc::new(Self {
            dim,
            sender: Mutex::new(Some(sender)),
            join: Mutex::new(Some(worker)),
        })
    }

    async fn submit_async(&self, job: Job) -> Result<()> {
        // Snapshot the Sender out of the Mutex so we don't hold the
        // lock across an `await`. Sender is cheap to clone (it's an
        // Arc internally).
        let snd = {
            let g = self
                .sender
                .lock()
                .map_err(|e| MnemeError::Embedding(format!("sender mutex poisoned: {e}")))?;
            g.as_ref().cloned()
        };
        match snd {
            Some(s) => s
                .send(job)
                .await
                .map_err(|_| MnemeError::Embedding("embed worker channel closed".into())),
            None => Err(MnemeError::Embedding("embed worker shut down".into())),
        }
    }
}

#[async_trait]
impl Embedder for BatchedEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let (tx, rx) = oneshot::channel();
        let job = Job {
            text: text.to_owned(),
            reply: tx,
        };
        self.submit_async(job).await?;
        rx.await
            .map_err(|_| MnemeError::Embedding("embed worker dropped reply channel".into()))?
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // Submit each text as its own job. The worker batches them
        // back together based on what's ready in the channel — which
        // may or may not be the same grouping the caller passed in.
        // Embeddings are deterministic per text and pooling is
        // independent across rows, so the result is identical either
        // way.
        let mut receivers = Vec::with_capacity(texts.len());
        for text in texts {
            let (tx, rx) = oneshot::channel();
            let job = Job {
                text: text.clone(),
                reply: tx,
            };
            self.submit_async(job).await?;
            receivers.push(rx);
        }
        let mut out = Vec::with_capacity(receivers.len());
        for rx in receivers {
            out.push(
                rx.await
                    .map_err(|_| MnemeError::Embedding("embed worker dropped reply".into()))??,
            );
        }
        Ok(out)
    }
}

impl Drop for BatchedEmbedder {
    fn drop(&mut self) {
        // Order matters: drop the sender FIRST so the worker observes
        // `blocking_recv() -> None` and exits its loop. THEN join.
        // Joining first would deadlock — the worker is parked inside
        // `blocking_recv` waiting for either a job or all-senders-
        // closed, and we'd never drop ours.
        if let Ok(mut g) = self.sender.lock() {
            let _ = g.take();
        }
        if let Ok(mut g) = self.join.lock()
            && let Some(handle) = g.take()
            && let Err(e) = handle.join()
        {
            tracing::warn!(?e, "embed worker thread panicked");
        }
    }
}

fn run_worker(
    inner: Box<dyn BlockingEmbedder>,
    rx: &mut mpsc::Receiver<Job>,
    max_batch_size: usize,
) {
    loop {
        // Block on the first job. Returns None when all senders drop.
        let first = match rx.blocking_recv() {
            Some(j) => j,
            None => return,
        };
        let mut batch = Vec::with_capacity(max_batch_size);
        batch.push(first);

        // Opportunistically drain anything else that's already ready.
        // No second `blocking_recv` — we never delay a small batch
        // hoping for a bigger one (latency budget per spec §13).
        while batch.len() < max_batch_size {
            match rx.try_recv() {
                Ok(j) => batch.push(j),
                Err(_) => break,
            }
        }

        let texts: Vec<String> = batch.iter().map(|j| j.text.clone()).collect();
        match inner.embed_batch_blocking(texts) {
            Ok(vectors) if vectors.len() == batch.len() => {
                for (j, v) in batch.into_iter().zip(vectors) {
                    let _ = j.reply.send(Ok(v));
                }
            }
            Ok(vectors) => {
                let msg = format!(
                    "embedder returned {} vectors for {} jobs",
                    vectors.len(),
                    batch.len()
                );
                for j in batch {
                    let _ = j.reply.send(Err(MnemeError::Embedding(msg.clone())));
                }
            }
            Err(e) => {
                // MnemeError isn't Clone; stringify for the fan-out.
                let msg = format!("batched embed failed: {e}");
                for j in batch {
                    let _ = j.reply.send(Err(MnemeError::Embedding(msg.clone())));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Trivial embedder for tests: returns one row per input, each row
    /// is the input length encoded into 4 floats. Counts how many
    /// times `embed_batch` is invoked so we can prove batching.
    struct CountingEmbedder {
        calls: Arc<AtomicUsize>,
        last_batch: Arc<Mutex<usize>>,
    }
    impl BlockingEmbedder for CountingEmbedder {
        fn dim(&self) -> usize {
            4
        }
        fn embed_batch_blocking(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_batch.lock().unwrap() = texts.len();
            Ok(texts
                .iter()
                .map(|t| {
                    let n = t.len() as f32;
                    vec![n, n + 1.0, n + 2.0, n + 3.0]
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn single_embed_round_trips() {
        let inner = Box::new(CountingEmbedder {
            calls: Arc::new(AtomicUsize::new(0)),
            last_batch: Arc::new(Mutex::new(0)),
        });
        let e = BatchedEmbedder::spawn(inner, 8);
        let v = e.embed("hello").await.unwrap();
        assert_eq!(v.len(), 4);
        assert_eq!(v[0], 5.0);
        assert_eq!(e.dim(), 4);
    }

    #[tokio::test]
    async fn embed_batch_returns_one_per_input_in_order() {
        let inner = Box::new(CountingEmbedder {
            calls: Arc::new(AtomicUsize::new(0)),
            last_batch: Arc::new(Mutex::new(0)),
        });
        let e = BatchedEmbedder::spawn(inner, 8);
        let xs = e
            .embed_batch(&["a".into(), "bb".into(), "ccc".into()])
            .await
            .unwrap();
        assert_eq!(xs.len(), 3);
        assert_eq!(xs[0][0], 1.0);
        assert_eq!(xs[1][0], 2.0);
        assert_eq!(xs[2][0], 3.0);
    }

    #[tokio::test]
    async fn concurrent_callers_are_batched_into_one_forward() {
        let calls = Arc::new(AtomicUsize::new(0));
        let last_batch = Arc::new(Mutex::new(0));
        let inner = Box::new(CountingEmbedder {
            calls: Arc::clone(&calls),
            last_batch: Arc::clone(&last_batch),
        });
        let e = BatchedEmbedder::spawn(inner, 16);

        // Fire 10 embed calls concurrently with no awaits between
        // sends. They all land in the channel; the worker picks up
        // the first and drains the rest in one batch.
        let futs: Vec<_> = (0..10)
            .map(|i| {
                let e = Arc::clone(&e);
                tokio::spawn(async move { e.embed(&format!("text_{i}")).await })
            })
            .collect();
        for f in futs {
            f.await.unwrap().unwrap();
        }

        // We don't promise *exactly* one forward (timing-dependent),
        // but should be far fewer than 10. In practice on a fast box
        // it's 1–2.
        let n = calls.load(Ordering::SeqCst);
        assert!(n <= 4, "expected ≤4 batched forwards, got {n}");
        let last = *last_batch.lock().unwrap();
        assert!((1..=16).contains(&last), "last batch size {last}");
    }

    #[tokio::test]
    async fn worker_shuts_down_on_last_arc_drop() {
        let inner = Box::new(CountingEmbedder {
            calls: Arc::new(AtomicUsize::new(0)),
            last_batch: Arc::new(Mutex::new(0)),
        });
        let e = BatchedEmbedder::spawn(inner, 4);
        let _ = e.embed("once").await.unwrap();
        // Dropping the last Arc<BatchedEmbedder> joins the worker.
        drop(e);
    }
}
