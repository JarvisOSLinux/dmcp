//! Server configuration (get/set).

use crate::discovery::get_manifest_path;
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
    std::fs::write(&manifest_path, output)
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
