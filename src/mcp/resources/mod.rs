//! Resource registry. A `Resource` is a noun the agent can read.
//!
//! Phase 1 ships only `mneme://stats`. The full v1 surface (`mneme://context`,
//! `mneme://procedural`, `mneme://session/{id}`, `mneme://recent`,
//! `mneme://scopes`) lands in later phases as the underlying memory tiers
//! come online.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

pub mod stats;

#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
    #[error("resource not found: {0}")]
    NotFound(String),
    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Debug, Clone)]
pub struct ResourceDescriptor {
    pub uri: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub mime_type: &'static str,
}

#[derive(Debug, Clone)]
pub struct ResourceContent {
    pub uri: String,
    pub mime_type: &'static str,
    pub text: String,
}

impl ResourceContent {
    pub fn to_json(&self) -> Value {
        json!({
            "uri": self.uri,
            "mimeType": self.mime_type,
            "text": self.text,
        })
    }
}

#[async_trait]
pub trait Resource: Send + Sync {
    fn descriptor(&self) -> ResourceDescriptor;
    async fn read(&self) -> Result<ResourceContent, ResourceError>;
}

#[derive(Default)]
pub struct ResourceRegistry {
    resources: BTreeMap<&'static str, Arc<dyn Resource>>,
}

impl ResourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn defaults() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(stats::Stats));
        r
    }

    pub fn register(&mut self, resource: Arc<dyn Resource>) {
        let uri = resource.descriptor().uri;
        self.resources.insert(uri, resource);
    }

    pub fn get(&self, uri: &str) -> Option<Arc<dyn Resource>> {
        self.resources.get(uri).cloned()
    }

    pub fn list(&self) -> Vec<ResourceDescriptor> {
        self.resources.values().map(|r| r.descriptor()).collect()
    }
}

pub fn descriptor_to_json(d: &ResourceDescriptor) -> Value {
    json!({
        "uri": d.uri,
        "name": d.name,
        "description": d.description,
        "mimeType": d.mime_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_register_stats() {
        let r = ResourceRegistry::defaults();
        let uris: Vec<_> = r.list().iter().map(|d| d.uri).collect();
        assert_eq!(uris, vec!["mneme://stats"]);
    }
}
