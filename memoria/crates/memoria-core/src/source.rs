//! Protocol-independent positions and bounded adjacency for immutable sources.
use serde::{Deserialize, Serialize};

pub const SOURCE_POSITION_KEY: &str = "memoria_source_position";
pub const SOURCE_CONTEXT_KEY: &str = "memoria_source_context";
pub const SOURCE_CONTEXT_VERSION: u8 = 1;
pub const SOURCE_CONTEXT_MAX_RADIUS: u64 = 2;
pub const SOURCE_CONTEXT_MAX_LINKS: usize = 40;
pub const SOURCE_CONTEXT_MAX_CHUNKS_PER_MESSAGE: usize = 8;
pub const SOURCE_CONTEXT_MAX_FETCH: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePosition {
    /// Opaque internal ingestion batch key. Never interpreted as a timestamp.
    pub batch_id: String,
    pub message_index: u64,
    pub chunk_index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceContextLink {
    pub memory_id: String,
    pub message_index: u64,
    pub chunk_index: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceContext {
    pub version: u8,
    pub links: Vec<SourceContextLink>,
}
