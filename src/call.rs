//! Call tools on MCP servers.
//!
//! Connects to a server via its transport (stdio, SSE, WebSocket) and invokes tools.

use std::path::{Path, PathBuf};

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use tokio::process::Command;

use crate::discovery::{get_manifest_path, get_server, Scope};
use crate::elevation::{is_elevated, re_exec_with_pkexec};
use crate::models::{Manifest, Transport};
use crate::paths::Paths;
use crate::run::config_to_env;

/// Errors from calling tools.
#[derive(Debug)]
pub enum CallError {
    ServerNotFound(String),
    NoTransports,
    NoTransportForHost(crate::transport::NoTransportForHost),
    NoStdioTransport,
    SystemScopeRequiresElevation(String),
    RemoteNotSupported(String),
    ConnectionFailed(String),
    ToolCallFailed(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::ServerNotFound(id) => write!(f, "Server not found: {}", id),
            CallError::NoTransports => write!(f, "No transports defined"),
            CallError::NoTransportForHost(e) => write!(f, "{}", e),
            CallError::NoStdioTransport => write!(f, "Server has no stdio transport"),
            CallError::SystemScopeRequiresElevation(id) => write!(
                f,
                "System-scope server '{}' cannot run on the unprivileged agent \
                 surface: `dmcp serve` does not elevate, so its tools would \
                 execute as the invoking user instead of root. Run it through \
                 the `dmcp call` CLI (which elevates via polkit), or use a \
                 user-scope server.",
                id
            ),
            CallError::RemoteNotSupported(t) => {
                write!(f, "Remote transport not yet supported: {}", t)
            }
            CallError::ConnectionFailed(e) => write!(f, "Connection failed: {}", e),
            CallError::ToolCallFailed(e) => write!(f, "Tool call failed: {}", e),
        }
    }
}

impl std::error::Error for CallError {}

impl From<crate::transport::SelectError> for CallError {
    fn from(e: crate::transport::SelectError) -> Self {
        match e {
            crate::transport::SelectError::Missing => CallError::NoTransports,
            crate::transport::SelectError::ForeignHost(detail) => {
                CallError::NoTransportForHost(detail)
            }
        }
    }
}

/// True when invoking a tool requires re-executing dmcp as root: the target is a
/// system-scoped stdio server (its process — and therefore every command it
/// runs — must be privileged) and we are not already elevated. User-scope
/// servers and remote (SSE/WebSocket) transports never elevate.
///
/// The transport passed here is the one this host will actually launch, not
/// entry zero: a manifest that pairs a remote endpoint for one platform with a
/// stdio launch line for another would otherwise decide elevation from a
/// transport nobody is about to spawn.
fn needs_system_elevation(scope: Scope, selected: Option<&Transport>, elevated: bool) -> bool {
    !elevated && scope == Scope::System && matches!(selected, Some(Transport::Stdio { .. }))
}

/// Refuse to run a system-scope stdio server from an unprivileged process,
/// rather than silently executing it at the wrong uid (#45).
///
/// The CLI elevates before it ever reaches tool execution
/// (`elevate_call_for_system_scope` re-execs through pkexec), so by the time
/// `call_tool` runs there it is already root and this passes. The agent surface
/// (`dmcp serve`, and the orchestrator behind `dispatch_tasks`) cannot elevate
/// — it is a long-lived, possibly headless process, so it can neither re-exec
/// through pkexec (that would replace the daemon) nor raise a per-call polkit
/// prompt. There the only safe answer is a clear refusal: the system server's
/// tools would otherwise run as the invoking user, a confusing wrong-uid failure
/// the agent cannot diagnose, or — worse, if the daemon were run as root to
/// "fix" that — every user-scope tool would silently gain root too, exactly the
/// blast radius the scope split exists to prevent. This mirrors how the serve
/// surface already refuses source-mutating installs it cannot safely perform.
///
/// The same predicate that decides the CLI's pkexec re-exec decides the refusal,
/// so the two surfaces cannot drift on what "needs root" means.
fn refuse_unelevated_system_stdio(
    scope: Scope,
    selected: &Transport,
    elevated: bool,
    id: &str,
) -> Result<(), CallError> {
    if needs_system_elevation(scope, Some(selected), elevated) {
        return Err(CallError::SystemScopeRequiresElevation(id.to_string()));
    }
    Ok(())
}

