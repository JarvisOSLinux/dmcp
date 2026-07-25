//! Run MCP servers.
//!
//! - **stdio**: Spawns the process with config injected as env vars, relays stdin/stdout.
//! - **SSE/WebSocket**: Prints the connection URL (server is already running remotely).
//!
//! Config keys are passed directly as environment variables using the key name stored
//! in the manifest (e.g. GITHUB_PERSONAL_ACCESS_TOKEN, BRAVE_API_KEY). This matches
//! what upstream servers expect based on their configurableProperties definitions.
//!
//! ## System-scoped servers
//!
//! When the server is installed in system scope (`/usr/share/mcp/installed/`) and the
//! current process is not already running as root, dmcp uses `pkexec` to re-execute
//! itself under the polkit action `org.jarvisos.dmcp.run-system-server`.  This keeps
//! the privilege-escalation path auditable and avoids hardcoded `sudoers` NOPASSWD
//! entries or setuid wrappers.
//!
//! See `policy/org.jarvisos.dmcp.policy` for the polkit action definition.

use std::collections::HashMap;
use std::io;
use std::process::{Command, Stdio};

use crate::discovery::{get_server, Scope};
use crate::elevation::{is_elevated, re_exec_with_pkexec};
use crate::models::{Manifest, Transport};
use crate::paths::Paths;

/// Errors from `run`.
#[derive(Debug)]
pub enum RunError {
    ServerNotFound(String),
    NoTransports,
    NoTransportForHost(crate::transport::NoTransportForHost),
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
            RunError::NoTransportForHost(e) => write!(f, "{}", e),
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

impl From<crate::transport::SelectError> for RunError {
    fn from(e: crate::transport::SelectError) -> Self {
        match e {
            crate::transport::SelectError::Missing => RunError::NoTransports,
            crate::transport::SelectError::ForeignHost(detail) => {
                RunError::NoTransportForHost(detail)
            }
        }
    }
}

/// Run an installed MCP server by id.
///
/// - **stdio**: Spawns the process, injects config as env vars, inherits stdin/stdout/stderr.
/// - **SSE/WebSocket**: Prints "<name> is running on <url>" and exits.
///
/// When the server is system-scoped and the current process is not already
/// root, this re-executes dmcp via `pkexec` so that polkit handles privilege
/// escalation under the `org.jarvisos.dmcp.run-system-server` action.
pub fn run(paths: &Paths, id: &str, _verbose: bool) -> Result<(), RunError> {
    let (manifest, scope) =
        get_server(paths, id).ok_or_else(|| RunError::ServerNotFound(id.to_string()))?;

    // System-scoped stdio servers need root to access /usr/share/mcp/.
    // Re-execute dmcp through pkexec so polkit can authenticate the user.
    // Remote transports (SSE/WebSocket) just print a URL and need no root, and
    // a transport declared for another platform is not the one being spawned —
    // so the decision reads the host-selected transport, not entry zero.
    if scope == Scope::System && !is_elevated() {
        if let Ok(Transport::Stdio { .. }) =
            crate::transport::select(manifest.transports.as_deref())
        {
            re_exec_with_pkexec();
        }
    }

    let primary = crate::transport::select(manifest.transports.as_deref())?;

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
    let install_dir = crate::call::resolve_stdio_install_dir(paths, manifest, id)
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
