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
├── serve.rs          Run dmcp as MCP server for LLM integration (incl. `update_server` + drift fields)
├── orchestrator.rs   Concurrent task dispatch and tracking
├── sync_index.rs     Sync local indices with installed servers
├── vector_index.rs   Semantic search with embeddings
├── models.rs         Core data structures (Index, Manifest, Transport)
├── platform.rs       Host platform identity + the registry `platforms` gate
├── elevation.rs      Privilege elevation for system scope (Linux: pkexec/polkit; macOS: sudo/osascript)
└── transport.rs      Per-host transport selection + transport-type extraction
```

### Dual-Scope Design

- **User scope**: `~/.local/share/mcp/`, `~/.config/mcp/` — no root required
- **System scope**: `/usr/share/mcp/`, `/etc/mcp/` — root via pkexec (Linux)

All paths are env-var-overridable before the XDG defaults apply
(`MCP_USER_SOURCES_PATH`, `MCP_USER_INSTALL_DIR`, `MCP_SYSTEM_SOURCES_PATH`,
`MCP_SYSTEM_INSTALL_DIR`, `MCP_VECTOR_INDEX_DIR`), loaded from `.env` via
dotenvy.

**System-scope elevation is a CLI-only capability.** The one-shot `dmcp call`
re-execs through pkexec before the runtime, so a system-scope **stdio** server's
tools run as root. The agent surface (`dmcp serve`, and the orchestrator behind
`dispatch_tasks`) cannot do this — it is long-lived and possibly headless, so it
can neither re-exec (that would replace the daemon) nor raise a per-call polkit
prompt. `call::call_tool` therefore **refuses** a system-scope stdio server when
the process is not already elevated (`refuse_unelevated_system_stdio`, keyed on
the same `needs_system_elevation` predicate that drives the CLI's re-exec, so the
two cannot drift), rather than silently running it at the invoking user's uid.
The CLI is unaffected (already root by the time `call_tool` runs); a `dmcp serve`
deliberately started as root is already elevated and proceeds. User-scope tools
and remote transports are never refused.

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
dmcp install <id> --ignore-platform # Install despite an unvouched host (see Platform support)
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

### Platform support (registry `platforms`)

A registry entry may declare `platforms` — `"linux"` | `"darwin"` | `"windows"` —
the platforms the registry has vetted the server on. `src/platform.rs` is the one
place that maps the host (`std::env::consts::OS`, `macos` → `darwin`) and reads the
field; `install` (by id **and** by manifest URL), `connect` and `update` refuse an
excluded host **before** any directory, clone or setup (non-zero exit),
`--ignore-platform` overrides on all three, and `browse` marks such entries
(`unsupported_on_host`, plus `platforms`, in the table and `--json` — the
`--json` shape is what reaches the agent through JARVIS's `search_servers`).
Both browse modes mark: the keyword one from the registry entry, the
`--vector`/`--vectors` one from the platform state `sync-index` copies into the
vector index (the surface dispatch's `browse_servers` actually calls).
`update --check --json` rows carry the same state. The agent path (`dmcp serve`)
has no override. `dmcp install <id>` refuses ahead of the pkexec re-exec, so an
unvouched host never costs a polkit prompt; `install()` gates again for library
callers.

`platform::PlatformDecl` is the one reading of the field, for raw JSON and for
the typed `Manifest`/`Transport` alike: **Absent** (key missing, `null`, or a
list that trims to nothing) is unrestricted — today's behavior, unchanged;
**Malformed** (anything that is not an array of platform names) covers no host,
because a gate that cannot parse its input must not conclude "no restriction";
**Declared** is the list. Deserializing never fails, so a slip in this field
cannot make a manifest unloadable and strand an installed server that `list`,
`info` and `uninstall` can no longer see. Uninstall reads the index rather than
the manifest for the same reason.

A **transport** may declare `platforms` too — one server entry, one launch line
per OS (`python3` vs `python`). `src/transport.rs::select` is the only selection
path: it returns the first transport the host is in (absent matches every host,
malformed matches none) and otherwise errors naming the declared platforms,
never falling through to entry zero. Every spawn site goes through it — one-shot
`call`, `tools`, `run`, the session broker — plus the install clone/remote
decision. The listing surfaces (`browse`, `list`) fall back to entry zero on
purpose: they describe a server, they do not start it, so a foreign-only server
still shows the transport it would launch elsewhere instead of `?`. Setup
scripts split the same way: `setupScript` (POSIX) vs `setupScriptWindows`
(`setup.ps1`, run through PowerShell), chosen by `setup::script_for_host` at all
three call sites (`install`, `connect`, `dmcp setup`) and delivered through the
one SHA-256 gate in `install.rs`. On Unix the script's shebang decides `sh` vs
`bash`, with `sh` as the announced fallback where bash is not installed.

### Drift on the agent surface (`dmcp serve`)

Detection (`update.rs`) reaches the agent through two places in `src/serve.rs`,
both feeding on one registry fetch via `update::assess_servers`:

- **`update_server`** — **id-only**, like `install_server`: no URL, no path, no
  source, so refreshing cannot widen the set of registries the agent installs
  from (TRUST-MODEL §2.2). `serve::update_decision` is the whole policy, split
  out from the tool body to be testable without an MCP client: registry entry
  gone **or** `trustStatus: "removed"` → refuse and advise `uninstall_server` (a
  revoked server is uninstalled, never refreshed); no drift → "already up to
  date"; drift → `update::refresh_install` and report `old -> new` hash.
  Trust gating is the **CLI** gate (`trust_gate_for_update`), not
  `agent_trust_gate`: the agent gate stops an agent *adopting* a deprecated or
  unreviewed server, and a refresh adopts nothing — a human already installed
  this id, and refusing would pin the box to the older manifest of the server
  whose drift is often the fix. So `deprecated` warns and proceeds (the warning
  prefixes the success text), `removed` is refused either way. `refresh_install`
  is called with `ignore_platform: false` — the agent path has no override, per
  Platform support above.
- **`get_server_info`** — carries `update_available`, `revoked` and
  `trust_status` for an installed server, so the drift signal exists at the
  point of use and not only in `browse` search results. **Best-effort**: any
  fetch failure (unreachable registry, no sources configured) omits all three
  silently rather than failing a read of local metadata.

The `dmcp serve` instructions state the retry rule: if a call to a server with
`update_available: true` fails, `update_server` once and retry.

## Specs & Docs

- `MCP-SYSTEM-SPEC.md` — Full path/format specification
- `MCP-REGISTRY-GUIDE.md` — Registry format and install flow
- `docs/LLM-INTEGRATION.md` — Configuring LLM clients to use dmcp

## Conventions

- `cargo fmt` + `cargo clippy` clean before pushing
- Commit messages: imperative mood
- No comments explaining what code does; only non-obvious WHY

## Changelog — corrected claims

*2026-07-30:* the agent surface refuses system-scope stdio it cannot elevate (#45). `call::call_tool` now gates on `refuse_unelevated_system_stdio` (new `CallError::SystemScopeRequiresElevation`) before spawning, so a system-scope stdio server invoked from `dmcp serve` / `dispatch_tasks` is refused with a clear error instead of silently running at the invoking user's uid. Keyed on the same `needs_system_elevation` predicate as the CLI's pkexec re-exec, so the CLI (already root by `call_tool`) and the agent surface cannot disagree on what needs root; user-scope and remote transports are untouched. The chokepoint covers every unprivileged `call_tool` caller (serve, orchestrator, future lib callers). Introspection (`list_tools`) stays unprivileged.

*2026-07-25:* drift reaches the agent (#39, dmcp half). `serve.rs` gains `update_server` (id-only, mirroring `install_server`'s confinement) plus `serve::update_decision`, the extracted policy: disowned/`removed` → refuse + advise `uninstall_server`, no drift → up to date, drift → `update::refresh_install` with `ignore_platform: false` and an `old -> new` hash in the result; `deprecated` warns and proceeds because the update path uses `trust_gate_for_update` (CLI gate), not `agent_trust_gate`. `get_server_info` carries best-effort `update_available` / `revoked` / `trust_status` from a live registry read, omitted silently on any fetch failure. `update::tests` is `pub(crate)` so the serve tests reuse its `TempTree` + `file://` registry fixtures instead of copying them. The daemon-side half (a Project-JARVIS `update_server` action, the periodic `--check --all` sweep) is not in this repo.

