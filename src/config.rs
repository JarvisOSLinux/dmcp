//! Server configuration (get/set).

use crate::discovery::get_manifest_path;
use crate::manifest_io::{write_manifest_atomic, Readers};
use crate::paths::Paths;

/// Set a config value for a server. Persists to manifest.json.
/// Uses raw JSON to preserve all manifest fields.
pub fn set_config_value(
    paths: &Paths,
    id: &str,
    key: &str,
    value: &str,
) -> Result<(), SetConfigError> {
    let manifest_path = get_manifest_path(paths, id).ok_or(SetConfigError::ServerNotFound)?;

    let content = std::fs::read_to_string(&manifest_path).map_err(SetConfigError::ReadFailed)?;
    let mut manifest: serde_json::Value =
        serde_json::from_str(&content).map_err(SetConfigError::ParseFailed)?;

    // Ensure config object exists
    if manifest.get("config").is_none() {
        manifest["config"] = serde_json::json!({});
    }

    let config = manifest
        .get_mut("config")
        .and_then(|c| c.as_object_mut())
        .ok_or(SetConfigError::InvalidManifest)?;

    config.insert(
        key.to_string(),
        serde_json::Value::String(value.to_string()),
    );

    let output =
        serde_json::to_string_pretty(&manifest).map_err(SetConfigError::SerializeFailed)?;
    // This is the write that put a plaintext token on disk: it must not be able
    // to lose the manifest it replaces, nor leave the value world-readable.
    let readers = Readers::for_manifest_path(paths, &manifest_path);
    write_manifest_atomic(&manifest_path, output.as_bytes(), readers)
        .map_err(|e| SetConfigError::WriteFailed(e, manifest_path.clone()))?;

    Ok(())
}

#[derive(Debug)]
pub enum SetConfigError {
    ServerNotFound,
    InvalidManifest,
    ReadFailed(std::io::Error),
    ParseFailed(serde_json::Error),
    SerializeFailed(serde_json::Error),
    WriteFailed(std::io::Error, std::path::PathBuf),
}

impl std::fmt::Display for SetConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetConfigError::ServerNotFound => write!(f, "Server not found"),
            SetConfigError::InvalidManifest => write!(f, "Manifest has no config object"),
            SetConfigError::ReadFailed(e) => write!(f, "Failed to read manifest: {}", e),
            SetConfigError::ParseFailed(e) => write!(f, "Failed to parse manifest: {}", e),
            SetConfigError::SerializeFailed(e) => write!(f, "Failed to serialize manifest: {}", e),
            SetConfigError::WriteFailed(e, _) => write!(f, "Failed to write manifest: {}", e),
        }
    }
}

impl std::error::Error for SetConfigError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Temp tree removed on drop; keeps the install tree off the real XDG paths.
    struct TempTree {
        root: std::path::PathBuf,
    }

    impl TempTree {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let root =
                std::env::temp_dir().join(format!("dmcp-config-test-{}-{}", std::process::id(), n));
            std::fs::create_dir_all(&root).unwrap();
            TempTree { root }
        }

        fn paths(&self) -> Paths {
            Paths {
                user_sources: self.root.join("user/sources.list"),
                user_install_dir: self.root.join("user/installed"),
                system_sources: self.root.join("system/sources.list"),
                system_install_dir: self.root.join("system/installed"),
                vector_index_dir: self.root.join("vector"),
            }
        }

        /// An already-installed server whose manifest was written the old way —
        /// group- and world-readable — so `config set` is exercised against the
        /// state a real machine is in, not only against a fresh install.
        fn installed_server(&self, id: &str) -> std::path::PathBuf {
            let dir = self.root.join("user/installed").join(id);
            std::fs::create_dir_all(&dir).unwrap();
            let manifest_path = dir.join("manifest.json");
            std::fs::write(
                &manifest_path,
                serde_json::json!({ "id": id, "config": { "ENDPOINT": "https://example.invalid" } })
                    .to_string(),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&manifest_path, std::fs::Permissions::from_mode(0o644))
                    .unwrap();
            }
            let index = serde_json::json!({
                "servers": { id: { "location": manifest_path.to_string_lossy(), "keywords": [] } }
            });
            std::fs::write(
                self.root.join("user/installed/index.json"),
                index.to_string(),
            )
            .unwrap();
            manifest_path
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn set_config_value_persists_the_value_and_leaves_no_second_copy() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let manifest_path = tree.installed_server("test.server");

        set_config_value(&paths, "test.server", "GITHUB_TOKEN", "ghp_secret").unwrap();

        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(written["config"]["GITHUB_TOKEN"], "ghp_secret");
        assert_eq!(
            written["config"]["ENDPOINT"], "https://example.invalid",
            "the rewrite must preserve every other field"
        );

        let left_behind: Vec<String> = std::fs::read_dir(manifest_path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n != "manifest.json")
            .collect();
        assert!(
            left_behind.is_empty(),
            "a scratch copy of the token must not survive the write: {left_behind:?}"
        );
    }

    /// The reported incident was a PAT written here at mode 644. A write is the
    /// one moment dmcp gets to set the mode, so it sets it even when the file it
    /// replaces was looser.
    #[cfg(unix)]
    #[test]
    fn set_config_value_writes_the_manifest_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let tree = TempTree::new();
        let paths = tree.paths();
        let manifest_path = tree.installed_server("test.server");

        set_config_value(&paths, "test.server", "GITHUB_TOKEN", "ghp_secret").unwrap();

        let mode = std::fs::metadata(&manifest_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the stored token must not be world-readable");
    }
}
