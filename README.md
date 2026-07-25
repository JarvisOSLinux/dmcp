# dmcp

**MCP Manager** — a modular, system- and user-level manager for MCP (Model Context Protocol) servers.

## What it does

dmcp discovers, manages, and invokes MCP servers installed on your system. It works at two scopes:

- **User scope** — per-user, no root required (`~/.local/share/mcp/`, `~/.config/mcp/`)
- **System scope** — system-wide, visible to all users (`/usr/share/mcp/`, `/etc/mcp/`)

It supports both **local** (stdio) and **remote** (SSE, WebSocket) servers. Local servers are cloned and run from disk; remote servers are metadata-only, with connection endpoints stored in manifests.

## Features

- **Discovery** — List installed servers (user + system)
- **Registry** — Browse servers from configurable registry URLs
- **Install** — Install from registry (by ID) or from URL (manifest/endpoint)
- **Connect** — Add remote servers by URL (fetches manifest if valid JSON, else treats as raw endpoint)
- **Config** — Get and set per-server configuration (API keys, endpoints, etc.)
- **Invocation** — Spawn stdio servers; SSE/WebSocket: print connection URL
- **Setup** — Run setup scripts at install (dependencies, config) or via `dmcp setup <id>`

## Configuration

Paths are configurable via environment variables. Copy `.env.example` to `.env` and adjust as needed:

```bash
cp .env.example .env
```

See [MCP-SYSTEM-SPEC.md](MCP-SYSTEM-SPEC.md) for the full specification and [MCP-REGISTRY-GUIDE.md](MCP-REGISTRY-GUIDE.md) for registry format and install flow.

## Build & Run

