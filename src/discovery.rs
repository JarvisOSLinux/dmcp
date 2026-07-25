//! Discovery of installed MCP servers.

use std::collections::HashMap;
use std::path::Path;

use crate::doc_comments;
use crate::models::{Index, Manifest};
use crate::paths::Paths;

/// Info about an installed server for display.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ServerInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub transport_type: String,
    pub scope: Scope,
    /// Path to manifest.json (or install dir for backwards compatibility in JSON)
    #[serde(alias = "install_dir")]
    pub manifest_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    User,
    System,
}

/// List installed servers from the given scopes.
/// User takes precedence over system for duplicate IDs.
pub fn list_servers(paths: &Paths, user: bool, system: bool, debug: bool) -> Vec<ServerInfo> {
    let mut seen = HashMap::new();

    if user {
        if let Some(servers) = load_from_scope(paths.user_install_dir(), Scope::User, debug) {
            for s in servers {
                seen.insert(s.id.clone(), s);
            }
        } else if debug {
            eprintln!("[debug] load_from_scope(user) returned None");
        }
    }

    if system {
        if let Some(servers) = load_from_scope(paths.system_install_dir(), Scope::System, debug) {
            for s in servers {
                seen.entry(s.id.clone()).or_insert(s);
            }
        } else if debug {
            eprintln!("[debug] load_from_scope(system) returned None");
        }
    }

    let mut result: Vec<_> = seen.into_values().collect();
    result.sort_by(|a, b| a.id.cmp(&b.id));
    result
}

fn load_from_scope(base: &Path, scope: Scope, debug: bool) -> Option<Vec<ServerInfo>> {
    let index_path = base.join("index.json");
    if debug {
        eprintln!("[debug] Reading index: {}", index_path.display());
    }
    let index: Index = match std::fs::read_to_string(&index_path) {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(idx) => idx,
            Err(e) => {
                if debug {
                    eprintln!("[debug] Index parse error: {}", e);
                }
                return None;
            }
        },
        Err(e) => {
            if debug {
                eprintln!("[debug] Index read error: {}", e);
            }
            return Some(vec![]);
        }
    };

    if debug {
        eprintln!("[debug] Index has {} servers", index.servers.len());
    }

    let mut servers = Vec::new();
    for (id, entry) in index.servers {
        let manifest_path = Path::new(&entry.location);
        if debug {
            eprintln!("[debug] Loading manifest: {}", manifest_path.display());
        }
        let manifest: Manifest = match std::fs::read_to_string(manifest_path) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(m) => m,
                Err(e) => {
                    if debug {
                        eprintln!("[debug] Manifest parse error for {}: {}", id, e);
                    }
                    continue;
                }
            },
            Err(e) => {
                if debug {
                    eprintln!("[debug] Manifest read error for {}: {}", id, e);
                }
                continue;
            }
        };

        // Report the transport this host would launch. Falling back to the first
        // entry keeps the listing informative for a server declared only for
        // other platforms, rather than blanking it out to "unknown".
        let transport_type = manifest
            .transports
            .as_ref()
            .and_then(|t| crate::transport::select(Some(t)).ok().or_else(|| t.first()))
            .map(transport_type_name)
            .unwrap_or_else(|| "unknown".to_string());

        servers.push(ServerInfo {
            id: manifest.id.unwrap_or(id),
            name: manifest.name.unwrap_or_else(|| "Unknown".to_string()),
            version: manifest.version.unwrap_or_else(|| "?".to_string()),
            transport_type,
            scope,
            manifest_path: entry.location.clone(),
        });
    }

    if debug {
        eprintln!(
            "[debug] Loaded {} servers from {:?} scope",
            servers.len(),
            scope
        );
    }

    Some(servers)
}

fn transport_type_name(t: &crate::models::Transport) -> String {
    match t {
        crate::models::Transport::Stdio { .. } => "stdio",
        crate::models::Transport::Sse { .. } => "sse",
        crate::models::Transport::WebSocket { .. } => "websocket",
    }
    .to_string()
}

/// Get a single server by id. User scope is checked first.
pub fn get_server(paths: &Paths, id: &str) -> Option<(Manifest, Scope)> {
    // Check user scope first
    if let Some((m, scope, _)) = load_server_from_scope(paths.user_install_dir(), id, Scope::User) {
        return Some((m, scope));
    }
    // Then system scope
    load_server_from_scope(paths.system_install_dir(), id, Scope::System).map(|(m, s, _)| (m, s))
}

/// Get the path to a server's manifest.json. User scope checked first.
pub fn get_manifest_path(paths: &Paths, id: &str) -> Option<std::path::PathBuf> {
    if let Some((_, _, path)) = load_server_from_scope(paths.user_install_dir(), id, Scope::User) {
        return Some(path);
    }
    load_server_from_scope(paths.system_install_dir(), id, Scope::System).map(|(_, _, p)| p)
}

/// Get manifest path, install dir, and scope for uninstall.
pub fn get_uninstall_info(
    paths: &Paths,
    id: &str,
) -> Option<(std::path::PathBuf, std::path::PathBuf, Scope)> {
    if let Some((_, scope, manifest_path)) =
        load_server_from_scope(paths.user_install_dir(), id, Scope::User)
    {
        let install_dir = manifest_path.parent()?.to_path_buf();
        return Some((manifest_path, install_dir, scope));
    }
    if let Some((_, scope, manifest_path)) =
        load_server_from_scope(paths.system_install_dir(), id, Scope::System)
    {
        let install_dir = manifest_path.parent()?.to_path_buf();
        return Some((manifest_path, install_dir, scope));
    }
    None
}

fn load_server_from_scope(
    base: &Path,
    id: &str,
    scope: Scope,
) -> Option<(Manifest, Scope, std::path::PathBuf)> {
    let index_path = base.join("index.json");
    let s = std::fs::read_to_string(&index_path).ok()?;
    let index: Index = serde_json::from_str(&s).ok()?;
    let entry = index.servers.get(id)?;
    let manifest_path = std::path::PathBuf::from(&entry.location);
    let s = std::fs::read_to_string(&manifest_path).ok()?;
    let mut manifest: Manifest = serde_json::from_str(&s).ok()?;
    enrich_manifest_from_doc_comments(&mut manifest, &manifest_path);
    Some((manifest, scope, manifest_path))
}

/// If the manifest has no `description` and no `summary`, scan the server's
/// install directory for Python `@mcp.tool` docstrings and use the first one
/// found as a description fallback.
fn enrich_manifest_from_doc_comments(manifest: &mut Manifest, manifest_path: &Path) {
    let has_description = manifest
        .description
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let has_summary = manifest
        .summary
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if has_description || has_summary {
        return;
    }

    // Derive install dir from the manifest path (parent directory).
    let install_dir = match manifest_path.parent() {
        Some(p) => p,
        None => return,
    };

    let docs = doc_comments::extract_tool_docs(install_dir);
    if let Some(desc) = doc_comments::first_description(&docs) {
        manifest.description = Some(desc);
    }
}
