//! Call tools on MCP servers.
//!
//! Connects to a server via its transport (stdio, SSE, WebSocket) and invokes tools.

use std::path::Path;

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use tokio::process::Command;

use crate::discovery::{get_manifest_path, get_server};
use crate::models::{Manifest, Transport};
use crate::paths::Paths;
use crate::run::config_to_env;

/// Errors from calling tools.
#[derive(Debug)]
pub enum CallError {
    ServerNotFound(String),
    NoTransports,
    NoStdioTransport,
    RemoteNotSupported(String),
    ConnectionFailed(String),
    ToolCallFailed(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::ServerNotFound(id) => write!(f, "Server not found: {}", id),
            CallError::NoTransports => write!(f, "No transports defined"),
            CallError::NoStdioTransport => write!(f, "Server has no stdio transport"),
            CallError::RemoteNotSupported(t) => {
                write!(f, "Remote transport not yet supported: {}", t)
            }
            CallError::ConnectionFailed(e) => write!(f, "Connection failed: {}", e),
            CallError::ToolCallFailed(e) => write!(f, "Tool call failed: {}", e),
        }
    }
}

impl std::error::Error for CallError {}

/// Call a tool on an installed MCP server.
pub async fn call_tool(
    paths: &Paths,
    id: &str,
    tool_name: &str,
    arguments: Option<serde_json::Value>,
) -> Result<CallToolResult, CallError> {
    let (manifest, _) =
        get_server(paths, id).ok_or_else(|| CallError::ServerNotFound(id.to_string()))?;

    let transports = manifest
        .transports
        .as_deref()
        .ok_or(CallError::NoTransports)?;
    let primary = transports.first().ok_or(CallError::NoTransports)?;

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
    let install_dir = manifest
        .install_dir
        .as_deref()
        .map(Path::new)
        .filter(|p| p.is_absolute())
        .map(|p| p.to_path_buf())
        .or_else(|| get_manifest_path(paths, id).and_then(|p| p.parent().map(|p| p.to_path_buf())))
        .ok_or(CallError::NoStdioTransport)?;

    let env = config_to_env(&manifest.config);

    let mut cmd = Command::new(command);
    let args: Vec<&str> = args
        .map(|a| a.iter().map(String::as_str).collect())
        .unwrap_or_default();
    cmd.args(&args).current_dir(&install_dir).envs(env);

    let transport = TokioChildProcess::new(cmd.configure(|_| {}))
        .map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

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

    let transports = manifest
        .transports
        .as_deref()
        .ok_or(CallError::NoTransports)?;
    let primary = transports.first().ok_or(CallError::NoTransports)?;

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
    let install_dir = manifest
        .install_dir
        .as_deref()
        .map(Path::new)
        .filter(|p| p.is_absolute())
        .map(|p| p.to_path_buf())
        .or_else(|| get_manifest_path(paths, id).and_then(|p| p.parent().map(|p| p.to_path_buf())))
        .ok_or(CallError::NoStdioTransport)?;

    let env = config_to_env(&manifest.config);

    let mut cmd = Command::new(command);
    let args: Vec<&str> = args
        .map(|a| a.iter().map(String::as_str).collect())
        .unwrap_or_default();
    cmd.args(&args).current_dir(&install_dir).envs(env);

    let transport = TokioChildProcess::new(cmd.configure(|_| {}))
        .map_err(|e| CallError::ConnectionFailed(e.to_string()))?;

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
    use rmcp::model::Content;

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