Requires [Rust](https://rustup.rs/).

```bash
cargo build --release
cargo install --path .   # Install to ~/.cargo/bin
```

## Commands

| Command | Description |
|---------|-------------|
| `dmcp list [--user] [--system] [--json]` | List installed MCP servers (default: both) |
| `dmcp info <id> [--json]` | Show detailed info for a server |
| `dmcp config <id> get [key] [--json]` | Get config value(s) |
| `dmcp config <id> set <key> <value>` | Set a config value (uses pkexec for system scope) |
| `dmcp sources list [--user] [--system]` | List registry source URLs |
| `dmcp sources add <url> [--system]` | Add a registry source (default: user) |
| `dmcp sources remove <url> [--system]` | Remove a registry source |
| `dmcp browse [url] [--user] [--system] [-k keyword...] [--vector JSON \| --vectors JSON] [--top-k N] [--min-score F] [--json]` | Browse servers in registries (keyword filter or semantic vector search against the local index; transport fetched from manifest when registry omits it; entries the registry does not vouch for on this host are marked `unsupported_on_host`) |
| `dmcp install <id or url> [--system] [--no-setup] [--ignore-platform]` | Install from registry (by ID) or from manifest/endpoint URL (refused before any clone or setup when the entry's `platforms` exclude this host, unless `--ignore-platform`) |
| `dmcp uninstall <id>` | Remove installed server |
| `dmcp update <id> \| --all [--check] [--json] [--ignore-platform]` | Refresh installed servers whose registry manifest hash has drifted (detects same-version fixes; `--check` reports without changing, including platform state; `--json` requires `--check`) |
| `dmcp run <id> [--verbose]` | Run server (stdio: spawn; SSE/WebSocket: print URL) |
| `dmcp tools <id> [--json]` | List tools on a server |
| `dmcp call <id> <tool> [--args JSON]` | Call a tool on a server |
| `dmcp serve` | Run dmcp as MCP server (for LLM integration) |
| `dmcp setup <id>` | Run setup script for an installed server |
| `dmcp connect <url> [--id] [--name] [--summary] [--version] [-c key=value...] [--system] [--no-setup]` | Connect to remote server |
| `dmcp count [--json]` | Count visible MCP servers (local + reachable registries) |
| `dmcp sync-index` | Download and cache registry vector embeddings for semantic search |
| `dmcp embedding-spec [--json]` | Show the embedding model spec the index expects |
| `dmcp index-server <id>` | Add/update one server in the local vector index |
| `dmcp paths` | Show resolved paths (debug) |

## Project Structure

```
src/
├── main.rs      # CLI entry point
├── lib.rs       # Library root
├── paths.rs     # Path resolution (env, XDG)
├── discovery.rs # List servers, get_server, load index/manifests
├── sources.rs   # Registry sources (sources.list)
├── config.rs    # Config get/set
├── install.rs   # Install, uninstall
├── run.rs       # Run servers (stdio spawn, SSE/WS URL)
├── setup.rs     # Setup script execution

├── browse.rs    # Browse registry servers
├── transport.rs # Transport extraction from manifests (MVP; fetches when registry omits)
├── connect.rs   # Connect to remote by URL (manifest or raw)
├── call.rs      # List/call tools on MCP servers
├── serve.rs     # Run dmcp as an MCP server (LLM integration)
├── orchestrator.rs # Concurrent task dispatch and tracking
├── sync_index.rs   # Sync registry embeddings into the local vector index
├── vector_index.rs # Semantic search (cosine similarity over embeddings)
├── doc_comments.rs # Extract @mcp.tool docstrings from Python servers (search-index fallback)
├── elevation.rs # Privilege elevation (Linux: pkexec/polkit; macOS: sudo/osascript)
└── models.rs    # Index, Manifest, Transport structs
```

## Connect

`dmcp connect` supports two modes:

1. **Manifest URL** — Fetches the URL as JSON. If valid (has `id` and `transports`), uses it and applies overrides.
2. **Raw fallback** — If fetch fails, treats URL as a raw SSE/WebSocket endpoint and auto-generates metadata.

## Status

Core features implemented: list, info, config, sources, browse (keyword + semantic search), install, uninstall, connect, run, setup, tools, call, serve (MCP server mode), count, sync-index, embedding-spec, index-server, paths.

## Platform support

A registry entry may declare `platforms` — `"linux"`, `"darwin"`, `"windows"` — the
platforms the registry has actually vetted that server on. dmcp maps the host via
`std::env::consts::OS` (`macos` → `darwin`) and refuses to install or refresh a
server whose list excludes the host, **before** any clone or setup script runs, with
a non-zero exit. `dmcp browse` marks such entries (`unsupported_on_host` in `--json`),
so an agent browsing the registry never proposes a server that cannot run here.

`--ignore-platform` overrides the refusal on `install` and `update`. It is the
intended path for verifying a server on a new OS — once it works, PR the platform
into the registry entry so everyone gets it.

An entry without `platforms` is unrestricted: pre-`platforms` manifests and
third-party registries install exactly as before.

## System-scoped servers

System-scoped MCP servers are installed under `/usr/share/mcp/installed/` and are
visible to every user on the machine.  Starting them requires root because the
install directory is owned by root.

### Why not sudo?

`sudo` cannot prompt for a password when dmcp has no TTY — for example when it is
launched by the JARVIS daemon or another MCP client.  Hardcoding `NOPASSWD` in
`/etc/sudoers` would skip authentication entirely, which is too broad.

### The polkit approach

dmcp uses `pkexec` (part of [polkit](https://gitlab.freedesktop.org/polkit/polkit))
to escalate privileges for system-scoped operations on Linux. (On macOS dmcp
elevates via `sudo -E` on an interactive TTY, or an osascript admin prompt
otherwise; other platforms do not support system scope yet.)  This is consistent
with how the rest of JARVIS OS handles privilege escalation.

The polkit action is defined in `policy/org.jarvisos.dmcp.policy`:

```
org.jarvisos.dmcp.run-system-server
```

Default behaviour:

| Session type | Result |
|---|---|
| Active desktop session | `auth_admin_keep` — user authenticates once, grant is cached for the task |
| Inactive or remote session | Denied |

When `dmcp run <id>` or `dmcp call <id> <tool>` targets a system-scoped stdio
server and the process is not already root, dmcp transparently re-executes
itself through `pkexec` under the same `org.jarvisos.dmcp.run-system-server`
action (system-scope writes like `install --system` and `config set` escalate
the same way).  polkit
presents an authentication dialog (or uses the cached grant) before handing control
back to the now-elevated dmcp process, which then spawns the server normally.

SSE and WebSocket servers only print a connection URL and never need root, so they
skip the escalation path entirely.

### Installation

Copy the policy file to the polkit actions directory:

```bash
sudo install -m 644 policy/org.jarvisos.dmcp.policy /usr/share/polkit-1/actions/
```

On Arch-based systems the PKGBUILD installs the policy file automatically;
the manual copy above is only needed for non-packaged installs.  The companion JS rules
in `packages/polkit/org.jarvisos.jarvis.rules` can be extended to grant `YES` for
members of the `jarvis-elevated` group, removing the password prompt for trusted
service accounts.

## LLM Integration

Run dmcp as an MCP server so LLMs (Cursor, Claude, etc.) can control it:

```bash
dmcp serve
```

Add to your MCP client config:

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

See [docs/LLM-INTEGRATION.md](docs/LLM-INTEGRATION.md) for details.

## References

- [Model Context Protocol](https://modelcontextprotocol.io/)
- [XDG Base Directory Specification](https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html)

## Changelog — corrected claims

*2026-07-22:* commands table completed (count, sync-index, embedding-spec, index-server; browse vector-search flags); project tree completed (call, serve, orchestrator, sync_index, vector_index, doc_comments); status list updated to the implemented surface; elevation notes cover `dmcp call` (#33) and per-OS behavior; PKGBUILD now installs the polkit policy file (manual copy only needed for non-packaged installs).

*2026-07-25:* registry `platforms` is enforced (#41) — install/update refuse a host the registry does not vouch for, `--ignore-platform` overrides, and browse marks unsupported entries; commands table updated with the new flags.
