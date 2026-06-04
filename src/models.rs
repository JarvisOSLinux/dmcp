//! Data structures for index and manifest files.

use serde::{Deserialize, Serialize};

/// Index file at `<base>/mcp/installed/index.json`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Index {
    pub servers: std::collections::HashMap<String, IndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    /// Path to manifest.json. Alias "manifest" for guide compatibility.
    #[serde(alias = "manifest")]
    pub location: String,
    #[serde(default)]
    pub keywords: Vec<String>,
}

/// Manifest file at `<install_dir>/manifest.json`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub id: Option<String>,
    pub name: Option<String>,
    pub summary: Option<String>,
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    pub transports: Option<Vec<Transport>>,
    #[serde(default)]
    pub config: std::collections::HashMap<String, serde_json::Value>,
    pub install_dir: Option<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub tools: Vec<serde_json::Value>,
    /// Filename (local) or URL (remote). Run at install to prepare environment.
    #[serde(default)]
    pub setup_script: Option<String>,
    /// Local path after install (written by dmcp).
    #[serde(default)]
    pub setup_script_path: Option<String>,
    /// Timestamp of last setup run.
    #[serde(default)]
    pub setup_script_run_at: Option<String>,
    /// Version of setup script.
    #[serde(default)]
    pub setup_script_version: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub source: Option<serde_json::Value>,
    /// Schema for user-facing configuration fields. Parsed by installers/wrappers
    /// (e.g. JARVIS) to collect values before setup. dmcp itself stores the values
    /// in `config` and injects them as env vars — it does not validate against this schema.
    #[serde(default)]
    pub configurable_properties: Vec<ConfigurableProperty>,
}

/// One user-facing configuration field declared by a server manifest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurableProperty {
    /// Environment variable name passed to the server process (e.g. "BRAVE_API_KEY").
    pub key: String,
    /// Human-readable field name shown in configuration UIs.
    #[serde(default)]
    pub label: Option<String>,
    /// Help text explaining the value and where to obtain it.
    #[serde(default)]
    pub description: Option<String>,
    /// If true, mask the value in UIs and logs.
    #[serde(default)]
    pub sensitive: bool,
    /// If true, the server cannot start without this value being set.
    #[serde(default)]
    pub required: bool,
    /// Pre-filled value used when no saved value exists.
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Transport {
    Stdio {
        command: String,
        args: Option<Vec<String>>,
        #[serde(default)]
        description: Option<String>,
    },
    Sse {
        url: String,
        #[serde(default)]
        description: Option<String>,
    },
    #[serde(rename = "websocket")]
    WebSocket {
        #[serde(rename = "wsUrl")]
        ws_url: String,
        #[serde(default)]
        description: Option<String>,
    },
}
