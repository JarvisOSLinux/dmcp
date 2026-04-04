//! Sync the local vector index from registry sources.
//!
//! Downloads all server/tool embeddings from configured registry sources
//! and stores them in the local vector index for semantic search.

use std::time::Duration;

use crate::install::rfc3339_now;
use crate::paths::Paths;
use crate::sources::list_sources;
use crate::vector_index::{EmbeddingSpec, VectorEntry, VectorIndex, VectorIndexError};

/// Result of a sync-index operation.
pub struct SyncResult {
    pub servers_indexed: usize,
    pub tools_indexed: usize,
    pub errors: Vec<String>,
}

#[derive(Debug)]
pub enum SyncError {
    VectorIndex(VectorIndexError),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::VectorIndex(e) => write!(f, "Vector index error: {}", e),
        }
    }
}

impl std::error::Error for SyncError {}

/// Download all server/tool embeddings from registry sources and update the local index.
///
/// - Replaces all `"registry"` entries with fresh data.
/// - Keeps `"local"` entries (from `dmcp index-server`) unchanged.
/// - If a registry is unreachable, reports a warning and continues.
/// - Updates `embedding_spec` from the first server that declares one.
pub fn sync_index(paths: &Paths) -> Result<SyncResult, SyncError> {
    let index_path = paths.vector_index_path();

    // Load existing index, keeping local entries
    let mut index = VectorIndex::load(&index_path).map_err(SyncError::VectorIndex)?;

    // Drop all registry entries; they will be re-fetched
    index.entries.retain(|e| e.source != "registry");

    let client = match reqwest::blocking::Client::builder()
        .user_agent("dmcp/1.0")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Ok(SyncResult {
                servers_indexed: 0,
                tools_indexed: 0,
                errors: vec![format!("Failed to build HTTP client: {}", e)],
            });
        }
    };

    let sources = list_sources(paths, true, true);
    let mut servers_indexed = 0usize;
    let mut tools_indexed = 0usize;
    let mut errors = Vec::new();

    for (url, _scope) in &sources {
        match fetch_embeddings_from_registry(&client, url) {
            Ok(fetched) => {
                for (server_id, new_entries, maybe_spec) in fetched {
                    // Adopt embedding spec from the first server that provides one
                    if index.embedding_spec.is_none() {
                        if let Some(spec) = maybe_spec {
                            index.embedding_spec = Some(spec);
                        }
                    }
                    let n_tools = new_entries
                        .iter()
                        .filter(|e| e.tool_name.is_some())
                        .count();
                    let n_server = new_entries.len() - n_tools;
                    servers_indexed += n_server;
                    tools_indexed += n_tools;
                    index.upsert_server_entries(&server_id, new_entries);
                }
            }
            Err(e) => {
                errors.push(format!("Warning: skipping {}: {}", url, e));
            }
        }
    }

    index.last_synced = Some(rfc3339_now());
    index.save(&index_path).map_err(SyncError::VectorIndex)?;

    Ok(SyncResult {
        servers_indexed,
        tools_indexed,
        errors,
    })
}

/// Fetch all servers with embeddings from one registry URL.
///
/// Returns a list of `(server_id, entries, embedding_spec)` tuples.
/// `embedding_spec` is derived from the embeddings block (model/version/dimensions).
fn fetch_embeddings_from_registry(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<Vec<(String, Vec<VectorEntry>, Option<EmbeddingSpec>)>, reqwest::Error> {
    let resp = client.get(url).send()?;
    if !resp.status().is_success() {
        return Err(resp.error_for_status().unwrap_err());
    }
    let registry: serde_json::Value = resp.json()?;

    let servers_array: Vec<serde_json::Value> = match registry.get("servers") {
        Some(s) if s.is_array() => s.as_array().unwrap().clone(),
        Some(s) if s.is_object() => s.as_object().unwrap().values().cloned().collect(),
        _ => return Ok(vec![]),
    };

    let mut result = Vec::new();

    for server in servers_array {
        let embeddings = match server.get("embeddings") {
            Some(e) if !e.is_null() => e,
            _ => continue, // no embeddings — skip, still keyword-searchable
        };

        let server_id = match server.get("id").and_then(|i| i.as_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };

        let server_name = server
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("?")
            .to_string();

        let server_description = server
            .get("description")
            .and_then(|d| d.as_str())
            .map(String::from)
            .or_else(|| {
                server
                    .get("summary")
                    .and_then(|s| s.as_str())
                    .map(String::from)
            });

        // Extract embedding spec from this server's embeddings block
        let maybe_spec = {
            let model = embeddings.get("model").and_then(|m| m.as_str());
            let version = embeddings.get("version").and_then(|v| v.as_str());
            match (model, version) {
                (Some(m), Some(v)) => {
                    // Infer dimensions from server vector if present
                    let dims = embeddings
                        .get("server")
                        .and_then(|s| s.as_array())
                        .map(|a| a.len())
                        .or_else(|| {
                            embeddings
                                .get("tools")
                                .and_then(|t| t.as_object())
                                .and_then(|obj| obj.values().next())
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                        })
                        .unwrap_or(0);
                    Some(EmbeddingSpec {
                        model: m.to_string(),
                        version: v.to_string(),
                        dimensions: dims,
                    })
                }
                _ => None,
            }
        };

        let mut entries = Vec::new();

        // Server-level vector
        if let Some(server_vec) = parse_vector(embeddings.get("server")) {
            entries.push(VectorEntry {
                server_id: server_id.clone(),
                server_name: server_name.clone(),
                server_description: server_description.clone(),
                tool_name: None,
                tool_description: None,
                parameter_schema: None,
                vector: server_vec,
                source: "registry".to_string(),
            });
        }

        // Tool-level vectors
        if let Some(tools_map) = embeddings.get("tools").and_then(|t| t.as_object()) {
            // Build lookup of tool details (description, params) from the tools array
            let tools_details: std::collections::HashMap<&str, &serde_json::Value> = server
                .get("tools")
                .and_then(|t| t.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| {
                            t.get("name")
                                .and_then(|n| n.as_str())
                                .map(|name| (name, t))
                        })
                        .collect()
                })
                .unwrap_or_default();

            for (tool_name, tool_vec_val) in tools_map {
                let Some(tool_vec) = parse_vector(Some(tool_vec_val)) else {
                    continue;
                };
                let tool_info = tools_details.get(tool_name.as_str());
                let tool_description = tool_info
                    .and_then(|t| t.get("description"))
                    .and_then(|d| d.as_str())
                    .map(String::from);
                let parameter_schema = tool_info.and_then(|t| t.get("params")).cloned();

                entries.push(VectorEntry {
                    server_id: server_id.clone(),
                    server_name: server_name.clone(),
                    server_description: server_description.clone(),
                    tool_name: Some(tool_name.clone()),
                    tool_description,
                    parameter_schema,
                    vector: tool_vec,
                    source: "registry".to_string(),
                });
            }
        }

        if !entries.is_empty() {
            result.push((server_id, entries, maybe_spec));
        }
    }

    Ok(result)
}

fn parse_vector(val: Option<&serde_json::Value>) -> Option<Vec<f32>> {
    let arr = val.and_then(|v| v.as_array())?;
    let vec: Vec<f32> = arr
        .iter()
        .filter_map(|v| v.as_f64().map(|f| f as f32))
        .collect();
    if vec.is_empty() { None } else { Some(vec) }
}
