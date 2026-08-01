//! Call tools on MCP servers.
//!
//! Connects to a server via its transport (stdio, SSE, WebSocket) and invokes tools.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, Content};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use tokio::task::JoinHandle;

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

/// How long to wait for the relay to hand back its retained bytes once the
/// child is gone. EOF normally arrives instantly then; the bound exists
/// because a server can leak its stderr fd into a longer-lived grandchild,
/// and a pipe that will never close must not hang the call.
const RELAY_DRAIN_SECS: u64 = 5;

/// Cap on the stderr RETAINED for failure detail: the most recent 64 KiB.
/// The tail is kept because the end of stderr is where the failure reason
/// lives — the final traceback, the last error line — while the head is
/// startup noise. Without the cap a flooding server balloons memory for the
/// whole call and turns the error text (which `dmcp serve` delivers verbatim
/// to the LLM as the tool-result error) into a prompt-sized payload. The live
/// relay onto dmcp's own stderr is deliberately NOT capped: the caller
/// consumes that stream incrementally, so it costs nothing to keep whole.
const STDERR_RETAIN_MAX: usize = 64 * 1024;

/// Prefixed to the failure detail when retention dropped earlier bytes, so a
/// reader knows the text is a tail, not the whole story.
const STDERR_TRUNCATION_MARKER: &str = "[stderr truncated to last 64 KiB]";

/// The bounded tail of a child's stderr, kept for failure detail.
#[derive(Debug, Default)]
struct RetainedStderr {
    bytes: Vec<u8>,
    truncated: bool,
}

impl RetainedStderr {
    /// The failure-detail text: the retained tail, trimmed, prefixed with the
    /// truncation marker when earlier bytes were dropped. `None` when nothing
    /// beyond whitespace was retained.
    fn detail(&self) -> Option<String> {
        let text = String::from_utf8_lossy(&self.bytes);
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        Some(if self.truncated {
            format!("{} {}", STDERR_TRUNCATION_MARKER, text)
        } else {
            text.to_string()
        })
    }
}

/// Tee a child's stderr onto `sink` (dmcp's own stderr) as it arrives, while
/// retaining the most recent `STDERR_RETAIN_MAX` bytes for failure detail
/// (#49). Raw chunks, never lines: a prompt like "Proceed? [Y/n] " has no
/// trailing newline and must still flow through promptly. The task also keeps
/// draining after a failed sink write, so a caller that closed our stderr
/// cannot back-pressure the child into a wedge — the old wait_with_output
/// drained both pipes unconditionally too.
fn relay_stderr<R, W>(mut source: R, mut sink: W) -> JoinHandle<RetainedStderr>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut retained = RetainedStderr::default();
        let mut buf = [0u8; 8192];
        loop {
            match source.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    retained.bytes.extend_from_slice(&buf[..n]);
                    if retained.bytes.len() > STDERR_RETAIN_MAX {
                        let excess = retained.bytes.len() - STDERR_RETAIN_MAX;
                        retained.bytes.drain(..excess);
                        retained.truncated = true;
                    }
                    if sink.write_all(&buf[..n]).await.is_ok() {
                        let _ = sink.flush().await;
                    }
                }
            }
        }
        retained
    })
}

/// Collect what the relay retained. Bounded: past the drain window the relay
/// is aborted rather than awaited, because the only way EOF is still pending
/// is an fd held open by something that outlived the server.
async fn finish_relay(relay: Option<JoinHandle<RetainedStderr>>) -> RetainedStderr {
    let Some(mut handle) = relay else {
        return RetainedStderr::default();
    };
    match tokio::time::timeout(Duration::from_secs(RELAY_DRAIN_SECS), &mut handle).await {
        Ok(Ok(retained)) => retained,
        Ok(Err(_)) => RetainedStderr::default(),
        Err(_) => {
            handle.abort();
            RetainedStderr::default()
        }
    }
}