/// Elevate a `dmcp call` on a system-scoped stdio server to root, mirroring
/// `dmcp run`: re-exec via pkexec so polkit authorizes it under
/// `org.jarvisos.dmcp.run-system-server`. Without this, a system server's tools
/// execute as the invoking user (e.g. `pacman` -> "you cannot perform this
/// operation unless you are root"), defeating the point of system scope (#33).
///
/// Call this from the `Call` command handler *before* the async runtime, so the
/// re-exec replaces the whole one-shot invocation; the elevated dmcp then spawns
/// the server as root and its stdout (the tool result) flows back to the caller.
/// Introspection (`list_tools`) is intentionally left unprivileged — it doesn't
/// run commands and the daemon calls it constantly.
pub fn elevate_call_for_system_scope(paths: &Paths, id: &str) {
    if let Some((manifest, scope)) = get_server(paths, id) {
        let selected = crate::transport::select(manifest.transports.as_deref()).ok();
        if needs_system_elevation(scope, selected, is_elevated()) {
            re_exec_with_pkexec();
        }
    }
}

/// Resolve the working directory for a stdio server: the manifest's absolute
/// `install_dir` when set, else the directory containing its manifest.json.
/// Shared by the one-shot call path, `dmcp run`, and the session broker so all
/// three spawn servers with the same cwd.
pub fn resolve_stdio_install_dir(paths: &Paths, manifest: &Manifest, id: &str) -> Option<PathBuf> {
    manifest
        .install_dir
        .as_deref()
        .map(Path::new)
        .filter(|p| p.is_absolute())
        .map(|p| p.to_path_buf())
        .or_else(|| get_manifest_path(paths, id).and_then(|p| p.parent().map(|p| p.to_path_buf())))
}

/// Build the tokio `Command` for a stdio MCP server: program, args, working
/// directory, and config-derived env. Factored out of the one-shot call path so
/// the session broker spawns servers with byte-identical semantics; the one-shot
/// and session paths must not drift in how a server is launched.
pub fn build_stdio_command(
    paths: &Paths,
    manifest: &Manifest,
    id: &str,
    command: &str,
    args: Option<&[String]>,
) -> Result<Command, CallError> {
    let install_dir =
        resolve_stdio_install_dir(paths, manifest, id).ok_or(CallError::NoStdioTransport)?;
    let env = config_to_env(&manifest.config);
    let mut cmd = Command::new(command);
    let args: Vec<&str> = args
        .map(|a| a.iter().map(String::as_str).collect())
        .unwrap_or_default();
    cmd.args(&args).current_dir(&install_dir).envs(env);
    Ok(cmd)
}

/// Call a tool on an installed MCP server.
pub async fn call_tool(
    paths: &Paths,
    id: &str,
    tool_name: &str,
    arguments: Option<serde_json::Value>,
) -> Result<CallToolResult, CallError> {
    let (manifest, scope) =
        get_server(paths, id).ok_or_else(|| CallError::ServerNotFound(id.to_string()))?;

    let primary = crate::transport::select(manifest.transports.as_deref())?;

    // Never silently run a system-scope stdio server unprivileged. The CLI has
    // already re-exec'd as root by now; an unelevated caller (the agent surface)
    // is refused here instead (#45).
    refuse_unelevated_system_stdio(scope, primary, is_elevated(), id)?;

    match primary {
        Transport::Stdio { command, args, .. } => {
            call_tool_stdio(
                paths,
                &manifest,
                id,
                command,
                args.as_deref(),
                tool_name,
                arguments,
            )
            .await
        }
        Transport::Sse { url, .. } => call_tool_remote(url, "sse", tool_name, arguments).await,
        Transport::WebSocket { ws_url, .. } => {
            call_tool_remote(ws_url, "websocket", tool_name, arguments).await
        }
    }
}

async fn call_tool_stdio(
    paths: &Paths,
    manifest: &Manifest,
    id: &str,
    command: &str,
    args: Option<&[String]>,
    tool_name: &str,
    arguments: Option<serde_json::Value>,
) -> Result<CallToolResult, CallError> {
    let cmd = build_stdio_command(paths, manifest, id, command, args)?;

    let transport =
        TokioChildProcess::new(cmd).map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

    let client = ().serve(transport).await.map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

    let args_obj = arguments
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    let result = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: tool_name.to_string().into(),
            arguments: if args_obj.is_empty() {
                None
            } else {
                Some(args_obj)
            },
            task: None,
        })
        .await
        .map_err(|e| CallError::ToolCallFailed(e.to_string()))?;

    client.cancel().await.ok();

    Ok(result)
}

