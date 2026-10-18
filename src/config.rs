//! Server configuration (get/set).

use crate::discovery::get_manifest_path;
use crate::manifest_io::{write_manifest_atomic, Readers};
use crate::paths::Paths;

/// Set a config value for a server. Persists to manifest.json.
/// Uses raw JSON to preserve all manifest fields.
///
/// Only a setting the manifest declares in `configurableProperties` is
/// accepted. A key reaches the server as an environment variable of that name,
/// and the caller may be an agent; NODE_OPTIONS or LD_PRELOAD set here would be
/// code execution in the server.
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

    let props: Vec<serde_json::Value> = manifest
        .get("configurableProperties")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let Some(prop) = props
        .iter()
        .find(|p| p.get("key").and_then(|k| k.as_str()) == Some(key))
    else {
        return Err(SetConfigError::Undeclared {
            key: key.to_string(),
            declared: props
                .iter()
                .filter_map(|p| p.get("key")?.as_str().map(str::to_string))
                .collect(),
        });
    };
    if let Some(why) = value_error(prop, value) {
        return Err(SetConfigError::Invalid {
            key: key.to_string(),
            why,
        });
    }

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

/// Why `value` is not valid for the setting `prop`, by its `type` (mcp-registry#115).
///
/// Mirrors `property_value_error` in the registry's validator. A setting with
/// no `type`, or one this dmcp does not know, takes any string: the registry
/// gate is where types are checked, and refusing a newer type here would
/// strand a server whose manifest is newer than dmcp.
pub fn value_error(prop: &serde_json::Value, value: &str) -> Option<String> {
    let strings = |field: &str| -> Vec<String> {
        prop.get(field)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    match prop
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("string")
    {
        "boolean" => {
            let spelled = |which: &str| {
                prop.get("values")
                    .and_then(|v| v.get(which))
                    .and_then(|v| v.as_str())
                    .unwrap_or(which)
                    .to_string()
            };
            let (yes, no) = (spelled("true"), spelled("false"));
            (value != yes && value != no).then(|| format!("must be '{yes}' or '{no}'"))
        }
        "integer" => {
            let digits = value.strip_prefix('-').unwrap_or(value);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Some("must be a whole number".to_string());
            }
            let Ok(number) = value.parse::<i64>() else {
                return Some("is out of range".to_string());
            };
            if let Some(min) = prop.get("min").and_then(|m| m.as_i64()) {
                if number < min {
                    return Some(format!("must be at least {min}"));
                }
            }
            if let Some(max) = prop.get("max").and_then(|m| m.as_i64()) {
                if number > max {
                    return Some(format!("must be at most {max}"));
                }
            }
            None
        }
        "choice" => {
            let choices = strings("choices");
            let multiple = prop.get("multiple").and_then(|m| m.as_bool()) == Some(true);
            let separator = prop
                .get("separator")
                .and_then(|s| s.as_str())
                .unwrap_or(",");
            let items: Vec<&str> = if multiple {
                value.split(separator).collect()
            } else {
                vec![value]
            };
            let bad = value.is_empty() || items.iter().any(|i| !choices.iter().any(|c| c == i));
            bad.then(|| {
                let how = if multiple {
                    "each of its items must be"
                } else {
                    "must be"
                };
                format!("{how} one of {}", choices.join(", "))
            })
        }
        _ => None,
    }
}

#[derive(Debug)]
pub enum SetConfigError {
    ServerNotFound,
    Undeclared { key: String, declared: Vec<String> },
    Invalid { key: String, why: String },
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
            SetConfigError::Undeclared { key, declared } if declared.is_empty() => {
                write!(f, "{key} is not a setting of this server; it declares none")
            }
            SetConfigError::Invalid { key, why } => write!(f, "{key} {why}"),
            SetConfigError::Undeclared { key, declared } => write!(
                f,
                "{key} is not a setting of this server; its settings are: {}",
                declared.join(", ")
            ),
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
    use serde_json::json;
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
                serde_json::json!({
                    "id": id,
                    "configurableProperties": [{"key": "ENDPOINT"}, {"key": "GITHUB_TOKEN"}],
                    "config": { "ENDPOINT": "https://example.invalid" },
                })
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

    #[test]
    fn a_setting_the_manifest_does_not_declare_is_refused() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let manifest_path = tree.installed_server("test.server");
        let before = std::fs::read(&manifest_path).unwrap();

        let err =
            set_config_value(&paths, "test.server", "NODE_OPTIONS", "--import=x").unwrap_err();

        assert!(matches!(err, SetConfigError::Undeclared { .. }), "{err}");
        assert_eq!(
            err.to_string(),
            "NODE_OPTIONS is not a setting of this server; its settings are: ENDPOINT, GITHUB_TOKEN"
        );
        assert_eq!(std::fs::read(&manifest_path).unwrap(), before);
    }

    #[test]
    fn a_value_is_checked_against_the_setting_type() {
        let cases = [
            (json!({"type": "boolean"}), "true", None),
            (
                json!({"type": "boolean"}),
                "yes",
                Some("must be 'true' or 'false'"),
            ),
            (
                json!({"type": "boolean", "values": {"true": "1", "false": "0"}}),
                "0",
                None,
            ),
            (
                json!({"type": "boolean", "values": {"true": "1", "false": "0"}}),
                "true",
                Some("must be '1' or '0'"),
            ),
            (
                json!({"type": "integer", "min": 1, "max": 65535}),
                "993",
                None,
            ),
            (
                json!({"type": "integer", "min": 1, "max": 65535}),
                "0",
                Some("must be at least 1"),
            ),
            (
                json!({"type": "integer"}),
                "3.5",
                Some("must be a whole number"),
            ),
            (
                json!({"type": "integer"}),
                "",
                Some("must be a whole number"),
            ),
            (
                json!({"type": "choice", "choices": ["ssl", "starttls"]}),
                "ssl",
                None,
            ),
            (
                json!({"type": "choice", "choices": ["ssl", "starttls"]}),
                "tls",
                Some("must be one of ssl, starttls"),
            ),
            (
                json!({"type": "choice", "multiple": true, "choices": ["mail", "calendar"]}),
                "mail,calendar",
                None,
            ),
            (
                json!({"type": "choice", "multiple": true, "choices": ["mail", "calendar"]}),
                "mail,,calendar",
                Some("each of its items must be one of mail, calendar"),
            ),
            (json!({}), "anything at all", None),
            (json!({"type": "from-the-future"}), "anything", None),
        ];
        for (prop, value, expected) in cases {
            assert_eq!(
                value_error(&prop, value).as_deref(),
                expected,
                "{prop} with {value:?}"
            );
        }
    }

    #[test]
    fn an_invalid_value_is_refused_and_nothing_is_written() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let manifest_path = tree.installed_server("test.server");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["configurableProperties"]
            .as_array_mut()
            .unwrap()
            .push(json!({"key": "READ_ONLY", "type": "boolean"}));
        std::fs::write(&manifest_path, manifest.to_string()).unwrap();
        let before = std::fs::read(&manifest_path).unwrap();

        let err = set_config_value(&paths, "test.server", "READ_ONLY", "yes").unwrap_err();
        assert_eq!(err.to_string(), "READ_ONLY must be 'true' or 'false'");
        assert_eq!(std::fs::read(&manifest_path).unwrap(), before);
        set_config_value(&paths, "test.server", "READ_ONLY", "true").unwrap();
    }
}