*2026-07-25:* the vector-search surface carries the platform state. `VectorEntry.platforms` / `platforms_malformed` are copied from the registry entry by `sync-index` (both server- and tool-level entries, read through `platform::platform_decl`); `SearchResult` gains `platforms`, `platforms_malformed` and an always-serialized `unsupported_on_host`, computed at search time so a copied index still answers for the host doing the searching. `dmcp browse --vector`/`--vectors` mark in the table and in `--json`. **Migration:** an `index.json` synced before this reads as unrestricted until `dmcp sync-index` runs again. Locally indexed entries (`dmcp index-server`) declare nothing and stay unrestricted. `setup::run_setup` refuses a POSIX script on a Windows host with `SetupError::NoWindowsScript` instead of handing `setup.sh` to `powershell.exe -File`, which could only fail on the extension.

*2026-07-25:* `platform::PlatformDecl` — one three-state reading (absent / malformed / declared) shared by the raw-JSON and typed views; `Manifest.platforms` and `Transport.platforms` deserialize through it and never fail. `connect` takes `ignore_platform` and gates the fetched manifest, so `dmcp install <url>` and `dmcp connect --ignore-platform` behave like the by-id path; `Commands::Install` refuses before the elevation prompt; `connect` selects its setup script with `setup::script_for_host`; `setup::run_setup` falls back from `bash` to `sh` and names the interpreter it failed to spawn.