async fn call_tool_remote(
    url: &str,
    _transport_type: &str,
    tool_name: &str,
    arguments: Option<serde_json::Value>,
) -> Result<CallToolResult, CallError> {
    use rmcp::transport::StreamableHttpClientTransport;
    use std::sync::Arc;

    let transport = StreamableHttpClientTransport::from_uri(Arc::from(url));

    let client = ().serve(transport).await.map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

    let args_obj = arguments
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    let result = client
        .call_tool(CallToolRequestParams {
            meta: None,
            name: tool_name.to_string().into(),
            arguments: if args_obj.is_empty() {
                None
            } else {
                Some(args_obj)
            },
            task: None,
        })
        .await
        .map_err(|e| CallError::ToolCallFailed(e.to_string()))?;

    client.cancel().await.ok();

    Ok(result)
}

/// List tools available on a server.
pub async fn list_tools(paths: &Paths, id: &str) -> Result<Vec<rmcp::model::Tool>, CallError> {
    let (manifest, _) =
        get_server(paths, id).ok_or_else(|| CallError::ServerNotFound(id.to_string()))?;

    let primary = crate::transport::select(manifest.transports.as_deref())?;

    match primary {
        Transport::Stdio { command, args, .. } => {
            list_tools_stdio(paths, &manifest, id, command, args.as_deref()).await
        }
        Transport::Sse { url, .. } => list_tools_remote(url).await,
        Transport::WebSocket { ws_url, .. } => list_tools_remote(ws_url).await,
    }
}

async fn list_tools_stdio(
    paths: &Paths,
    manifest: &Manifest,
    id: &str,
    command: &str,
    args: Option<&[String]>,
) -> Result<Vec<rmcp::model::Tool>, CallError> {
    let cmd = build_stdio_command(paths, manifest, id, command, args)?;

    let transport =
        TokioChildProcess::new(cmd).map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

    let client = ().serve(transport).await.map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

    let tools = client
        .list_tools(Default::default())
        .await
        .map_err(|e| CallError::ToolCallFailed(e.to_string()))?;

    client.cancel().await.ok();

    Ok(tools.tools)
}

async fn list_tools_remote(url: &str) -> Result<Vec<rmcp::model::Tool>, CallError> {
    use rmcp::transport::StreamableHttpClientTransport;
    use std::sync::Arc;

    let transport = StreamableHttpClientTransport::from_uri(Arc::from(url));

    let client = ().serve(transport).await.map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

    let tools = client
        .list_tools(Default::default())
        .await
        .map_err(|e| CallError::ToolCallFailed(e.to_string()))?;

    client.cancel().await.ok();

    Ok(tools.tools)
}

/// Format CallToolResult content for display.
///
/// Error status is deliberately NOT encoded in this string. A tool-reported
/// error is signalled out-of-band via a non-zero process exit code (see the
/// `Call` command handler), never a sentinel inside the output — otherwise tool
/// output containing that sentinel could spoof the call's status.
pub fn format_call_result(result: &CallToolResult) -> String {
    let mut out = String::new();
    for c in &result.content {
        if let Some(t) = c.as_text() {
            out.push_str(&t.text);
        }
    }
    out
}

