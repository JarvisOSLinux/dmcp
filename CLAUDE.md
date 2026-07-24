# CLAUDE.md — dmcp

## What This Is

MCP Manager — a modular, system- and user-level manager for MCP (Model Context
Protocol) servers. Think of it as a package manager for MCP servers: discover,
install, configure, run, and invoke tools.

## Role in the JARVIS Ecosystem

dmcp is the server management layer. dispatch delegates to dmcp for server
discovery and tool invocation. Project-JARVIS's Python adapter calls dmcp
directly for server installation, configuration, and tool listing.

```
dispatch → dmcp → MCP servers (git, shell, browser, ...)
```

dmcp also runs as an MCP server itself (`dmcp serve`), allowing LLMs to
manage MCP servers through the MCP protocol.

## Tech Stack

- Rust (2021 edition)
- clap 4 for CLI
- rmcp 0.16 for MCP protocol (server, client, multiple transports)
- reqwest 0.12 (blocking, rustls-tls) for HTTP
- tokio 1 (full features) for async runtime
- serde / serde_json for serialization
- dotenvy for environment config
- dirs 5 for XDG paths
- sha2 for integrity verification
- schemars 1 for JSON Schema generation

## Architecture

```
src/
├── main.rs           CLI entry point (all subcommands)
├── lib.rs            Library root
├── paths.rs          XDG-compliant path resolution (user/system scopes)
├── discovery.rs      List servers, fetch manifests, load indices
├── sources.rs        Registry source URL management (sources.list)
├── browse.rs         Fetch registries, search by keyword
├── install.rs        Install/uninstall from registries or URLs
├── update.rs         Hash-drift detection + `dmcp update` (reuses the install flow + trust gates)
├── broker.rs         Session-scoped persistent-server broker (UDS/NDJSON) + `--session` thin client + `dmcp session`
├── connect.rs        Add remote servers (manifest URL or raw endpoint)
├── doc_comments.rs   Extract @mcp.tool docstrings from Python servers (description fallback for the search index)
├── config.rs         Per-server config get/set
├── run.rs            Spawn stdio servers, print SSE/WS URLs
├── setup.rs          Execute setup scripts
├── call.rs           Call tools on MCP servers
├── serve.rs          Run dmcp as MCP server for LLM integration
├── orchestrator.rs   Concurrent task dispatch and tracking
├── sync_index.rs     Sync local indices with installed servers
├── vector_index.rs   Semantic search with embeddings
├── models.rs         Core data structures (Index, Manifest, Transport)
├── elevation.rs      Privilege elevation for system scope (Linux: pkexec/polkit; macOS: sudo/osascript)
└── transport.rs      Transport extraction from manifests
```

### Dual-Scope Design

- **User scope**: `~/.local/share/mcp/`, `~/.config/mcp/` — no root required
- **System scope**: `/usr/share/mcp/`, `/etc/mcp/` — root via pkexec (Linux)

All paths are env-var-overridable before the XDG defaults apply
(`MCP_USER_SOURCES_PATH`, `MCP_USER_INSTALL_DIR`, `MCP_SYSTEM_SOURCES_PATH`,
`MCP_SYSTEM_INSTALL_DIR`, `MCP_VECTOR_INDEX_DIR`), loaded from `.env` via
dotenvy.

### Server Types

- **Local (stdio)**: Git repos cloned to disk, spawned as child processes
- **Remote (SSE/WebSocket)**: Metadata-only, connection endpoints stored locally

## Build & Test

```bash
cargo build --release
cargo install --path .   # Install to ~/.cargo/bin
cargo test
cargo clippy
cargo fmt --check
```

## Key Commands

```bash
dmcp list                          # List installed servers
dmcp browse                        # Browse registries
dmcp install <id>                  # Install from registry
dmcp update <id> | --all [--check] # Refresh servers whose registry manifest hash drifted
dmcp run <id>                      # Run a server
dmcp tools <id>                    # List tools on a server
dmcp call <id> <tool> --args '{}'  # Call a tool (one-shot: spawn, call once, kill)
dmcp call <id> <tool> --session <sid> # Keep a stateful user-scope server alive across calls (broker)
dmcp session list|close|gc         # Inspect / close live sessions; sweep idle ones
dmcp serve                         # Run dmcp as MCP server
dmcp sync-index                    # Cache registry embeddings locally
dmcp browse --vector '[...]'       # Semantic search against the local index
```

### Session broker (stateful servers)

A stateful server (`manifest.stateful: true`) holds state in-process — a browser,
a REPL, a DB connection — so the one-shot "spawn, call once, kill" lifecycle
throws it away between calls. `dmcp call --session <sid>` instead routes through a
long-lived **broker** that keeps one child process per `(server_id, session_id)`
alive until it is closed or its idle TTL (`DMCP_SESSION_TTL_SECS`, default 300s)
expires. The broker auto-starts on first use, listens on a 0600 Unix socket under
`$XDG_RUNTIME_DIR/dmcp/` (fallback `/tmp/dmcp-<uid>/`, dir 0700, uid-checked), and
serializes concurrent calls to one session. `--session` is gated to **stateful +
user scope** (system-scope sessions are refused — elevation safety); without it,
the one-shot path is untouched. See `src/broker.rs`.

## Specs & Docs

- `MCP-SYSTEM-SPEC.md` — Full path/format specification
- `MCP-REGISTRY-GUIDE.md` — Registry format and install flow
- `docs/LLM-INTEGRATION.md` — Configuring LLM clients to use dmcp

## Conventions

- `cargo fmt` + `cargo clippy` clean before pushing
- Commit messages: imperative mood
- No comments explaining what code does; only non-obvious WHY

## Changelog — corrected claims

*2026-07-22:* `doc_comments.rs` added to the tree; elevation described per-OS; env-var path overrides documented; semantic-search commands added to Key Commands; stale line count dropped.

*2026-07-24:* `update.rs` added — hash-drift detection and the `dmcp update` subcommand (single id / `--all`, `--check`, `--json`); reuses the install flow and trust gates. `browse` now surfaces `update_available` for drifted installed servers.

*2026-07-24:* `broker.rs` added — session-scoped persistent-server broker (#36). `Manifest.stateful` flag; `dmcp call --session <sid>` thin client (gated on stateful + user scope) over a UDS/NDJSON protocol; `dmcp broker` (hidden, auto-started) and `dmcp session list|close|gc`. Spawn/env/install-dir resolution is factored into `call::build_stdio_command` / `call::resolve_stdio_install_dir`, shared by the one-shot path, `run`, and the broker. Integration tests use a stdlib-only fake stateful MCP server (`tests/fixtures/fake_stateful_server.py`).
