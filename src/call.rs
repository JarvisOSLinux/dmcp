//! Call tools on MCP servers.
//!
//! Connects to a server via its transport (stdio, SSE, WebSocket) and invokes tools.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, Content};
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
    ElevationFailed(String, String),
    ElevationTimedOut(String, u64),
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
                "System-scope server '{}' needs root and no elevation path is \
                 available from here. Run it through the `dmcp call` CLI, or \
                 use a user-scope server.",
                id
            ),
            CallError::ElevationFailed(id, detail) => write!(
                f,
                "Elevation for system-scope server '{}' was not granted: {}. \
                 polkit denies a non-active session (SSH, headless, a system \
                 unit) outright and does not prompt; from a desktop session the \
                 prompt may have been dismissed.",
                id,
                detail.trim()
            ),
            CallError::ElevationTimedOut(id, secs) => write!(
                f,
                "Elevation for system-scope server '{}' went unanswered for {}s \
                 and was cancelled (set DMCP_ELEVATION_TIMEOUT_SECS to change). \
                 The authentication prompt was never completed.",
                id, secs
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

/// Set on a child spawned purely to elevate, so it can never spawn another.
/// If pkexec failed in the child, the answer is the refusal, not a second
/// attempt — without this a denial would recurse until the box gave out.
const DELEGATED_ENV: &str = "DMCP_ELEVATION_DELEGATED";

/// What an unelevated caller should do about the server it is about to invoke.
#[derive(Debug, PartialEq, Eq)]
enum ElevationPlan {
    /// Run it here: user scope, a remote transport, or we are already root.
    Direct,
    /// Spawn a child `dmcp call`, which re-execs through pkexec and prompts.
    Delegate,
    /// Elevation is needed but unreachable — report rather than mis-run.
    Refuse,
}

/// Decide how a system-scope stdio server gets its root (#45).
///
/// `dmcp serve` cannot elevate *itself* — re-execing through pkexec would
/// replace the daemon, and it may be headless. But it does not need to: it can
/// spawn a **child** `dmcp call`, which re-execs through pkexec on its own and
/// lets polkit raise the prompt (the action sets `allow_gui`, precisely so a
/// caller with no TTY can be answered graphically). The child runs the server as
/// root, prints the tool result, and exits — so the privilege dies with the
/// command instead of becoming a standing capability. This is the same shape
/// dispatch already uses to reach dmcp.
///
/// Two things must stay true. A child spawned to elevate never delegates again,
/// or a polkit denial would recurse. And when polkit refuses outright — it
/// denies a non-active session rather than prompting — the caller gets a clear
/// error instead of a tool silently running at the invoking user's uid.
///
/// The predicate that decides the CLI's own re-exec decides this too, so the two
/// surfaces cannot drift on what "needs root" means.
fn plan_elevation(
    scope: Scope,
    selected: Option<&Transport>,
    elevated: bool,
    already_delegated: bool,
) -> ElevationPlan {
    if !needs_system_elevation(scope, selected, elevated) {
        return ElevationPlan::Direct;
    }
    if already_delegated {
        return ElevationPlan::Refuse;
    }
    ElevationPlan::Delegate
}

/// Argv (after the program name) for the delegated `dmcp call`. Mirrors the
/// one-shot CLI invocation byte-for-byte — no `--session`, because an elevated
/// server must not outlive the command that needed it.
fn elevated_cli_args(
    id: &str,
    tool_name: &str,
    arguments: Option<&serde_json::Value>,
) -> Vec<String> {
    let mut args = vec!["call".to_string(), id.to_string(), tool_name.to_string()];
    if let Some(v) = arguments {
        if !v.is_null() && v != &serde_json::json!({}) {
            args.push("--args".to_string());
            args.push(v.to_string());
        }
    }
    args
}

/// How long to wait for the human to answer polkit before giving up.
fn elevation_timeout_secs() -> u64 {
    std::env::var("DMCP_ELEVATION_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|s| *s > 0)
        .unwrap_or(180)
}

/// Map the delegated CLI's exit into a tool result, mirroring how `dmcp call`
/// reports: 0 is success, 2 is a tool-reported error (its status rides the exit
/// code, never a sentinel in the output), anything else failed to elevate.
fn result_from_cli_exit(
    code: Option<i32>,
    stdout: String,
    stderr: String,
    id: &str,
) -> Result<CallToolResult, CallError> {
    match code {
        Some(0) => Ok(CallToolResult::success(vec![Content::text(stdout)])),
        Some(2) => Ok(CallToolResult::error(vec![Content::text(stdout)])),
        _ => {
            let detail = if stderr.trim().is_empty() {
                match code {
                    Some(c) => format!("elevation helper exited with status {}", c),
                    None => "elevation helper was killed by a signal".to_string(),
                }
            } else {
                stderr
            };
            Err(CallError::ElevationFailed(id.to_string(), detail))
        }
    }
}

/// Run the tool through a child `dmcp call`, which elevates itself via pkexec.
///
/// The child owns its process group and is killed on drop, so a cancelled or
/// timed-out call tears down the pkexec/server subtree instead of leaving a root
/// process behind.
async fn call_tool_elevated(
    id: &str,
    tool_name: &str,
    arguments: Option<serde_json::Value>,
) -> Result<CallToolResult, CallError> {
    let exe = std::env::current_exe().map_err(|e| {
        CallError::ElevationFailed(
            id.to_string(),
            format!("cannot locate the dmcp binary: {}", e),
        )
    })?;

    let mut cmd = Command::new(exe);
    cmd.args(elevated_cli_args(id, tool_name, arguments.as_ref()))
        .env(DELEGATED_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let child = cmd
        .spawn()
        .map_err(|e| CallError::ElevationFailed(id.to_string(), format!("cannot spawn: {}", e)))?;

    let secs = elevation_timeout_secs();
    let out = tokio::time::timeout(Duration::from_secs(secs), child.wait_with_output())
        .await
        .map_err(|_| CallError::ElevationTimedOut(id.to_string(), secs))?
        .map_err(|e| CallError::ElevationFailed(id.to_string(), e.to_string()))?;

    result_from_cli_exit(
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        id,
    )
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

    // A system-scope stdio server's tools must run as root. The CLI has already
    // re-exec'd by now; an unelevated caller (the agent surface) hands the call
    // to a child that elevates itself, so the tool runs at the right uid instead
    // of silently at the invoking user's (#45).
    match plan_elevation(
        scope,
        Some(primary),
        is_elevated(),
        std::env::var_os(DELEGATED_ENV).is_some(),
    ) {
        ElevationPlan::Direct => {}
        ElevationPlan::Delegate => return call_tool_elevated(id, tool_name, arguments).await,
        ElevationPlan::Refuse => {
            return Err(CallError::SystemScopeRequiresElevation(id.to_string()))
        }
    }

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
                None,
            )
            .await
        }
        Transport::Sse { url, .. } => call_tool_remote(url, "sse", tool_name, arguments).await,
        Transport::WebSocket { ws_url, .. } => {
            call_tool_remote(ws_url, "websocket", tool_name, arguments).await
        }
    }
}