/// Append the retained server stderr to a failed call's error text. Only the
/// variants that mean "the server misbehaved" carry it — that is where the
/// explanation (a traceback, a missing dependency) actually lives.
fn attach_stderr_detail(err: CallError, stderr: &RetainedStderr) -> CallError {
    let Some(text) = stderr.detail() else {
        return err;
    };
    match err {
        CallError::ConnectionFailed(d) => {
            CallError::ConnectionFailed(format!("{}; server stderr: {}", d, text))
        }
        CallError::ToolCallFailed(d) => {
            CallError::ToolCallFailed(format!("{}; server stderr: {}", d, text))
        }
        other => other,
    }
}

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

    let mut child = cmd
        .spawn()
        .map_err(|e| CallError::ElevationFailed(id.to_string(), format!("cannot spawn: {}", e)))?;

    // The child's stderr — polkit chatter plus whatever its server relays — is
    // teed onto our own as it arrives instead of sitting invisible until exit,
    // while the retained copy still becomes the ElevationFailed detail (#49).
    let relay = child
        .stderr
        .take()
        .map(|s| relay_stderr(s, tokio::io::stderr()));

    let secs = elevation_timeout_secs();
    // stdout is read to EOF before the wait, concurrently with the stderr
    // relay task — the same both-pipes-at-once guarantee wait_with_output
    // gave, so neither pipe filling up can wedge the call.
    let (status, stdout) = tokio::time::timeout(Duration::from_secs(secs), async {
        let mut stdout = Vec::new();
        if let Some(mut out) = child.stdout.take() {
            out.read_to_end(&mut stdout).await?;
        }
        let status = child.wait().await?;
        std::io::Result::Ok((status, stdout))
    })
    .await
    .map_err(|_| CallError::ElevationTimedOut(id.to_string(), secs))?
    .map_err(|e| CallError::ElevationFailed(id.to_string(), e.to_string()))?;

    let stderr = finish_relay(relay).await;

    result_from_cli_exit(
        status.code(),
        String::from_utf8_lossy(&stdout).trim_end().to_string(),
        stderr.detail().unwrap_or_default(),
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

    // Piped instead of rmcp's inherited default: the relay keeps the same live
    // view a caller had under inheritance, and the retained copy lets a failed
    // call say why instead of pointing at a log nobody captured (#49). The
    // JSON-RPC wire (stdout) is untouched.
    let (transport, child_stderr) = TokioChildProcess::builder(cmd)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CallError::ConnectionFailed(e.to_string()))?;
    let relay = child_stderr.map(|s| relay_stderr(s, tokio::io::stderr()));

    let outcome = drive_stdio_call(transport, id, tool_name, arguments).await;

    // The server is gone by now (cancelled, or its transport dropped and the
    // child reaped), so the relay is at EOF; draining it here keeps trailing
    // stderr ahead of the result we are about to report.
    let retained = finish_relay(relay).await;
    outcome.map_err(|e| attach_stderr_detail(e, &retained))
}