/// Whether a tool call reported an error via rmcp's structured `is_error` flag.
pub fn call_is_error(result: &CallToolResult) -> bool {
    result.is_error.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::PlatformDecl;
    use rmcp::model::Content;

    fn stdio() -> Vec<Transport> {
        vec![Transport::Stdio {
            command: "srv".into(),
            args: None,
            description: None,
            platforms: PlatformDecl::Absent,
        }]
    }

    fn selected(transports: &[Transport]) -> Option<&Transport> {
        crate::transport::select(Some(transports)).ok()
    }

    #[test]
    fn system_stdio_elevates_when_not_root() {
        assert!(needs_system_elevation(
            Scope::System,
            selected(&stdio()),
            false
        ));
    }

    #[test]
    fn already_root_does_not_re_elevate() {
        // Guards against a pkexec re-exec loop: once elevated, never again.
        assert!(!needs_system_elevation(
            Scope::System,
            selected(&stdio()),
            true
        ));
    }

    #[test]
    fn user_scope_never_elevates() {
        assert!(!needs_system_elevation(
            Scope::User,
            selected(&stdio()),
            false
        ));
    }

    #[test]
    fn remote_system_server_does_not_elevate() {
        let sse = vec![Transport::Sse {
            url: "http://example".into(),
            description: None,
            platforms: PlatformDecl::Absent,
        }];
        assert!(!needs_system_elevation(
            Scope::System,
            selected(&sse),
            false
        ));
    }

    /// Elevation follows the transport this host launches. A remote endpoint
    /// listed first for another platform must not talk dmcp out of the pkexec
    /// re-exec the local stdio server needs.
    #[test]
    fn elevation_follows_the_host_selected_transport() {
        let transports = vec![
            Transport::Sse {
                url: "http://example".into(),
                description: None,
                platforms: PlatformDecl::Declared(vec![
                    crate::platform::foreign_platform().to_string()
                ]),
            },
            Transport::Stdio {
                command: "srv".into(),
                args: None,
                description: None,
                platforms: PlatformDecl::Declared(vec![
                    crate::platform::host_platform().to_string()
                ]),
            },
        ];
        assert!(needs_system_elevation(
            Scope::System,
            selected(&transports),
            false
        ));
    }

    /// Nothing runnable here: no transport, so nothing to elevate for. The call
    /// itself then reports the refusal.
    #[test]
    fn no_transport_for_this_host_does_not_elevate() {
        let transports = vec![Transport::Stdio {
            command: "srv".into(),
            args: None,
            description: None,
            platforms: PlatformDecl::Declared(
                vec![crate::platform::foreign_platform().to_string()],
            ),
        }];
        assert!(selected(&transports).is_none());
        assert!(!needs_system_elevation(
            Scope::System,
            selected(&transports),
            false
        ));
    }

    /// The refusal reaches the caller as a call error naming the platforms, not
    /// as a spawn failure for a command that was never meant to run here.
    #[test]
    fn select_error_maps_to_a_call_error_naming_the_platforms() {
        let transports = vec![Transport::Stdio {
            command: "srv".into(),
            args: None,
            description: None,
            platforms: PlatformDecl::Declared(
                vec![crate::platform::foreign_platform().to_string()],
            ),
        }];
        let err: CallError = crate::transport::select(Some(&transports))
            .unwrap_err()
            .into();
        assert!(matches!(err, CallError::NoTransportForHost(_)));
        assert!(err
            .to_string()
            .contains(crate::platform::foreign_platform()));

        let missing: CallError = crate::transport::select(None).unwrap_err().into();
        assert!(matches!(missing, CallError::NoTransports));
    }

    /// The agent surface is unprivileged, so a system-scope stdio server is
    /// refused there with the dedicated error — never run at the wrong uid (#45).
    #[test]
    fn agent_surface_refuses_unelevated_system_stdio() {
        let transports = stdio();
        let err = refuse_unelevated_system_stdio(
            Scope::System,
            selected(&transports).unwrap(),
            false,
            "sys.server",
        )
        .unwrap_err();
        assert!(
            matches!(err, CallError::SystemScopeRequiresElevation(ref id) if id == "sys.server")
        );
        let msg = err.to_string();
        assert!(msg.contains("sys.server"));
        assert!(msg.contains("agent surface"));
    }

    /// A serve deliberately started as root is already elevated — root is the
    /// right uid for a system tool, so it proceeds (matches the CLI outcome).
    #[test]
    fn elevated_process_runs_system_stdio() {
        let transports = stdio();
        assert!(refuse_unelevated_system_stdio(
            Scope::System,
            selected(&transports).unwrap(),
            true,
            "sys.server",
        )
        .is_ok());
    }

    /// User-scope tools are unaffected by the refusal — the whole point of the
    /// scope split is that they never need root.
    #[test]
    fn agent_surface_allows_unelevated_user_stdio() {
        let transports = stdio();
        assert!(refuse_unelevated_system_stdio(
            Scope::User,
            selected(&transports).unwrap(),
            false,
            "user.server",
        )
        .is_ok());
    }

    /// A remote (SSE/WebSocket) system server holds no local process to run as
    /// root, so it is not refused even unprivileged — consistent with the CLI,
    /// which never elevates for a remote transport.
    #[test]
    fn agent_surface_allows_unelevated_system_remote() {
        let sse = vec![Transport::Sse {
            url: "http://example".into(),
            description: None,
            platforms: PlatformDecl::Absent,
        }];
        assert!(refuse_unelevated_system_stdio(
            Scope::System,
            selected(&sse).unwrap(),
            false,
            "sys.remote",
        )
        .is_ok());
    }

    #[test]
    fn success_result_is_plain_output_with_no_error_status() {
        let ok = CallToolResult::success(vec![Content::text("hello")]);
        assert_eq!(format_call_result(&ok), "hello");
        assert!(!call_is_error(&ok));
    }

    #[test]
    fn error_result_omits_the_sentinel_and_is_flagged_structurally() {
        let err = CallToolResult::error(vec![Content::text("boom")]);
        // The output stream is the tool's content only — no "(Error)" sentinel
        // that tool output could otherwise forge.
        let out = format_call_result(&err);
        assert_eq!(out, "boom");
        assert!(!out.contains("(Error)"));
        // The error is carried structurally instead (surfaced via exit code).
        assert!(call_is_error(&err));
    }
}