/// Call a tool, answering any question the server asks over this process's own
/// stdio (the `--interactive` one-shot path).
///
/// This is what lets a **root** command prompt and be answered: system-scope
/// tools cannot use the broker (sessions are user-scope for elevation safety),
/// so the elevated one-shot call is the only path to them — and it re-execs
/// through pkexec carrying `--interactive`, which pkexec passes stdio through,
/// so the prompt stream reaches whoever spawned dmcp (dispatch) even from root.
///
/// Elevation composes: a system-scope stdio server invoked unelevated is handed
/// to a child `dmcp call --interactive` (the child re-execs through pkexec and
/// relays the same stdio), so delegation carries interactivity rather than
/// dropping it. On the CLI path elevation already happened via re-exec before
/// this runs, so the plan is Direct.
pub async fn call_tool_interactive(
    paths: &Paths,
    id: &str,
    tool_name: &str,
    arguments: Option<serde_json::Value>,
) -> Result<CallToolResult, CallError> {
    let (manifest, scope) =
        get_server(paths, id).ok_or_else(|| CallError::ServerNotFound(id.to_string()))?;

    let primary = crate::transport::select(manifest.transports.as_deref())?;

    // The CLI re-execs the whole process (argv carrying `--interactive`) through
    // pkexec *before* this runs, so on that path elevation is already done and
    // the plan is Direct. Delegate/Refuse are the unelevated system-scope cases,
    // which the interactive one-shot never reaches from the CLI; treat them as
    // the refusal rather than silently declining every prompt.
    match plan_elevation(
        scope,
        Some(primary),
        is_elevated(),
        std::env::var_os(DELEGATED_ENV).is_some(),
    ) {
        ElevationPlan::Direct => {}
        ElevationPlan::Delegate | ElevationPlan::Refuse => {
            return Err(CallError::SystemScopeRequiresElevation(id.to_string()))
        }
    }

    match primary {
        Transport::Stdio { command, args, .. } => {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let call = call_tool_stdio(
                paths,
                &manifest,
                id,
                command,
                args.as_deref(),
                tool_name,
                arguments,
                Some(tx),
            );
            drive_interactive_over_stdio(call, rx).await
        }
        // A remote server could elicit too, but the one-shot remote path holds
        // no long-lived channel and dispatch reaches remote servers the same
        // way; leave it unattended, matching call_tool.
        Transport::Sse { url, .. } => call_tool_remote(url, "sse", tool_name, arguments).await,
        Transport::WebSocket { ws_url, .. } => {
            call_tool_remote(ws_url, "websocket", tool_name, arguments).await
        }
    }
}

