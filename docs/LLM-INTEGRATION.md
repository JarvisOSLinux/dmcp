# Using dmcp with LLMs (Cursor, Claude, etc.)

dmcp can run as an MCP server, exposing its operations as tools. LLMs connect to dmcp and use these tools to manage and invoke MCP servers.

## How LLMs Discover dmcp's Tools

When an LLM connects to dmcp, dmcp sends its **tool list** via the MCP protocol (`tools/list`). Each tool includes:

- **Name** — e.g. `list_servers`, `install_server`, `call_server_tool`
- **Description** — What the tool does (the LLM reads this to decide when to use it)
- **Input schema** — JSON schema for arguments (the LLM uses this to construct valid calls)

The LLM does not need separate documentation. It learns from the tool metadata at connection time.

## Configuring Cursor

1. Open Cursor Settings → MCP (or edit `~/.cursor/mcp.json`).
2. Add dmcp as a server:

```json
{
  "mcpServers": {
    "dmcp": {
      "command": "dmcp",
      "args": ["serve"]
    }
  }
}
```

3. Restart Cursor or reload MCP servers.
4. The AI can now use dmcp's tools (list servers, install, call tools, etc.).

## Configuring Claude Desktop

1. Edit the Claude config:
   - **Linux:** `~/.config/Claude/claude_desktop_config.json`
   - **macOS:** `~/Library/Application Support/Claude/claude_desktop_config.json`
   - **Windows:** `%APPDATA%\Claude\claude_desktop_config.json`

2. Add dmcp:

```json
{
  "mcpServers": {
    "dmcp": {
      "command": "dmcp",
      "args": ["serve"]
    }
  }
}
```

3. Restart Claude Desktop.

## dmcp Tools Exposed to LLMs

| Tool | Description |
|------|-------------|
| `list_servers` | List installed MCP servers (user/system scope) |
| `get_server_info` | Get detailed info for a server by ID |
| `install_server` | Install a server from a configured registry (trust-gated: `deprecated`/`removed` servers are always refused for the agent; community-tier installs are flagged as not maintainer-reviewed) |
| `uninstall_server` | Uninstall a server |
| `set_config` | Set a config value for a server |
| `list_server_tools` | List tools available on an MCP server |
| `call_server_tool` | Call a tool on an MCP server |
| `dispatch_tasks` | Dispatch multiple MCP tool calls concurrently; returns PIDs for tracking |
| `get_task_status` | Get completed/failed tasks since last call; optional rolling log |
| `kill_task` | Kill a dispatched task by PID |

### Multitasking (dispatch_tasks, get_task_status, kill_task)

For concurrent execution: use `dispatch_tasks` with `{ "tasks": [ { "server": "id", "tool": "name", "params": {} }, ... ] }` to spawn tasks in parallel. Poll `get_task_status` for results; use `kill_task` with a PID to abort a running task.

## Example Flow

1. User: "What MCP servers do I have installed?"
2. LLM calls `list_servers` → gets the list.
3. User: "Install the calculator server."
4. LLM calls `install_server` with `id: "com.example.calculator"`.
5. User: "What tools does the calculator have?"
6. LLM calls `list_server_tools` with `id: "com.example.calculator"`.
7. User: "Add 2 and 3 using the calculator."
8. LLM calls `call_server_tool` with `id`, `tool: "add"`, `args: {"a": 2, "b": 3}`.

## Path to dmcp

Ensure `dmcp` is in your `PATH`:

```bash
cargo install --path .   # from dmcp repo
# or
cargo build --release && sudo cp target/release/dmcp /usr/local/bin/
```

If `dmcp` is not in PATH, use the full path in the config:

```json
{
  "command": "/home/user/.cargo/bin/dmcp",
  "args": ["serve"]
}
```

## Changelog — corrected claims

*2026-07-22:* `install_server` description notes the agent trust gate (deprecated/removed refused, community tier flagged).
