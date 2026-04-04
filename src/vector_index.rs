//! Vector index for semantic search over MCP server/tool descriptions.
//!
//! Stores pre-computed embedding vectors alongside server/tool metadata.
//! Supports cosine similarity search (brute-force flat index).
//! The index is a local cache — it can always be rebuilt via `dmcp sync-index`.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Embedding model specification stored alongside the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingSpec {
    pub model: String,
    pub version: String,
    pub dimensions: usize,
}

/// A single entry in the vector index (server-level or tool-level).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorEntry {
    pub server_id: String,
    pub server_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_description: Option<String>,
    /// `None` = server-level entry; `Some` = tool-level entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_schema: Option<serde_json::Value>,
    /// Pre-computed embedding vector.
    pub vector: Vec<f32>,
    /// `"registry"` (from sync-index) or `"local"` (from index-server).
    pub source: String,
}

/// The local vector index, stored at `~/.local/share/mcp/vector_index/index.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VectorIndex {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_spec: Option<EmbeddingSpec>,
    #[serde(default)]
    pub entries: Vec<VectorEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced: Option<String>,
}

/// A single search result with similarity score.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub server_id: String,
    pub server_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter_schema: Option<serde_json::Value>,
    pub score: f32,
}

impl VectorIndex {
    /// Load the index from a JSON file. Returns an empty index if the file does not exist.
    pub fn load(path: &Path) -> Result<Self, VectorIndexError> {
        if !path.exists() {
            return Ok(VectorIndex::default());
        }
        let content = std::fs::read_to_string(path).map_err(VectorIndexError::Io)?;
        serde_json::from_str(&content).map_err(VectorIndexError::Parse)
    }

    /// Save the index to a JSON file, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<(), VectorIndexError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(VectorIndexError::Io)?;
        }
        let content = serde_json::to_string_pretty(self).map_err(VectorIndexError::Serialize)?;
        std::fs::write(path, content).map_err(VectorIndexError::Io)
    }

    /// Cosine similarity between two vectors. Returns 0.0 if lengths differ or either is zero.
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if mag_a == 0.0 || mag_b == 0.0 {
            0.0
        } else {
            dot / (mag_a * mag_b)
        }
    }

    /// Search the index for the most similar entries to `query`.
    /// Returns up to `top_k` results with score >= `min_score`, ordered by descending score.
    pub fn search(&self, query: &[f32], top_k: usize, min_score: f32) -> Vec<SearchResult> {
        let mut scored: Vec<(f32, &VectorEntry)> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let score = Self::cosine_similarity(query, &entry.vector);
                if score >= min_score {
                    Some((score, entry))
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);

        scored
            .into_iter()
            .map(|(score, entry)| SearchResult {
                server_id: entry.server_id.clone(),
                server_name: entry.server_name.clone(),
                server_description: entry.server_description.clone(),
                tool_name: entry.tool_name.clone(),
                tool_description: entry.tool_description.clone(),
                parameter_schema: entry.parameter_schema.clone(),
                score,
            })
            .collect()
    }

    /// Batch search: one result set per query vector, each respecting the same top_k / min_score.
    pub fn search_batch(
        &self,
        queries: &[Vec<f32>],
        top_k: usize,
        min_score: f32,
    ) -> Vec<Vec<SearchResult>> {
        queries
            .iter()
            .map(|q| self.search(q, top_k, min_score))
            .collect()
    }

    /// Remove all entries for a given `server_id` from the index.
    pub fn remove_server_entries(&mut self, server_id: &str) {
        self.entries.retain(|e| e.server_id != server_id);
    }

    /// Replace all entries for `server_id` with `new_entries` (removes existing first).
    pub fn upsert_server_entries(&mut self, server_id: &str, new_entries: Vec<VectorEntry>) {
        self.remove_server_entries(server_id);
        self.entries.extend(new_entries);
    }
}

#[derive(Debug)]
pub enum VectorIndexError {
    Io(std::io::Error),
    Parse(serde_json::Error),
    Serialize(serde_json::Error),
}

impl std::fmt::Display for VectorIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VectorIndexError::Io(e) => write!(f, "I/O error: {}", e),
            VectorIndexError::Parse(e) => write!(f, "Parse error: {}", e),
            VectorIndexError::Serialize(e) => write!(f, "Serialize error: {}", e),
        }
    }
}

impl std::error::Error for VectorIndexError {}
