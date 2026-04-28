use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use ulid::Ulid;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct MemoryId(pub Ulid);

impl MemoryId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    pub fn timestamp(&self) -> DateTime<Utc> {
        let ms = self.0.timestamp_ms() as i64;
        DateTime::<Utc>::from_timestamp_millis(ms).unwrap_or_default()
    }
}

impl Default for MemoryId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for MemoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct SessionId(pub Ulid);

impl SessionId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Episodic event identifier (Phase 4 L3). ULIDs are lexically
/// time-ordered so a prefix scan over the episodic table iterates in
/// creation order without an auxiliary timestamp index.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct EventId(pub Ulid);

impl EventId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    pub fn timestamp(&self) -> DateTime<Utc> {
        let ms = self.0.timestamp_ms() as i64;
        DateTime::<Utc>::from_timestamp_millis(ms).unwrap_or_default()
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
