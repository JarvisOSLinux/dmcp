//! Path resolution for user and system scope.
//!
//! Uses env vars when set (from .env if present), otherwise values from
//! .env.example, then XDG/hardcoded defaults.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Resolved paths for MCP directories.
#[derive(Debug, Clone)]
pub struct Paths {
    pub user_sources: PathBuf,
    pub user_install_dir: PathBuf,
    pub system_sources: PathBuf,
    pub system_install_dir: PathBuf,
    pub vector_index_dir: PathBuf,
}

/// Resolved paths rendered as absolute strings for machine consumers
/// (`dmcp paths --json`).
#[derive(Debug, serde::Serialize)]
pub struct PathsJson {
    /// Base directory that contains `installed/` and `vector_index/` — the
    /// parent of the user install dir. Downstream tooling reads this to locate
    /// installed server manifests on every OS, so it must stay the parent of
    /// `install_dir` rather than a hardcoded guess.
    pub data_dir: String,
    pub config_dir: String,
    pub install_dir: String,
    pub vector_index_dir: String,
    pub system_install_dir: String,
}

impl Paths {
    /// Resolve paths from environment, falling back to .env.example, then XDG/defaults.
    pub fn resolve() -> Self {
        let env_defaults = load_env_example_defaults();
        let user_sources = resolve_path(
            "MCP_USER_SOURCES_PATH",
            dirs::config_dir().map(|p| p.join("mcp/sources.list")),
            "~/.config/mcp/sources.list",
            &env_defaults,
        );
        let user_install_dir = resolve_path(
            "MCP_USER_INSTALL_DIR",
            dirs::data_local_dir().map(|p| p.join("mcp/installed")),
            "~/.local/share/mcp/installed/",
            &env_defaults,
        );
        let system_sources = resolve_path(
            "MCP_SYSTEM_SOURCES_PATH",
            Some(system_sources_default()),
            "/etc/mcp/sources.list",
            &env_defaults,
        );
        let system_install_dir = resolve_path(
            "MCP_SYSTEM_INSTALL_DIR",
            Some(system_install_dir_default()),
            "/usr/share/mcp/installed/",
            &env_defaults,
        );
        let vector_index_dir = resolve_path(
            "MCP_VECTOR_INDEX_DIR",
            dirs::data_local_dir().map(|p| p.join("mcp/vector_index")),
            "~/.local/share/mcp/vector_index",
            &env_defaults,
        );

        Self {
            user_sources,
            user_install_dir,
            system_sources,
            system_install_dir,
            vector_index_dir,
        }
    }

    /// User sources list path.
    pub fn user_sources_path(&self) -> &Path {
        &self.user_sources
    }

    /// User install directory (index + manifests).
    pub fn user_install_dir(&self) -> &Path {
        &self.user_install_dir
    }

    /// System sources list path.
    pub fn system_sources_path(&self) -> &Path {
        &self.system_sources
    }

    /// System install directory.
    pub fn system_install_dir(&self) -> &Path {
        &self.system_install_dir
    }

    /// Vector index directory (user-scope only).
    pub fn vector_index_dir(&self) -> &Path {
        &self.vector_index_dir
    }

    /// Path to the vector index JSON file.
    pub fn vector_index_path(&self) -> PathBuf {
        self.vector_index_dir.join("index.json")
    }

    /// Resolved paths for machine consumers. `data_dir` is derived as the parent
    /// of the user install dir (and `config_dir` as the parent of the user
    /// sources file) so an `MCP_USER_INSTALL_DIR` override moves both together
    /// instead of the JSON pointing at a stale default.
    pub fn as_json(&self) -> PathsJson {
        PathsJson {
            data_dir: parent_or_self(&self.user_install_dir),
            config_dir: parent_or_self(&self.user_sources),
            install_dir: self.user_install_dir.display().to_string(),
            vector_index_dir: self.vector_index_dir.display().to_string(),
            system_install_dir: self.system_install_dir.display().to_string(),
        }
    }
}

/// A path's parent as a string, falling back to the path itself when it has no
/// parent (e.g. a filesystem root or a bare filename), so a JSON value is never
/// empty.
fn parent_or_self(path: &Path) -> String {
    path.parent().unwrap_or(path).display().to_string()
}

fn resolve_path(
    env_var: &str,
    xdg_default: Option<PathBuf>,
    fallback: &str,
    env_defaults: &HashMap<String, String>,
) -> PathBuf {
    if let Ok(val) = std::env::var(env_var) {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return expand_tilde(trimmed);
        }
    }
    if let Some(val) = env_defaults.get(env_var) {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return expand_tilde(trimmed);
        }
    }
    xdg_default.unwrap_or_else(|| expand_tilde(fallback))
}

