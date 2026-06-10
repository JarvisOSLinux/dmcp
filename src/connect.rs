//! Connect to remote (SSE/WebSocket) MCP servers by URL, without a registry.
//!
//! Tries to fetch manifest from URL first; if valid JSON with id+transports, uses it.
//! Otherwise falls back to treating URL as raw endpoint.

use std::time::Duration;

use crate::paths::Paths;
use crate::setup;

/// Connect to a remote MCP server. Tries to fetch manifest from URL; falls back to raw endpoint.
pub fn connect(
    paths: &Paths,
    url: &str,
    id_override: Option<&str>,
    name: Option<&str>,
    summary: Option<&str>,
    version: Option<&str>,
    config: &[(String, String)],
    scope: crate::discovery::Scope,
    run_setup: bool,
) -> Result<String, ConnectError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(ConnectError::InvalidUrl);
    }

    if let Some(mut manifest) = try_fetch_manifest(url) {
        // Manifest mode: use fetched manifest, apply overrides
        let id = id_override
            .map(String::from)
            .or_else(|| {
                manifest
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .unwrap_or_else(|| {
                next_connected_server_id(paths, scope)
                    .unwrap_or_else(|_| "com.user.connected.server1".to_string())
            });

        let install_dir = match scope {
            crate::discovery::Scope::User => paths.user_install_dir().join(&id),
            crate::discovery::Scope::System => paths.system_install_dir().join(&id),
        };

        std::fs::create_dir_all(&install_dir).map_err(ConnectError::CreateDir)?;

        manifest["installDir"] =
            serde_json::Value::String(install_dir.to_string_lossy().to_string());
        manifest["id"] = serde_json::Value::String(id.clone());

        if let Some(n) = name {
            manifest["name"] = serde_json::Value::String(n.to_string());
        } else if manifest.get("name").is_none() {
            manifest["name"] = serde_json::Value::String(id.clone());
        }

        if let Some(s) = summary {
            manifest["summary"] = serde_json::Value::String(s.to_string());
        } else if manifest.get("summary").is_none() {
            manifest["summary"] =
                serde_json::Value::String("Connected via dmcp connect".to_string());
        }

        if let Some(v) = version {
            manifest["version"] = serde_json::Value::String(v.to_string());
        } else if manifest.get("version").is_none() {
            manifest["version"] = serde_json::Value::String("1.0.0".to_string());
        }

        // Merge config overrides
        let mut config_obj = manifest
            .get("config")
            .and_then(|c| c.as_object().cloned())
            .unwrap_or_default();
        for (k, v) in config {
            config_obj.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
        manifest["config"] = serde_json::Value::Object(config_obj);

        let manifest_path = install_dir.join("manifest.json");
        let output = serde_json::to_string_pretty(&manifest).map_err(ConnectError::Serialize)?;
        std::fs::write(&manifest_path, output).map_err(ConnectError::WriteManifest)?;

        // Run setup script if present
        if run_setup {
            if let Some(setup_script) = manifest
                .get("setupScript")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                let config_map = manifest
                    .get("config")
                    .and_then(|c| c.as_object())
                    .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                if let Err(e) = setup::run_setup(setup_script, &install_dir, &config_map) {
                    return Err(ConnectError::SetupFailed(e.to_string()));
                }
            }
        }

        let keywords: Vec<String> = manifest
            .get("keywords")
            .and_then(|k| k.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        crate::install::update_index_add(paths, &id, &manifest_path, scope, &keywords)
            .map_err(|e| ConnectError::IndexError(e.to_string()))?;

        return Ok(id);
    }

    // Raw fallback: treat URL as endpoint
    connect_raw(
        paths,
        url,
        id_override,
        name,
        summary,
        version,
        config,
        scope,
    )
}

/// Try to fetch URL as JSON manifest. Returns Some if valid (has id and transports).
fn try_fetch_manifest(url: &str) -> Option<serde_json::Value> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("dmcp/1.0")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()
        .ok()?;

    let resp = client.get(url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let manifest: serde_json::Value = resp.json().ok()?;
    let id = manifest.get("id").and_then(|v| v.as_str())?;
    let transports = manifest.get("transports").and_then(|t| t.as_array())?;
    if id.is_empty() || transports.is_empty() {
        return None;
    }

    Some(manifest)
}

/// Raw endpoint mode: infer transport from URL, auto-generate metadata.
fn connect_raw(
    paths: &Paths,
    url: &str,
    id_override: Option<&str>,
    name: Option<&str>,
    summary: Option<&str>,
    version: Option<&str>,
    config: &[(String, String)],
    scope: crate::discovery::Scope,
) -> Result<String, ConnectError> {
    let transport_type = if url.starts_with("wss://") || url.starts_with("ws://") {
        "websocket"
    } else {
        "sse"
    };

    let id = id_override.map(String::from).unwrap_or_else(|| {
        next_connected_server_id(paths, scope)
            .unwrap_or_else(|_| "com.user.connected.server1".into())
    });

    let install_dir = match scope {
        crate::discovery::Scope::User => paths.user_install_dir().join(&id),
        crate::discovery::Scope::System => paths.system_install_dir().join(&id),
    };

    std::fs::create_dir_all(&install_dir).map_err(ConnectError::CreateDir)?;

    let transport = if transport_type == "websocket" {
        serde_json::json!({
            "type": "websocket",
            "wsUrl": url
        })
    } else {
        serde_json::json!({
            "type": "sse",
            "url": url
        })
    };

    let mut config_obj = serde_json::Map::new();
    for (k, v) in config {
        config_obj.insert(k.clone(), serde_json::Value::String(v.clone()));
    }

    let manifest = serde_json::json!({
        "id": id,
        "name": name.unwrap_or(&id),
        "summary": summary.unwrap_or("Connected via dmcp connect"),
        "version": version.unwrap_or("1.0.0"),
        "transports": [transport],
        "installDir": install_dir.to_string_lossy(),
        "config": config_obj
    });

    let manifest_path = install_dir.join("manifest.json");
    let output = serde_json::to_string_pretty(&manifest).map_err(ConnectError::Serialize)?;
    std::fs::write(&manifest_path, output).map_err(ConnectError::WriteManifest)?;

    crate::install::update_index_add(paths, &id, &manifest_path, scope, &[])
        .map_err(|e| ConnectError::IndexError(e.to_string()))?;

    Ok(id)
}

fn next_connected_server_id(
    paths: &Paths,
    scope: crate::discovery::Scope,
) -> Result<String, ConnectError> {
    let index_path = match scope {
        crate::discovery::Scope::User => paths.user_install_dir().join("index.json"),
        crate::discovery::Scope::System => paths.system_install_dir().join("index.json"),
    };

    let content = std::fs::read_to_string(&index_path)
        .unwrap_or_else(|_| r#"{"servers":{},"version":"1.0"}"#.to_string());
    let index: serde_json::Value =
        serde_json::from_str(&content).map_err(ConnectError::ParseIndex)?;

    let empty = serde_json::Map::new();
    let servers = index
        .get("servers")
        .and_then(|s| s.as_object())
        .unwrap_or(&empty);

    let mut max_n = 0u32;
    for (id, _) in servers {
        if let Some(n) = id
            .strip_prefix("com.user.connected.server")
            .and_then(|s| s.parse::<u32>().ok())
        {
            if n > max_n {
                max_n = n;
            }
        }
    }

    Ok(format!("com.user.connected.server{}", max_n + 1))
}

#[derive(Debug)]
pub enum ConnectError {
    InvalidUrl,
    CreateDir(std::io::Error),
    Serialize(serde_json::Error),
    WriteManifest(std::io::Error),
    SetupFailed(String),
    ParseIndex(serde_json::Error),
    IndexError(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::InvalidUrl => write!(f, "Invalid or empty URL"),
            ConnectError::CreateDir(e) => write!(f, "Failed to create directory: {}", e),
            ConnectError::Serialize(e) => write!(f, "Failed to serialize: {}", e),
            ConnectError::WriteManifest(e) => write!(f, "Failed to write manifest: {}", e),
            ConnectError::SetupFailed(s) => write!(f, "Setup failed: {}", s),
            ConnectError::ParseIndex(e) => write!(f, "Failed to parse index: {}", e),
            ConnectError::IndexError(s) => write!(f, "{}", s),
        }
    }
}

impl std::error::Error for ConnectError {}
