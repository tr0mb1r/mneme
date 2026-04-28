//! `mneme://stats` — Phase 1 stub. Returns a fixed JSON document so MCP
//! hosts can validate the resources/read code path. Real metrics land
//! in Phase 2 once we have a Storage backend to inspect.

use async_trait::async_trait;
use serde_json::json;

use super::{Resource, ResourceContent, ResourceDescriptor, ResourceError};

pub struct Stats;

#[async_trait]
impl Resource for Stats {
    fn descriptor(&self) -> ResourceDescriptor {
        ResourceDescriptor {
            uri: "mneme://stats",
            name: "stats",
            description: "Memory health metrics: counts, sizes, last consolidation timestamp.",
            mime_type: "application/json",
        }
    }

    async fn read(&self) -> Result<ResourceContent, ResourceError> {
        let body = json!({
            "phase": 1,
            "ready": false,
            "memories": { "total": 0 },
            "note": "Phase 1 stub — real metrics land in Phase 2."
        });
        Ok(ResourceContent {
            uri: self.descriptor().uri.into(),
            mime_type: "application/json",
            text: serde_json::to_string(&body).expect("static JSON serialises"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_returns_json() {
        let s = Stats;
        let c = s.read().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&c.text).unwrap();
        assert_eq!(v["phase"], 1);
    }
}