*2026-07-22:* `doc_comments.rs` added to the tree; elevation described per-OS; env-var path overrides documented; semantic-search commands added to Key Commands; stale line count dropped.

*2026-07-24:* `update.rs` added — hash-drift detection and the `dmcp update` subcommand (single id / `--all`, `--check`, `--json`); reuses the install flow and trust gates. `browse` now surfaces `update_available` for drifted installed servers.

*2026-07-25:* per-transport `platforms` (#42). `Transport::platforms()` plus `transport::select` / `select_for_host` (host injectable) — the single selection path used by `call`, `list_tools`, `run`, the broker, `install`'s transport-type read, and the `list`/`browse` displays; `CallError::NoTransportForHost` / `RunError::NoTransportForHost` carry the refusal. `Manifest.setup_script_windows` + `SetupScriptSpec` in `install.rs` (POSIX/Windows share one download-verify-write gate, `integrity.setupScriptWindowsSha256`); `setup.rs` chooses PowerShell for `.ps1`/Windows and honours a bash shebang instead of always invoking `sh`.

*2026-07-25:* `platform.rs` added — host detection (`macos` → `darwin`) and the registry `platforms` gate (#41). `Manifest.platforms`; `install`/`update` refuse an unvouched host before any clone or setup, with `--ignore-platform` on both; `RegistryServer` (browse) and `DriftReport` (`update --check --json`) carry `platforms` + `unsupported_on_host`. `install::install` and `update::refresh_install` take an `ignore_platform` argument; the `dmcp serve` agent path passes `false`.

*2026-07-24:* `broker.rs` added — session-scoped persistent-server broker (#36). `Manifest.stateful` flag; `dmcp call --session <sid>` thin client (gated on stateful + user scope) over a UDS/NDJSON protocol; `dmcp broker` (hidden, auto-started) and `dmcp session list|close|gc`. Spawn/env/install-dir resolution is factored into `call::build_stdio_command` / `call::resolve_stdio_install_dir`, shared by the one-shot path, `run`, and the broker. Integration tests use a stdlib-only fake stateful MCP server (`tests/fixtures/fake_stateful_server.py`).