/// Run `call` while relaying each prompt it raises to this process's stdout as a
/// tagged JSON line and reading the answer back from stdin — the same tagged
/// stream the broker's `--interactive` path speaks, so a driver (dispatch) reads
/// one shape whether or not a session is involved.
///
/// The prompt channel is drained CONCURRENTLY with the call, biased so a
/// finished call wins a race with a late prompt: a server that elicits is
/// blocked until answered, so awaiting the call first would deadlock against the
/// question that must be answered to finish it. A closed or unreadable stdin
/// resolves to a decline, so the server unblocks rather than the call hanging.
async fn drive_interactive_over_stdio<F>(
    call: F,
    mut prompts: tokio::sync::mpsc::Receiver<crate::elicit::Prompt>,
) -> Result<CallToolResult, CallError>
where
    F: std::future::Future<Output = Result<CallToolResult, CallError>>,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut stdout = tokio::io::stdout();
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    tokio::pin!(call);
    loop {
        tokio::select! {
            biased;
            out = &mut call => return out,
            got = prompts.recv() => {
                let Some(prompt) = got else { continue };
                let line = serde_json::json!({
                    "type": "prompt",
                    "server": prompt.request.server,
                    "message": prompt.request.message,
                    "schema": prompt.request.schema,
                    "url": prompt.request.url,
                });
                let answer = if stdout
                    .write_all(format!("{}\n", line).as_bytes())
                    .await
                    .is_ok()
                    && stdout.flush().await.is_ok()
                {
                    match stdin.next_line().await {
                        Ok(Some(l)) => serde_json::from_str(l.trim())
                            .unwrap_or(crate::elicit::PromptAnswer::Decline),
                        _ => crate::elicit::PromptAnswer::Decline,
                    }
                } else {
                    crate::elicit::PromptAnswer::Decline
                };
                let _ = prompt.answer.send(answer);
            }
        }
    }
}

