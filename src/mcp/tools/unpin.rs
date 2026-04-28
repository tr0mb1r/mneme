//! `unpin` — Phase 4. Remove an entry from the procedural pinned-list
//! by id. Idempotent: removing a non-existent id reports "no such
//! pinned" rather than an error.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use ulid::Ulid;

use super::{Tool, ToolDescriptor, ToolError, ToolResult};
use crate::ids::MemoryId;
use crate::memory::procedural::ProceduralStore;

const DESCRIPTION: &str = "Remove a pinned item from procedural memory \
by ULID. Use this when a previously-pinned preference no longer \
applies. Does not error if the id is unknown — the post-condition \
'this id is not pinned' holds either way.";

pub struct Unpin {
    store: Arc<ProceduralStore>,
}

impl Unpin {
    pub fn new(store: Arc<ProceduralStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for Unpin {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "unpin",
            description: DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Procedural item ULID." }
                },
                "required": ["id"]
            }),
        }
    }

    async fn invoke(&self, args: Value) -> Result<ToolResult, ToolError> {
        let id_str = args
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("`id` is required".into()))?;
        let ulid = Ulid::from_string(id_str)
            .map_err(|e| ToolError::InvalidArguments(format!("`id` is not a valid ULID: {e}")))?;
        let memory_id = MemoryId(ulid);

        let existed = self
            .store
            .unpin(memory_id)
            .await
            .map_err(|e| ToolError::Internal(format!("unpin failed: {e}")))?;
        Ok(ToolResult::text(if existed {
            format!("unpinned {memory_id}")
        } else {
            format!("no such pinned {memory_id}")
        }))
    }
}
