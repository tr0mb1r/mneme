//! `mneme://procedural` — current procedural pinned-list as JSON.
//! Hosts read this to surface the always-on context block; the
//! source of truth is `<root>/procedural/pinned.jsonl`, which is
//! human-editable.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::{Resource, ResourceContent, ResourceDescriptor, ResourceError};
use crate::memory::procedural::ProceduralStore;

pub struct Procedural {
    store: Arc<ProceduralStore>,
}

impl Procedural {
    pub fn new(store: Arc<ProceduralStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Resource for Procedural {
    fn descriptor(&self) -> ResourceDescriptor {
        ResourceDescriptor {
            uri: "mneme://procedural",
            name: "procedural",
            description: "Always-on pinned items: preferences, identity facts, binding decisions.",
            mime_type: "application/json",
        }
    }

    async fn read(&self, _uri: &str) -> Result<ResourceContent, ResourceError> {
        let items = self
            .store
            .list(None)
            .map_err(|e| ResourceError::Internal(format!("procedural list: {e}")))?;
        let body: Vec<_> = items
            .into_iter()
            .map(|p| {
                json!({
                    "id": p.id.to_string(),
                    "content": p.content,
                    "tags": p.tags,
                    "scope": p.scope,
                    "created_at": p.created_at.to_rfc3339(),
                })
            })
            .collect();
        let text = serde_json::to_string(&body)
            .map_err(|e| ResourceError::Internal(format!("serialise: {e}")))?;
        Ok(ResourceContent {
            uri: self.descriptor().uri.into(),
            mime_type: "application/json",
            text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use tempfile::TempDir;

    fn fixture() -> (Procedural, TempDir) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        (Procedural::new(store), tmp)
    }

    #[tokio::test]
    async fn empty_store_returns_empty_array() {
        let (p, _tmp) = fixture();
        let c = p.read("mneme://procedural").await.unwrap();
        assert_eq!(c.text, "[]");
    }

    #[tokio::test]
    async fn single_pinned_item_round_trips() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        store
            .pin("test rule".into(), vec!["tag1".into()], "work".into())
            .await
            .unwrap();
        let p = Procedural::new(store);
        let c = p.read("mneme://procedural").await.unwrap();
        let v: Vec<serde_json::Value> = serde_json::from_str(&c.text).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["content"], "test rule");
        assert_eq!(v[0]["tags"], json!(["tag1"]));
        assert_eq!(v[0]["scope"], "work");
    }

    #[tokio::test]
    async fn serialisation_format_has_required_fields() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(ProceduralStore::open(tmp.path()).unwrap());
        store
            .pin("rule".into(), vec![], "personal".into())
            .await
            .unwrap();
        let p = Procedural::new(store);
        let c = p.read("mneme://procedural").await.unwrap();
        let v: Vec<serde_json::Value> = serde_json::from_str(&c.text).unwrap();
        assert_eq!(v.len(), 1);
        assert!(v[0].get("id").and_then(Value::as_str).is_some());
        assert!(v[0].get("content").and_then(Value::as_str).is_some());
        assert!(v[0].get("tags").and_then(Value::as_array).is_some());
        assert!(v[0].get("scope").and_then(Value::as_str).is_some());
        assert!(v[0].get("created_at").and_then(Value::as_str).is_some());
    }
}
