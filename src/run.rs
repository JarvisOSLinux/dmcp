//! Run MCP servers.
//!
//! - **stdio**: Spawns the process with config injected as env vars, relays stdin/stdout.
//! - **SSE/WebSocket**: Prints the connection URL (server is already running remotely).
//!
//! Config keys are passed directly as environment variables using the key name stored
//! in the manifest (e.g. GITHUB_PERSONAL_ACCESS_TOKEN, BRAVE_API_KEY). This matches
//! what upstream servers expect based on their configurableProperties definitions.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::discovery::{get_manifest_path, get_server};
use crate::models::{Manifest, Transport};
use crate::paths::Paths;

/// Errors from `run`.
#[derive(Debug)]
pub enum RunError {
    ServerNotFound(String),
    NoTransports,
    NoStdioTransport,
    CommandNotFound(String),
    SpawnFailed(io::Error),
    ProcessExited(i32),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::ServerNotFound(id) => write!(f, "Server not found: {}", id),
            RunError::NoTransports => write!(f, "No transports defined for this server"),
            RunError::NoStdioTransport => write!(
                f,
                "Server has no stdio transport (remote servers: use the printed URL to connect)"
            ),
            RunError::CommandNotFound(cmd) => write!(f, "Command not found: {}", cmd),
            RunError::SpawnFailed(e) => write!(f, "Failed to spawn process: {}", e),
            RunError::ProcessExited(code) => write!(f, "Process exited with code {}", code),
        }
    }
}

impl std::error::Error for RunError {}

/// Run an installed MCP server by id.
///
/// - **stdio**: Spawns the process, injects config as env vars, inherits stdin/stdout/stderr.
/// - **SSE/WebSocket**: Prints "<name> is running on <url>" and exits.
pub fn run(paths: &Paths, id: &str, _verbose: bool) -> Result<(), RunError> {
    let (manifest, _scope) =
        get_server(paths, id).ok_or_else(|| RunError::ServerNotFound(id.to_string()))?;

    let transports = manifest
        .transports
        .as_deref()
        .ok_or(RunError::NoTransports)?;
    let primary = transports.first().ok_or(RunError::NoTransports)?;

    match primary {
        Transport::Stdio { command, args, .. } => {
            run_stdio(paths, &manifest, id, command, args.as_deref())
        }
        Transport::Sse { url, .. } => run_remote(&manifest, "SSE", url),
        Transport::WebSocket { ws_url, .. } => run_remote(&manifest, "WebSocket", ws_url),
    }
}

fn run_stdio(
    paths: &Paths,
    manifest: &Manifest,
    id: &str,
    command: &str,
    args: Option<&[String]>,
) -> Result<(), RunError> {
    let install_dir = manifest
        .install_dir
        .as_deref()
        .map(Path::new)
        .filter(|p| p.is_absolute())
        .map(|p| p.to_path_buf())
        .or_else(|| get_manifest_path(paths, id).and_then(|p| p.parent().map(|p| p.to_path_buf())))
        .ok_or(RunError::NoStdioTransport)?;

    let env = config_to_env(&manifest.config);

    let args: Vec<&str> = args
        .map(|a| a.iter().map(String::as_str).collect())
        .unwrap_or_default();

    let mut child = match Command::new(command)
        .args(&args)
        .current_dir(&install_dir)
        .envs(env)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(RunError::CommandNotFound(command.to_string()));
        }
        Err(e) => return Err(RunError::SpawnFailed(e)),
    };

    let status = child.wait().map_err(RunError::SpawnFailed)?;
    if let Some(code) = status.code() {
        if code != 0 {
            return Err(RunError::ProcessExited(code));
        }
    }

    Ok(())
}

/// Convert manifest config to env vars using the key name as-is.
/// Config keys in configurableProperties ARE the expected env var names
/// (e.g. GITHUB_PERSONAL_ACCESS_TOKEN, BRAVE_API_KEY).
pub fn config_to_env(
    config: &std::collections::HashMap<String, serde_json::Value>,
) -> HashMap<String, std::ffi::OsString> {
    let mut env = HashMap::new();
    for (key, value) in config {
        let env_val = match value {
            serde_json::Value::String(s) => s.clone(),
            _ => value.to_string(),
        };
        env.insert(key.clone(), std::ffi::OsString::from(env_val));
    }
    env
}

fn run_remote(manifest: &Manifest, transport_name: &str, url: &str) -> Result<(), RunError> {
    let name = manifest
        .name
        .as_deref()
        .unwrap_or(manifest.id.as_deref().unwrap_or("MCP Server"));
    println!("{} is running on {} ({})", name, url, transport_name);
    Ok(())
}
