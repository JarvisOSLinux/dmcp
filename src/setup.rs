//! Run setup scripts for MCP servers.
//!
//! Setup scripts can install dependencies, configure the environment, or (for remote servers)
//! prepare connection info. They run with MCP_CONFIG_* and MCP_INSTALL_DIR in the environment.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::Command;

/// Errors from running setup.
#[derive(Debug)]
pub enum SetupError {
    NoSetupScript,
    ScriptNotFound(String),
    FetchFailed(String),
    SpawnFailed(io::Error),
    SetupFailed(i32),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::NoSetupScript => write!(f, "No setup script defined"),
            SetupError::ScriptNotFound(path) => write!(f, "Setup script not found: {}", path),
            SetupError::FetchFailed(msg) => write!(f, "Failed to fetch setup script: {}", msg),
            SetupError::SpawnFailed(e) => write!(f, "Failed to run setup script: {}", e),
            SetupError::SetupFailed(code) => write!(f, "Setup script exited with code {}", code),
        }
    }
}

impl std::error::Error for SetupError {}

/// Run the setup script for a server.
///
/// - `setup_script`: Path (relative to install_dir) or URL (http/https)
/// - `install_dir`: Working directory for the script
/// - `config`: Manifest config, injected as MCP_CONFIG_* env vars
pub fn run_setup(
    setup_script: &str,
    install_dir: &Path,
    config: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<(), SetupError> {
    let script_path = if setup_script.starts_with("http://") || setup_script.starts_with("https://")
    {
        fetch_script(setup_script)?
    } else {
        let path = install_dir.join(setup_script);
        if !path.exists() {
            return Err(SetupError::ScriptNotFound(
                path.to_string_lossy().to_string(),
            ));
        }
        path
    };

    let env = build_env(install_dir, config);

    let status = Command::new("sh")
        .arg(&script_path)
        .current_dir(install_dir)
        .envs(env)
        .status()
        .map_err(SetupError::SpawnFailed)?;

    if let Some(code) = status.code() {
        if code != 0 {
            return Err(SetupError::SetupFailed(code));
        }
    }

    Ok(())
}

fn build_env(
    install_dir: &Path,
    config: &std::collections::HashMap<String, serde_json::Value>,
) -> HashMap<String, OsString> {
    let mut env = HashMap::new();
    env.insert(
        "MCP_INSTALL_DIR".to_string(),
        OsString::from(install_dir.to_string_lossy().as_ref()),
    );
    for (key, value) in config {
        let env_key = format!(
            "MCP_CONFIG_{}",
            key.to_uppercase().replace('-', "_").replace('.', "_")
        );
        let env_val = match value {
            serde_json::Value::String(s) => s.clone(),
            _ => value.to_string(),
        };
        env.insert(env_key, OsString::from(env_val));
    }
    env
}

fn fetch_script(url: &str) -> Result<std::path::PathBuf, SetupError> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("dmcp/1.0")
        .build()
        .map_err(|e| SetupError::FetchFailed(e.to_string()))?;

    let resp = client
        .get(url)
        .send()
        .map_err(|e| SetupError::FetchFailed(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(SetupError::FetchFailed(format!("HTTP {}", resp.status())));
    }

    let body = resp
        .bytes()
        .map_err(|e| SetupError::FetchFailed(e.to_string()))?;

    let temp = std::env::temp_dir().join(format!("dmcp-setup-{}.sh", std::process::id()));
    std::fs::write(&temp, &body).map_err(|e| SetupError::FetchFailed(e.to_string()))?;

    Ok(temp)
}