/// Per-OS default system sources path. Linux: `/etc/mcp/sources.list`. macOS:
/// under `/Library/Application Support` (not `/usr/share`, which is SIP-sealed
/// on macOS 13+). Windows: under `%ProgramData%`.
#[cfg(target_os = "linux")]
fn system_sources_default() -> PathBuf {
    PathBuf::from("/etc/mcp/sources.list")
}

#[cfg(target_os = "macos")]
fn system_sources_default() -> PathBuf {
    PathBuf::from("/Library/Application Support/mcp/sources.list")
}

#[cfg(target_os = "windows")]
fn system_sources_default() -> PathBuf {
    program_data_dir().join("mcp").join("sources.list")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn system_sources_default() -> PathBuf {
    PathBuf::from("/etc/mcp/sources.list")
}

/// Per-OS default system install dir. See [`system_sources_default`] for the
/// macOS SIP rationale.
#[cfg(target_os = "linux")]
fn system_install_dir_default() -> PathBuf {
    PathBuf::from("/usr/share/mcp/installed/")
}

#[cfg(target_os = "macos")]
fn system_install_dir_default() -> PathBuf {
    PathBuf::from("/Library/Application Support/mcp/installed/")
}

#[cfg(target_os = "windows")]
fn system_install_dir_default() -> PathBuf {
    program_data_dir().join("mcp").join("installed")
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn system_install_dir_default() -> PathBuf {
    PathBuf::from("/usr/share/mcp/installed/")
}

#[cfg(target_os = "windows")]
fn program_data_dir() -> PathBuf {
    std::env::var("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\ProgramData"))
}

/// Load default values from .env.example. Searches: cwd, XDG_CONFIG_HOME/mcp, /etc/dmcp.
///
/// Linux-only: on macOS/Windows a stray `.env.example` in the cwd (e.g. a git
/// checkout) would silently shadow the real per-OS system paths above.
#[cfg(target_os = "linux")]
fn load_env_example_defaults() -> HashMap<String, String> {
    let candidates = [
        std::env::current_dir().ok().map(|p| p.join(".env.example")),
        dirs::config_dir().map(|p| p.join("mcp/.env.example")),
        Some(PathBuf::from("/etc/dmcp/env.example")),
    ];
    for path in candidates.iter().flatten() {
        if path.exists() {
            if let Ok(map) = parse_env_file(path) {
                return map;
            }
        }
    }
    HashMap::new()
}

#[cfg(not(target_os = "linux"))]
fn load_env_example_defaults() -> HashMap<String, String> {
    HashMap::new()
}

fn parse_env_file(path: &Path) -> std::io::Result<HashMap<String, String>> {
    let content = std::fs::read_to_string(path)?;
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(eq) = line.find('=') {
            let key = line[..eq].trim();
            let val = line[eq + 1..].trim();
            if !key.is_empty() {
                map.insert(key.to_string(), val.to_string());
            }
        }
    }
    Ok(map)
}

fn expand_tilde(path: &str) -> PathBuf {
    let expanded = shellexpand::tilde(path);
    PathBuf::from(expanded.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_paths() -> Paths {
        Paths {
            user_sources: PathBuf::from("/home/u/.config/mcp/sources.list"),
            user_install_dir: PathBuf::from("/home/u/.local/share/mcp/installed"),
            system_sources: PathBuf::from("/etc/mcp/sources.list"),
            system_install_dir: PathBuf::from("/usr/share/mcp/installed"),
            vector_index_dir: PathBuf::from("/home/u/.local/share/mcp/vector_index"),
        }
    }

    #[test]
    fn as_json_data_dir_is_parent_of_install_dir() {
        let json = sample_paths().as_json();

        // The load-bearing invariant PJ relies on: data_dir is the parent of
        // install_dir and the base that contains installed/ and vector_index/.
        assert_eq!(json.install_dir, "/home/u/.local/share/mcp/installed");
        assert_eq!(json.data_dir, "/home/u/.local/share/mcp");
        assert_eq!(
            Path::new(&json.install_dir).parent().unwrap(),
            Path::new(&json.data_dir)
        );
        assert!(json.data_dir.ends_with("/mcp"));

        assert_eq!(json.config_dir, "/home/u/.config/mcp");
        assert_eq!(
            json.vector_index_dir,
            "/home/u/.local/share/mcp/vector_index"
        );
        assert_eq!(json.system_install_dir, "/usr/share/mcp/installed");
    }

    #[test]
    fn as_json_serializes_all_keys_as_absolute_strings() {
        let value = serde_json::to_value(sample_paths().as_json()).unwrap();
        let obj = value.as_object().unwrap();

        for key in [
            "data_dir",
            "config_dir",
            "install_dir",
            "vector_index_dir",
            "system_install_dir",
        ] {
            let s = obj.get(key).unwrap().as_str().unwrap();
            assert!(
                Path::new(s).is_absolute(),
                "{key} should be absolute, got {s}"
            );
        }
    }
}
