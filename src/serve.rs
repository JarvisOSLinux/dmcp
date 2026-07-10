//! dmcp as an MCP server.
//!
//! Exposes dmcp operations as MCP tools so LLMs (Cursor, Claude, etc.) can control dmcp.

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::Deserialize;
use std::sync::Arc;

use crate::orchestrator::{DispatchRequest, Orchestrator};
use crate::paths::Paths;

/// dmcp MCP server - exposes all dmcp operations as tools.
#[derive(Clone)]
pub struct DmcpServer {
    paths: Arc<Paths>,
    orchestrator: Arc<Orchestrator>,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ListServersParams {
    #[serde(default)]
    user: bool,
    #[serde(default)]
    system: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ServerIdParam {
    id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct InstallParams {
    id: String,
    #[serde(default)]
    system: bool,
    #[serde(default)]
    no_setup: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ConfigSetParams {
    id: String,
    key: String,
    value: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CallToolParams {
    id: String,
    tool: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DispatchTasksParams {
    tasks: Vec<crate::orchestrator::DispatchTask>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct GetTaskStatusParams {
    #[serde(default)]
    include_log: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct KillTaskParams {
    pid: u64,
}

#[tool_router]
impl DmcpServer {
    pub fn new(paths: Paths) -> Self {
        let paths = Arc::new(paths);
        let orchestrator = Arc::new(Orchestrator::new(Arc::clone(&paths)));
        Self {
            paths,
            orchestrator,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List installed MCP servers. Use user=true for user scope, system=true for system scope."
    )]
    async fn list_servers(
        &self,
        params: Parameters<ListServersParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let p = params.0;
        let user = p.user || !p.system;
        let system = p.system || !p.user;
        let servers = crate::list_servers(&self.paths, user, system, false);
        let json = serde_json::to_string_pretty(&servers).unwrap_or_default();
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Get detailed info for a server by ID")]
    async fn get_server_info(
        &self,
        params: Parameters<ServerIdParam>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let id = params.0.id;
        match crate::get_server(&self.paths, &id) {
            Some((manifest, scope)) => {
                let scope_str = if scope == crate::discovery::Scope::User {
                    "user"
                } else {
                    "system"
                };
                let mut out = serde_json::Map::new();
                out.insert(
                    "id".into(),
                    manifest.id.clone().unwrap_or(id.clone()).into(),
                );
                out.insert(
                    "name".into(),
                    manifest.name.clone().unwrap_or_default().into(),
                );
                out.insert(
                    "version".into(),
                    manifest.version.clone().unwrap_or_default().into(),
                );
                out.insert("scope".into(), scope_str.into());
                if let Some(ref t) = manifest.transports {
                    let types: Vec<String> = t
                        .iter()
                        .map(|x| match x {
                            crate::models::Transport::Stdio { .. } => "stdio",
                            crate::models::Transport::Sse { .. } => "sse",
                            crate::models::Transport::WebSocket { .. } => "websocket",
                        })
                        .map(String::from)
                        .collect();
                    out.insert(
                        "transports".into(),
                        serde_json::to_string_pretty(&types).unwrap().into(),
                    );
                }
                let json = serde_json::to_string_pretty(&out).unwrap_or_default();
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            None => Ok(CallToolResult::error(vec![Content::text(format!(
                "Server not found: {}",
                id
            ))])),
        }
    }

    #[tool(description = "Install an MCP server from registry by ID")]
    async fn install_server(
        &self,
        params: Parameters<InstallParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let p = params.0;
        match crate::fetch_server_from_registry(&self.paths, &p.id) {
            Ok(server) => {
                let scope = if p.system {
                    crate::discovery::Scope::System
                } else {
                    crate::install::scope_from_registry_server(&server)
                };
                match crate::install::install(&self.paths, &p.id, scope, Some(server), !p.no_setup)
                {
                    Ok(()) => Ok(CallToolResult::success(vec![Content::text(format!(
                        "Installed {}",
                        p.id
                    ))])),
                    Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
                }
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    #[tool(description = "Uninstall an MCP server by ID")]
    async fn uninstall_server(
        &self,
        params: Parameters<ServerIdParam>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let id = params.0.id;
        match crate::uninstall(&self.paths, &id) {
            Ok(()) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Uninstalled {}",
                id
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    #[tool(description = "Set a config value for a server. Key and value are strings.")]
    async fn set_config(
        &self,
        params: Parameters<ConfigSetParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let p = params.0;
        match crate::set_config_value(&self.paths, &p.id, &p.key, &p.value) {
            Ok(()) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Set {} = {}",
                p.key, p.value
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    #[tool(description = "List tools available on an MCP server")]
    async fn list_server_tools(
        &self,
        params: Parameters<ServerIdParam>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let id = params.0.id;
        match crate::call::list_tools(&self.paths, &id).await {
            Ok(tools) => {
                let out: Vec<serde_json::Value> = tools
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": t.input_schema
                        })
                    })
                    .collect();
                let json = serde_json::to_string_pretty(&out).unwrap_or_default();
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    #[tool(description = "Call a tool on an MCP server. Args is optional JSON object.")]
    async fn call_server_tool(
        &self,
        params: Parameters<CallToolParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let p = params.0;
        match crate::call::call_tool(&self.paths, &p.id, &p.tool, p.args).await {
            Ok(result) => Ok(result),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
    }

    #[tool(
        description = "Dispatch multiple MCP tool calls concurrently. Returns PIDs for tracking. Use get_task_status to poll results, kill_task to abort."
    )]
    async fn dispatch_tasks(
        &self,
        params: Parameters<DispatchTasksParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let req = DispatchRequest {
            tasks: params.0.tasks,
        };
        match self.orchestrator.dispatch_tasks(req).await {
            Ok(pids) => {
                let json = serde_json::to_string_pretty(&serde_json::json!({ "pids": pids }))
                    .unwrap_or_default();
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }

    #[tool(
        description = "Get completed/failed tasks since last call. Set include_log=true for rolling 20-entry log window."
    )]
    async fn get_task_status(
        &self,
        params: Parameters<GetTaskStatusParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let include_log = params.0.include_log;
        let status = self.orchestrator.get_task_status(include_log).await;
        let json = serde_json::to_string_pretty(&status).unwrap_or_default();
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Kill a dispatched task by PID.")]
    async fn kill_task(
        &self,
        params: Parameters<KillTaskParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let pid = params.0.pid;
        match self.orchestrator.kill_task(pid).await {
            Ok(killed) => {
                let msg = if killed {
                    format!("Task {} killed", pid)
                } else {
                    format!("Task {} not found (already completed or invalid)", pid)
                };
                Ok(CallToolResult::success(vec![Content::text(msg)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e)])),
        }
    }
}

#[tool_handler]
impl ServerHandler for DmcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "dmcp is an MCP server manager. Use these tools to list, install, uninstall, \
                 configure, and run MCP servers. You can also list and call tools on installed servers. \
                 For concurrent multitasking: dispatch_tasks spawns multiple tool calls in parallel and returns PIDs; \
                 get_task_status returns completed/failed results; kill_task aborts a task by PID."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// Run dmcp as an MCP server (stdio transport).
pub fn run(paths: &Paths) -> Result<(), Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let server = DmcpServer::new(paths.clone());
        let service = server.serve(stdio()).await?;
        service.waiting().await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Agent-confinement invariant — see docs/TRUST-MODEL.md §2.2 (mcp-registry).
    //
    // Project-JARVIS reaches dmcp only through this MCP surface, and it must NOT
    // be able to widen the set of sources it can install from. Two locks:
    //   1. no tool mutates the source list (sources.list);
    //   2. install_server is id-only — no url/path/source — so the agent can only
    //      install servers a human already trusted by configuring a registry.
    // These tests introspect the actual generated tool router, so they catch a
    // regression the moment a tool or field is added. If a change here is
    // intended, update these lists AND TRUST-MODEL.md deliberately.

    #[test]
    fn serve_surface_has_no_source_mutation_tool() {
        let allowed = [
            "list_servers",
            "get_server_info",
            "install_server",
            "uninstall_server",
            "set_config",
            "list_server_tools",
            "call_server_tool",
            "dispatch_tasks",
            "get_task_status",
            "kill_task",
        ];
        for tool in DmcpServer::tool_router().list_all() {
            let name = tool.name.as_ref();
            assert!(
                allowed.contains(&name),
                "unexpected MCP tool '{name}' on the serve surface; if it can reach \
                 the source list it breaks agent confinement (TRUST-MODEL.md §2.2)"
            );
            let lower = name.to_lowercase();
            assert!(
                !lower.contains("source") && !lower.contains("registr"),
                "tool '{name}' looks like it manages sources; confinement forbids it"
            );
        }
    }

    #[test]
    fn install_server_tool_is_id_only() {
        let router = DmcpServer::tool_router();
        let tool = router
            .get("install_server")
            .expect("install_server tool must exist");
        let props = tool
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("install_server input schema must list properties");
        for forbidden in [
            "url", "uri", "path", "source", "manifest", "endpoint", "registry",
        ] {
            assert!(
                !props.contains_key(forbidden),
                "InstallParams gained a '{forbidden}' field — the agent could install \
                 from an arbitrary location, breaking source confinement"
            );
        }
        assert!(
            props.contains_key("id"),
            "install_server must install by id"
        );
    }
}