// One spawn helper covers both the plain and interactive stdio paths, so its
// argument list is legitimately wide; grouping them behind a struct would only
// move the noise to the call sites.
#[allow(clippy::too_many_arguments)]
async fn call_tool_stdio(
    paths: &Paths,
    manifest: &Manifest,
    id: &str,
    command: &str,
    args: Option<&[String]>,
    tool_name: &str,
    arguments: Option<serde_json::Value>,
    prompts: Option<tokio::sync::mpsc::Sender<crate::elicit::Prompt>>,
) -> Result<CallToolResult, CallError> {
    let cmd = build_stdio_command(paths, manifest, id, command, args)?;

    let transport =
        TokioChildProcess::new(cmd).map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

    // Attended only when there is somewhere for a prompt to go (the interactive
    // one-shot path). Otherwise the server sees no elicitation capability and
    // its questions are declined — today's behavior, unchanged.
    let handler = match prompts {
        Some(sink) => crate::elicit::ServerClient::attended(id, sink),
        None => crate::elicit::ServerClient::unattended(id),
    };
    let client = handler
        .serve(transport)
        .await
        .map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

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

    let client = crate::elicit::ServerClient::unattended(url)
        .serve(transport)
        .await
        .map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

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

    let client = crate::elicit::ServerClient::unattended(id)
        .serve(transport)
        .await
        .map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

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

    let client = crate::elicit::ServerClient::unattended(url)
        .serve(transport)
        .await
        .map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

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
    /// handed to a child that elevates itself — never run at the wrong uid (#45).
    #[test]
    fn agent_surface_delegates_unelevated_system_stdio() {
        let transports = stdio();
        assert_eq!(
            plan_elevation(Scope::System, selected(&transports), false, false),
            ElevationPlan::Delegate
        );
    }

    /// A child spawned to elevate must never spawn another: if pkexec failed
    /// there, delegating again would recurse until the box gave out.
    #[test]
    fn a_delegated_child_refuses_instead_of_recursing() {
        let transports = stdio();
        assert_eq!(
            plan_elevation(Scope::System, selected(&transports), false, true),
            ElevationPlan::Refuse
        );
    }

    /// A serve deliberately started as root is already elevated — root is the
    /// right uid for a system tool, so it runs in-process (as the CLI does).
    #[test]
    fn elevated_process_runs_system_stdio_directly() {
        let transports = stdio();
        assert_eq!(
            plan_elevation(Scope::System, selected(&transports), true, false),
            ElevationPlan::Direct
        );
    }

    /// User-scope tools never elevate — the whole point of the scope split is
    /// that they do not need root.
    #[test]
    fn user_scope_never_delegates() {
        let transports = stdio();
        assert_eq!(
            plan_elevation(Scope::User, selected(&transports), false, false),
            ElevationPlan::Direct
        );
        assert_eq!(
            plan_elevation(Scope::User, selected(&transports), false, true),
            ElevationPlan::Direct
        );
    }

    /// A remote (SSE/WebSocket) system server holds no local process to run as
    /// root, so it is never delegated — consistent with the CLI, which does not
    /// elevate for a remote transport.
    #[test]
    fn system_remote_is_never_delegated() {
        let sse = vec![Transport::Sse {
            url: "http://example".into(),
            description: None,
            platforms: PlatformDecl::Absent,
        }];
        assert_eq!(
            plan_elevation(Scope::System, selected(&sse), false, false),
            ElevationPlan::Direct
        );
    }

    /// The delegated argv is the one-shot CLI invocation — and carries no
    /// `--session`, so an elevated server cannot outlive its command.
    #[test]
    fn delegated_argv_is_the_one_shot_cli_call() {
        let args = elevated_cli_args("sys.server", "run", Some(&serde_json::json!({"a": 1})));
        assert_eq!(args[0], "call");
        assert_eq!(args[1], "sys.server");
        assert_eq!(args[2], "run");
        assert_eq!(args[3], "--args");
        assert_eq!(args[4], r#"{"a":1}"#);
        assert!(!args.iter().any(|a| a == "--session"));
    }

    /// Empty or absent arguments produce no `--args`, matching a hand-typed CLI
    /// invocation.
    #[test]
    fn delegated_argv_omits_empty_arguments() {
        assert_eq!(elevated_cli_args("s", "t", None).len(), 3);
        assert_eq!(
            elevated_cli_args("s", "t", Some(&serde_json::json!({}))).len(),
            3
        );
        assert_eq!(
            elevated_cli_args("s", "t", Some(&serde_json::Value::Null)).len(),
            3
        );
    }

    /// The child's exit code carries the tool's status, exactly as `dmcp call`
    /// reports it: 0 success, 2 a tool-reported error whose output is preserved.
    #[test]
    fn delegated_exit_code_maps_to_the_tool_result() {
        let ok = result_from_cli_exit(Some(0), "output".into(), String::new(), "s").unwrap();
        assert!(!call_is_error(&ok));
        assert_eq!(format_call_result(&ok), "output");

        let tool_err = result_from_cli_exit(Some(2), "boom".into(), String::new(), "s").unwrap();
        assert!(call_is_error(&tool_err));
        assert_eq!(format_call_result(&tool_err), "boom");
    }

    /// A polkit denial is an elevation failure, not a tool result — and the
    /// stderr explaining it reaches the caller.
    #[test]
    fn a_denied_elevation_is_reported_not_silently_run() {
        let err = result_from_cli_exit(Some(127), String::new(), "Not authorized".into(), "sys")
            .unwrap_err();
        assert!(matches!(err, CallError::ElevationFailed(ref id, _) if id == "sys"));
        let msg = err.to_string();
        assert!(msg.contains("Not authorized"));
        assert!(msg.contains("non-active session"));

        // A signal kill still names the server rather than passing for success.
        let killed = result_from_cli_exit(None, String::new(), String::new(), "sys").unwrap_err();
        assert!(killed.to_string().contains("signal"));
    }

    /// The unanswered-prompt timeout is always positive, so a bad env value
    /// cannot turn it into an instant or infinite wait.
    #[test]
    fn elevation_timeout_is_positive_and_named_in_the_error() {
        assert!(elevation_timeout_secs() > 0);
        let err = CallError::ElevationTimedOut("sys".into(), 180);
        assert!(err.to_string().contains("180s"));
        assert!(err.to_string().contains("DMCP_ELEVATION_TIMEOUT_SECS"));
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
