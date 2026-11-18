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
    /// Where that transport's launch line runs, when not directly on the host
    /// (see `Execution::describe`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<String>,
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
                    // Warned about unconditionally: a server that is installed
                    // but unlistable is otherwise invisible, and a diagnostic
                    // only `--debug` prints is one nobody thinks to ask for.
                    eprintln!(
                        "[warn] {}: manifest at {} does not parse ({}); \
                         the server is installed but hidden — `dmcp uninstall {}` removes it",
                        id,
                        manifest_path.display(),
                        e,
                        id
                    );
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
        let shown = manifest
            .transports
            .as_ref()
            .and_then(|t| crate::transport::select(Some(t)).ok().or_else(|| t.first()));
        let transport_type = shown
            .map(transport_type_name)
            .unwrap_or_else(|| "unknown".to_string());
        let execution = shown
            .and_then(crate::models::Transport::execution)
            .map(crate::execution::Execution::describe);

        servers.push(ServerInfo {
            id: manifest.id.unwrap_or(id),
            name: manifest.name.unwrap_or_else(|| "Unknown".to_string()),
            version: manifest.version.unwrap_or_else(|| "?".to_string()),
            transport_type,
            execution,
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
    load_server(paths, id).map(|(m, s, _)| (m, s))
}

/// Get the path to a server's manifest.json. User scope checked first.
pub fn get_manifest_path(paths: &Paths, id: &str) -> Option<std::path::PathBuf> {
    load_server(paths, id).map(|(_, _, p)| p)
}

/// The installed server `id`, user scope first.
///
/// The first scope whose index lists `id` answers, even when its manifest does
/// not load. Falling through to the next scope on a load failure would turn a
/// broken user-scope install into the same-id system-scope server, which runs
/// as root: a different server, at a different privilege, under the name the
/// caller asked for.
fn load_server(paths: &Paths, id: &str) -> Option<(Manifest, Scope, std::path::PathBuf)> {
    for (base, scope) in [
        (paths.user_install_dir(), Scope::User),
        (paths.system_install_dir(), Scope::System),
    ] {
        if indexed_manifest_path(base, id).is_some() {
            return load_server_from_scope(base, id, scope);
        }
    }
    None
}

/// Get manifest path, install dir, and scope for uninstall.
///
/// Read from the index, not from the manifest: removing a server must not
/// depend on its manifest still being readable. A manifest that fails to parse
/// is exactly when uninstall is needed, and requiring one to parse first is how
/// a server becomes installed, invisible, and unremovable at the same time.
pub fn get_uninstall_info(
    paths: &Paths,
    id: &str,
) -> Option<(std::path::PathBuf, std::path::PathBuf, Scope)> {
    for (base, scope) in [
        (paths.user_install_dir(), Scope::User),
        (paths.system_install_dir(), Scope::System),
    ] {
        if let Some(manifest_path) = indexed_manifest_path(base, id) {
            let install_dir = manifest_path.parent()?.to_path_buf();
            return Some((manifest_path, install_dir, scope));
        }
    }
    None
}

/// The manifest path index.json records for `id`, if the index lists it.
fn indexed_manifest_path(base: &Path, id: &str) -> Option<std::path::PathBuf> {
    let s = std::fs::read_to_string(base.join("index.json")).ok()?;
    let index: Index = serde_json::from_str(&s).ok()?;
    Some(std::path::PathBuf::from(&index.servers.get(id)?.location))
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(std::path::PathBuf);

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tree(name: &str) -> (Tree, Paths) {
        let root =
            std::env::temp_dir().join(format!("dmcp-discovery-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            user_sources: root.join("u.list"),
            user_install_dir: root.join("user"),
            system_sources: root.join("s.list"),
            system_install_dir: root.join("system"),
            vector_index_dir: root.join("v"),
        };
        (Tree(root), paths)
    }

    fn install_at(base: &Path, id: &str, manifest: &str) {
        let dir = base.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest_path = dir.join("manifest.json");
        std::fs::write(&manifest_path, manifest).unwrap();
        let index = serde_json::json!({
            "servers": {id: {"location": manifest_path.to_string_lossy()}}
        });
        std::fs::write(base.join("index.json"), index.to_string()).unwrap();
    }

    const GOOD: &str = r#"{"id": "srv", "transports": [{"type": "stdio", "command": "srv"}]}"#;
    const BROKEN: &str = r#"{"id": "srv", "transports": [{"type": "stdio", "command": "srv",
        "execution": {"type": "docker", "imgae": "python:3-slim"}}]}"#;

    #[test]
    fn a_user_install_that_does_not_load_does_not_resolve_to_the_system_one() {
        let (_t, paths) = tree("no-fallthrough");
        install_at(paths.user_install_dir(), "srv", BROKEN);
        install_at(paths.system_install_dir(), "srv", GOOD);

        assert!(get_server(&paths, "srv").is_none());
        assert!(get_manifest_path(&paths, "srv").is_none());
    }

    #[test]
    fn the_system_install_answers_when_user_scope_does_not_list_the_id() {
        let (_t, paths) = tree("system-only");
        install_at(paths.user_install_dir(), "other", GOOD);
        install_at(paths.system_install_dir(), "srv", GOOD);

        let (_, scope) = get_server(&paths, "srv").expect("system install");
        assert_eq!(scope, Scope::System);
    }
}