async fn drive_stdio_call(
    transport: TokioChildProcess,
    id: &str,
    tool_name: &str,
    arguments: Option<serde_json::Value>,
) -> Result<CallToolResult, CallError> {
    let client = crate::elicit::ServerClient::unattended(id)
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
        .map_err(|e| CallError::ToolCallFailed(e.to_string()));

    // Cancelled on the error path too, not just success: the relay behind this
    // call drains at EOF, and EOF only comes once the child is gone.
    client.cancel().await.ok();

    result
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

    /// The elevated path and the one-shot path share `relay_stderr`; these
    /// unit tests are the elevated half's coverage, since pkexec cannot be
    /// driven end to end from a test suite.
    ///
    /// A newline-less chunk (an interactive prompt) must reach the sink while
    /// the source is still open — before EOF, before any line terminator — or
    /// the relay is buffering exactly the way #49 forbids.
    #[tokio::test]
    async fn relay_forwards_a_partial_line_before_eof_and_retains_it() {
        let (mut source_in, source_out) = tokio::io::duplex(64);
        let (sink, mut sink_out) = tokio::io::duplex(64);
        let handle = relay_stderr(source_out, sink);

        source_in.write_all(b"Proceed? [Y/n] ").await.unwrap();
        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(5), sink_out.read(&mut buf))
            .await
            .expect("the chunk must arrive while the source is still open")
            .unwrap();
        assert_eq!(&buf[..n], b"Proceed? [Y/n] ");

        source_in.write_all(b"denied\n").await.unwrap();
        drop(source_in);
        let retained = handle.await.unwrap();
        assert_eq!(retained.bytes, b"Proceed? [Y/n] denied\n");
        assert!(!retained.truncated);
    }

    /// A caller that closed our stderr must not cost the failure detail — the
    /// relay keeps reading (so the child cannot wedge on a full pipe) and keeps
    /// retaining (so the error text still says why).
    #[tokio::test]
    async fn relay_survives_a_closed_sink_and_still_retains() {
        let (mut source_in, source_out) = tokio::io::duplex(64);
        let (sink, sink_out) = tokio::io::duplex(16);
        drop(sink_out);
        let handle = relay_stderr(source_out, sink);

        source_in.write_all(b"Traceback: boom").await.unwrap();
        drop(source_in);
        assert_eq!(handle.await.unwrap().bytes, b"Traceback: boom");
    }

    /// Retention is a bounded TAIL: a flooding server keeps only its most
    /// recent 64 KiB — the end, where the failure reason lives — flagged as
    /// truncated, so neither the call's memory nor the eventual error text
    /// scales with how much the server wrote.
    #[tokio::test]
    async fn relay_retains_only_the_tail_of_a_flood() {
        let (mut source_in, source_out) = tokio::io::duplex(8192);
        let handle = relay_stderr(source_out, tokio::io::sink());

        source_in.write_all(b"EARLY_MARKER\n").await.unwrap();
        source_in
            .write_all(&vec![b'x'; 3 * STDERR_RETAIN_MAX])
            .await
            .unwrap();
        source_in.write_all(b"\nLATE_MARKER").await.unwrap();
        drop(source_in);

        let retained = handle.await.unwrap();
        assert!(retained.truncated);
        assert_eq!(retained.bytes.len(), STDERR_RETAIN_MAX);
        assert!(retained.bytes.ends_with(b"LATE_MARKER"));
        let early = b"EARLY_MARKER";
        assert!(!retained.bytes.windows(early.len()).any(|w| w == early));
    }

    /// A stderr fd leaked into a grandchild never reaches EOF; the drain is
    /// bounded so that cannot hang the call after its server is gone.
    #[tokio::test(start_paused = true)]
    async fn a_relay_that_never_sees_eof_cannot_hang_the_call() {
        let (source_in, source_out) = tokio::io::duplex(8);
        let (sink, _sink_out) = tokio::io::duplex(8);
        let handle = relay_stderr(source_out, sink);

        let retained = finish_relay(Some(handle)).await;
        assert!(retained.bytes.is_empty());
        drop(source_in);
    }

    fn complete(bytes: &[u8]) -> RetainedStderr {
        RetainedStderr {
            bytes: bytes.to_vec(),
            truncated: false,
        }
    }

    /// The retained stderr lands in the failure a caller reads, on exactly the
    /// variants that mean the server misbehaved.
    #[test]
    fn failure_detail_carries_the_retained_stderr() {
        let err = attach_stderr_detail(
            CallError::ToolCallFailed("connection closed".into()),
            &complete(b"Traceback: boom\n"),
        );
        let msg = err.to_string();
        assert!(msg.contains("connection closed"));
        assert!(msg.contains("server stderr: Traceback: boom"));
        assert!(!msg.contains(STDERR_TRUNCATION_MARKER));

        let conn = attach_stderr_detail(
            CallError::ConnectionFailed("broken pipe".into()),
            &complete(b"denied"),
        );
        assert!(conn.to_string().contains("server stderr: denied"));
    }

    /// A truncated tail announces itself: the marker leads the detail, so a
    /// reader knows the text is the end of a longer stream.
    #[test]
    fn truncated_detail_leads_with_the_marker() {
        let retained = RetainedStderr {
            bytes: b"tail of the flood".to_vec(),
            truncated: true,
        };
        let err = attach_stderr_detail(CallError::ToolCallFailed("boom".into()), &retained);
        assert!(err.to_string().contains(&format!(
            "server stderr: {} tail of the flood",
            STDERR_TRUNCATION_MARKER
        )));
    }

    /// Silence adds nothing: a server that wrote no stderr (or only whitespace)
    /// leaves the error text byte-identical to before the relay existed.
    #[test]
    fn empty_stderr_leaves_the_error_text_unchanged() {
        let plain = attach_stderr_detail(CallError::ToolCallFailed("boom".into()), &complete(b""));
        assert_eq!(plain.to_string(), "Tool call failed: boom");
        let blank = attach_stderr_detail(
            CallError::ToolCallFailed("boom".into()),
            &complete(b"  \n\t"),
        );
        assert_eq!(blank.to_string(), "Tool call failed: boom");

        let unrelated =
            attach_stderr_detail(CallError::ServerNotFound("s".into()), &complete(b"noise"));
        assert_eq!(unrelated.to_string(), "Server not found: s");
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
