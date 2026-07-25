//! Data structures for index and manifest files.

use serde::{Deserialize, Serialize};

use crate::platform::PlatformDecl;

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
    /// Windows counterpart of `setup_script` (e.g. `setup.ps1`), run through
    /// PowerShell. Absent means the POSIX script is all there is.
    #[serde(default)]
    pub setup_script_windows: Option<String>,
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
    /// When true, the server holds state in-process across tool calls (a browser,
    /// a REPL, a DB connection), so a fresh process per call would lose it. Such
    /// servers are eligible for session-scoped (`--session`) calls kept alive by
    /// the broker. Absent or false means stateless — the default one-shot path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stateful: Option<bool>,
    /// Platforms the registry vouches for: `"linux"`, `"darwin"`, `"windows"`.
    /// Absent means unrestricted — dmcp installs and runs it on any host.
    ///
    /// Read through `PlatformDecl`, which never fails: a manifest whose only
    /// defect is this field still loads, so a slip cannot hide an installed
    /// server from `list`, `info` and `uninstall` alike. What it cannot do is
    /// pass the gate — an unreadable declaration covers no host.
    #[serde(default, skip_serializing_if = "PlatformDecl::is_absent")]
    pub platforms: PlatformDecl,
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
        /// Platforms this launch line is for: `"linux"`, `"darwin"`, `"windows"`.
        /// Absent matches every host, which is what keeps every pre-`platforms`
        /// manifest launching exactly as before.
        #[serde(default, skip_serializing_if = "PlatformDecl::is_absent")]
        platforms: PlatformDecl,
    },
    Sse {
        url: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "PlatformDecl::is_absent")]
        platforms: PlatformDecl,
    },
    #[serde(rename = "websocket")]
    WebSocket {
        #[serde(rename = "wsUrl")]
        ws_url: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "PlatformDecl::is_absent")]
        platforms: PlatformDecl,
    },
}

impl Transport {
    /// Platforms this transport is declared for. The declaration is returned as
    /// read — including "present but unreadable", which covers no host — so the
    /// typed and raw-JSON views of the same manifest select the same transport.
    pub fn platforms(&self) -> &PlatformDecl {
        match self {
            Transport::Stdio { platforms, .. }
            | Transport::Sse { platforms, .. }
            | Transport::WebSocket { platforms, .. } => platforms,
        }
    }
}

impl Manifest {
    /// The setup script to run on `host`: the Windows variant when there is one,
    /// the POSIX field otherwise. See `crate::setup::script_for_host`.
    pub fn setup_script_for_host(&self, host: &str) -> Option<&str> {
        crate::setup::script_for_host(
            host,
            self.setup_script_windows.as_deref(),
            self.setup_script.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_with(platforms: &str) -> Manifest {
        let json = format!(
            r#"{{"id":"com.test.s","transports":[{{"type":"stdio","command":"py"}}],
                 "platforms":{}}}"#,
            platforms
        );
        serde_json::from_str(&json).expect("a platforms slip must not sink the whole manifest")
    }

    /// The failure this forecloses: an installed server whose manifest no longer
    /// parses is invisible to `list`, `info`, `run` and `uninstall` alike — on
    /// disk, unusable, and unremovable through the CLI.
    #[test]
    fn a_malformed_platforms_value_still_parses_and_covers_no_host() {
        for value in [
            r#""windows""#,
            r#"{"windows":true}"#,
            r#"[123]"#,
            r#"["linux", 5]"#,
            r#"[["linux"]]"#,
        ] {
            let manifest = manifest_with(value);
            assert!(
                manifest.platforms.is_malformed(),
                "{value} must read as malformed"
            );
            assert!(
                !manifest
                    .platforms
                    .supports(crate::platform::host_platform()),
                "{value} must not pass the gate"
            );
            assert_eq!(manifest.id.as_deref(), Some("com.test.s"));
            assert!(manifest.transports.is_some(), "the rest still loads");
        }
    }

    #[test]
    fn a_well_formed_platforms_value_reads_as_declared() {
        let manifest = manifest_with(r#"["linux"]"#);
        assert_eq!(
            manifest.platforms.names(),
            Some(["linux".to_string()].as_slice())
        );

        // Empty and blank-only lists keep the documented "absent" reading.
        assert!(manifest_with("[]").platforms.is_absent());
        assert!(manifest_with(r#"["", "  "]"#).platforms.is_absent());
        assert!(manifest_with("null").platforms.is_absent());
    }

    #[test]
    fn an_absent_platforms_field_is_not_written_back() {
        let manifest: Manifest = serde_json::from_str(r#"{"id":"com.test.s"}"#).unwrap();
        assert!(manifest.platforms.is_absent());
        let out = serde_json::to_string(&manifest).unwrap();
        assert!(
            !out.contains("platforms"),
            "an absent list must not be written back as null: {out}"
        );
    }

    /// Same leniency per transport, and the malformed value is preserved rather
    /// than rewritten when the manifest is saved again (`dmcp config set`).
    #[test]
    fn a_malformed_transport_platforms_value_still_parses() {
        let manifest: Manifest = serde_json::from_str(
            r#"{"id":"com.test.s",
                "transports":[{"type":"stdio","command":"py","platforms":"windows"}]}"#,
        )
        .expect("a per-transport platforms slip must not sink the manifest");
        let transports = manifest.transports.as_ref().unwrap();
        assert!(transports[0].platforms().is_malformed());
        assert!(!transports[0].platforms().supports("windows"));

        let out = serde_json::to_value(&manifest).unwrap();
        assert_eq!(
            out["transports"][0]["platforms"],
            serde_json::json!("windows")
        );
    }
}
