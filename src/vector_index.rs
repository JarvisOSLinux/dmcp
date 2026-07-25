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
    /// Platforms the registry vouches for, copied from the entry at sync time.
    /// Absent means unrestricted — which is also what an index synced before
    /// this field existed reads as, until `dmcp sync-index` runs again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
    /// The registry entry declared `platforms`, but not as an array of platform
    /// names. Stored rather than resolved so the verdict is computed against the
    /// host that is searching, not the host that synced.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub platforms_malformed: bool,
}

impl VectorEntry {
    /// Whether `host` is outside what this entry vouches for. A declaration the
    /// sync could not read is carried as a flag rather than a value, and still
    /// has to read as unsupported: a gate that cannot parse its input must not
    /// conclude "no restriction", exactly as in `browse`.
    pub(crate) fn unsupported_on(&self, host: &str) -> bool {
        self.platforms_malformed
            || !crate::platform::PlatformDecl::from_names(self.platforms.as_deref().unwrap_or(&[]))
                .supports(host)
    }
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
    /// Platforms the registry vouches for. Absent means unrestricted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
    /// True when `platforms` excludes this host. Always serialized, for the same
    /// reason as in `browse`: `--json` vector search is the discovery surface the
    /// agent actually reaches through JARVIS, and an agent that cannot see this
    /// keeps proposing servers that cannot run here.
    pub unsupported_on_host: bool,
    /// The entry declares `platforms`, but not as an array of platform names, so
    /// it vouches for nothing readable. Omitted for every well-formed entry.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub platforms_malformed: bool,
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
    ///
    /// The host verdict is decided here rather than at sync time, so an index
    /// copied between machines — or synced before a host was re-imaged — still
    /// answers for the machine doing the searching.
    pub fn search(&self, query: &[f32], top_k: usize, min_score: f32) -> Vec<SearchResult> {
        let host = crate::platform::host_platform();
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
                platforms: entry.platforms.clone(),
                unsupported_on_host: entry.unsupported_on(host),
                platforms_malformed: entry.platforms_malformed,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{foreign_platform, host_platform};

    fn entry(platforms: Option<Vec<String>>, malformed: bool) -> VectorEntry {
        VectorEntry {
            server_id: "com.example.mcp.thing".to_string(),
            server_name: "Thing".to_string(),
            server_description: Some("Does a thing".to_string()),
            tool_name: None,
            tool_description: None,
            parameter_schema: None,
            vector: vec![1.0, 0.0],
            source: "registry".to_string(),
            platforms,
            platforms_malformed: malformed,
        }
    }

    fn index(entries: Vec<VectorEntry>) -> VectorIndex {
        VectorIndex {
            entries,
            ..Default::default()
        }
    }

    fn search_one(entry: VectorEntry) -> SearchResult {
        let index = index(vec![entry]);
        let mut results = index.search(&[1.0, 0.0], 5, 0.0);
        assert_eq!(results.len(), 1);
        results.remove(0)
    }

    /// The `--json` payload of `dmcp browse --vector` is what JARVIS's
    /// search_servers hands the agent: the flag must be present whether or not
    /// the entry declares platforms, exactly as in the keyword browse.
    #[test]
    fn json_output_always_carries_the_host_verdict() {
        let marked = serde_json::to_value(search_one(entry(
            Some(vec![foreign_platform().to_string()]),
            false,
        )))
        .unwrap();
        assert_eq!(marked["unsupported_on_host"], serde_json::json!(true));
        assert_eq!(marked["platforms"], serde_json::json!([foreign_platform()]));

        let plain = serde_json::to_value(search_one(entry(None, false))).unwrap();
        assert_eq!(plain["unsupported_on_host"], serde_json::json!(false));
        assert!(
            plain.get("platforms").is_none(),
            "an undeclared list stays absent rather than becoming an empty one"
        );
        assert!(
            plain.get("platforms_malformed").is_none(),
            "the flag is noise on every well-formed entry"
        );
    }

    #[test]
    fn an_entry_vouching_for_this_host_is_not_marked() {
        let r = search_one(entry(Some(vec![host_platform().to_string()]), false));
        assert!(!r.unsupported_on_host);
        assert_eq!(
            r.platforms.as_deref(),
            Some([host_platform().to_string()].as_slice())
        );
    }

    /// A declaration the sync could not read vouches for nothing, so searching
    /// must report it as unsupported rather than as unrestricted.
    #[test]
    fn a_malformed_declaration_is_reported_as_unsupported() {
        let r = search_one(entry(None, true));
        assert!(r.unsupported_on_host);
        assert!(r.platforms_malformed);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["unsupported_on_host"], serde_json::json!(true));
        assert_eq!(json["platforms_malformed"], serde_json::json!(true));
    }

    /// Locally indexed entries (`dmcp index-server`) have no registry entry to
    /// read platforms from, and an index synced before the field existed has
    /// none either: absent stays unrestricted, byte for byte today's behavior.
    #[test]
    fn an_index_without_platforms_still_loads_and_reads_as_unrestricted() {
        let legacy = serde_json::json!({
            "entries": [{
                "server_id": "com.example.mcp.legacy",
                "server_name": "Legacy",
                "vector": [1.0, 0.0],
                "source": "registry",
            }]
        });
        let index: VectorIndex = serde_json::from_value(legacy).unwrap();
        let results = index.search(&[1.0, 0.0], 5, 0.0);
        assert_eq!(results.len(), 1);
        assert!(!results[0].unsupported_on_host);
        assert_eq!(results[0].platforms, None);
    }

    /// The verdict follows the searching host, not the syncing one, so a copied
    /// index does not carry a stale answer.
    #[test]
    fn the_verdict_is_computed_per_host_not_stored() {
        let e = entry(Some(vec![foreign_platform().to_string()]), false);
        assert!(e.unsupported_on(host_platform()));
        assert!(!e.unsupported_on(foreign_platform()));
    }

    /// Batch search is the `browse_servers_batch` path; it must mark the same
    /// way the single-query one does.
    #[test]
    fn batch_search_marks_results_too() {
        let index = index(vec![entry(
            Some(vec![foreign_platform().to_string()]),
            false,
        )]);
        let batch = index.search_batch(&[vec![1.0, 0.0], vec![0.0, 1.0]], 5, 0.5);
        assert!(batch[0][0].unsupported_on_host);
        assert!(batch[1].is_empty(), "min_score still applies per query");
    }
}
