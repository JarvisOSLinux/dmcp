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
├── manifest_io.rs    Atomic, owner-only writes for the manifests that hold server credentials
├── run.rs            Spawn stdio servers, print SSE/WS URLs
├── pathext.rs        Resolve a launch command via PATH + PATHEXT on Windows (call, broker, run)
├── setup.rs          Execute setup scripts
├── call.rs           Call tools on MCP servers
├── serve.rs          Run dmcp as MCP server for LLM integration (incl. `update_server` + drift fields)
├── orchestrator.rs   Concurrent task dispatch and tracking
├── sync_index.rs     Sync local indices with installed servers
├── vector_index.rs   Semantic search with embeddings
├── models.rs         Core data structures (Index, Manifest, Transport)
├── platform.rs       Host platform identity + the registry `platforms` gate
├── elevation.rs      Privilege elevation for system scope (Linux: pkexec/polkit; macOS: sudo/osascript)
├── elicit.rs         Inbound `elicitation/create`: a server asking for input mid-tool-call
├── execution.rs      Execution backends: the parsed `execution` block + the one stdio spawn plan
└── transport.rs      Per-host transport selection + transport-type extraction
```

### Dual-Scope Design

- **User scope**: `~/.local/share/mcp/`, `~/.config/mcp/` — no root required
- **System scope**: `/usr/share/mcp/`, `/etc/mcp/` — root via pkexec (Linux)

All paths are env-var-overridable before the XDG defaults apply
(`MCP_USER_SOURCES_PATH`, `MCP_USER_INSTALL_DIR`, `MCP_SYSTEM_SOURCES_PATH`,
`MCP_SYSTEM_INSTALL_DIR`, `MCP_VECTOR_INDEX_DIR`), loaded from `.env` via
dotenvy, except in the root half of an elevation re-exec
(`elevation::is_root_half`).

**System-scope elevation reaches the agent surface by delegation.** The one-shot
`dmcp call` re-execs through pkexec before the runtime, so a system-scope
**stdio** server's tools run as root. `dmcp serve` cannot re-exec *itself* — that
would replace the daemon — but it does not need to: `call::call_tool` spawns a
child `dmcp call`, which performs that re-exec on its own and lets polkit raise
the prompt (the action sets `allow_gui`, precisely so a caller with no TTY can be
answered graphically). The child runs the server as root, prints the result, and
exits, so **the privilege dies with the command** instead of becoming a standing
capability. Same shape dispatch already uses to reach dmcp.

`plan_elevation` is the whole policy — pure, and keyed on the same
`needs_system_elevation` predicate that drives the CLI's re-exec, so the two
surfaces cannot drift on what "needs root" means:

- **Direct** — user scope, a remote transport, or already root (the CLI by the
  time it reaches `call_tool`; a `dmcp serve` deliberately started as root).
- **Delegate** — unelevated + system scope + stdio: spawn the child.
- **Refuse** — the child is *itself* a delegate (`DMCP_ELEVATION_DELEGATED`) and
  still could not elevate. Without this a polkit denial would recurse.

The child's exit carries the outcome exactly as `dmcp call` reports it: 0 is
success, 2 is a tool-reported error (status rides the exit code, never a sentinel
in the output), anything else is `ElevationFailed` with the child's stderr — so a
polkit denial is reported rather than downgraded into an unprivileged run. That
stderr is relayed, not parked: during any call, delegated or direct, the child's
stderr is teed onto dmcp's own in raw chunks as it arrives (`call::relay_stderr`)
while a retained copy still supplies the failure detail, so a caller tailing dmcp
(dispatch's live task tail) sees the chatter mid-call, not after it. polkit
**denies a non-active session outright** (`allow_inactive: no`) rather than
prompting, so SSH/headless/system-unit callers land here. `DMCP_ELEVATION_TIMEOUT_SECS`
(default 180) bounds **only the pre-authentication window**, not the command's
own runtime: the re-exec'd, now-root child emits an internal sentinel to stderr
the moment it is past pkexec, and the parent's relay cancels the deadline on
seeing it (and strips it from what it tees onward), so an authorized long-running
job — `pacman -Syu` — runs to completion instead of being killed at 180s with a
false "auth never completed". The deadline firing therefore means authentication
genuinely did not complete, and its message says so; the child owns its process
group and is killed on drop, but a job holder that detached itself (double-fork +
setsid) can outlive that kill, which the message also states rather than implying
the operation stopped. The delegated argv carries no `--session`: an elevated
server must not outlive the command that needed it, which is also why `--session`
stays user-scope-only.

The re-exec itself is `pkexec <dmcp> <args>`, **not** `pkexec env HOME=… <dmcp> …`:
pkexec matches its polkit action by the program it is asked to exec, so the env
wrapper matched `/usr/bin/env`, never the annotated `/usr/bin/dmcp`, and the
custom action (`allow_gui`, `auth_admin_keep`) never fired. HOME is instead
restored **inside** dmcp — `elevation::restore_invoking_user_home`, the first
statement of `main` (before dotenvy, `Paths::resolve`, and any thread), maps
`PKEXEC_UID` back to the invoking user's home so config resolution reads their
`~/.config/mcp/sources.list`, not `/root`'s. It is guarded on euid 0 + a present
`PKEXEC_UID`, so a deliberately-root `dmcp serve` is untouched, and any
unresolvable uid leaves HOME as-is rather than forcing `/root`.

**sudo instead of pkexec (Project-JARVIS#310).** When `DMCP_SUDO_ASKPASS` names a
program and that program answers `--check` with success, the re-exec (and the
`cp`/`rm -rf` of `write_file_elevated`/`remove_dir_elevated`) runs
`sudo -A -- <program> …` with `SUDO_ASKPASS` set to it, and the waiting notice
ends "(sudo askpass)" instead of "(polkit prompt)". Anything else (unset, empty,
missing, failing, not answering) keeps pkexec. It is asked at every elevation,
so whoever runs the program (JARVIS's password popup) can turn it on and off
without restarting dmcp. The root half restores HOME from `SUDO_UID` only when
argv carries `--dmcp-internal-sudo-reexec` (stripped before clap, like the
sentinel flag): sudo resets the environment, and a `SUDO_UID` alone may come from
a human's `sudo bash` further up the tree.

`auth_admin_keep` in the policy means one password covers a short window; TLA
still confirms each privileged command upstream, so informed consent is
per-command and proof-of-presence is per-window.

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
dmcp update <id> | --all [--check] # Refresh servers whose registry manifest or setup-script hash drifted
dmcp install <id> --ignore-platform # Install despite an unvouched host (see Platform support)
dmcp run <id>                      # Run a server
dmcp tools <id>                    # List tools on a server
dmcp call <id> <tool> --args '{}'  # Call a tool (one-shot: spawn, call once, kill)
dmcp config <id> set <KEY> --stdin # Set a value read from stdin, not echoed (secrets: argv is world-readable)
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

### Mid-call elicitation (a server asking a question)

Some operations cannot be made non-interactive and cannot be answered up front,
because they ask a **sequence** of questions that only appears as the work
unfolds — `fdisk`, `mysql_secure_installation`, a REPL, an installer with no
`-y`. `--noconfirm` handles the single-prompt case and cannot get past step 1 of
a wizard. MCP's `elicitation/create` is the answer: the tool call does **not**
return while the question is outstanding, so the process stays alive and there
is nothing to reattach to.

`src/elicit.rs` is dmcp's client side. Two rules:

- **Only advertise it where it can be answered.** Capability negotiation is a
  promise a server checks before asking. The one-shot `dmcp call` path spawns,
  calls once, and exits with nobody to ask, so `ServerClient::unattended`
  declares nothing and declines. Only the brokered path uses `attended`, and it
  declares **form mode only** — URL mode asks the client to open a browser,
  which a headless daemon cannot promise.
- **The prompt text is untrusted.** It is authored by whoever wrote the server.
  Every `PromptRequest` carries the id of the server that asked, so a renderer
  attributes the question instead of speaking it in JARVIS's voice. Nothing here
  invents an answer; a password-shaped prompt is a human's decision upstream.

**Every path ends in an answer.** No driver, a dropped channel, a hung-up
socket, a malformed reply, or a spent budget all resolve to a *decline* — a real
protocol outcome the server must already handle — because silence would park the
session on a question nobody will ever see. `DMCP_ELICIT_MAX_PROMPTS`
(default 16) bounds one server's questions so a looping server cannot hold a
session, or the human's attention, hostage.

**In the broker**, the prompt receiver lives beside the cached client in the
session slot (the client outlives any one connection; calls to a session are
serialized, so exactly one in-flight call drains it). `drive_call` selects over
the call future and the prompt channel **concurrently** — a server that elicits
is blocked until answered, so awaiting the call first would deadlock against the
question that has to be answered to finish it. The select is `biased` so a
finished call wins a race with a late prompt.

On the wire a call becomes an exchange: interim `prompt` responses out
(`BrokerResponse::is_prompt`), `answer` ops back, then the final response.

**For a caller driving dmcp as a subprocess**, `dmcp call --session <sid>
--interactive` turns stdio into that channel: every stdout line is a JSON object
tagged `type` (`prompt`, then `result`), and one JSON answer line on stdin
resolves each prompt. All-JSON is deliberate — mixing a JSON prompt with a
bare-text result on one stream would leave the reader guessing. Without the
flag, output is byte-identical to today and prompts are declined.

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

### Execution backends (`execution` on a transport)

A stdio transport may declare **where** its launch line runs. `src/execution.rs`
is the whole feature: the parsed block, the argv it produces, and the one
`SpawnPlan` every stdio spawn site builds its process from
(`call::stdio_spawn_plan` → the one-shot `call`, `tools`, `run`, and the session
broker). Per-transport, like `platforms`, because the backend belongs to the
launch line it modifies.

Two mutually exclusive forms:

```json
"execution": {"type": "docker", "image": "python:3-slim",
              "mountInstallDir": "ro", "extraArgs": ["--cpus", "1"]}
"execution": {"wrapper": ["ssh", "build-host", "--"]}
```

- **docker** → `docker run [--env-file <file>] -i --rm [--mount type=bind,"src=<dir>","dst=<dir>"[,readonly]]
  -w <dir> [extraArgs …] <image> <command> <args …>`. `mountInstallDir` defaults
  to `"ro"`; `"none"` omits the mount but keeps `-w`. `--mount`, not `-v`: `-v`
  splits at every `:`; the CSV fields are quoted, so `,` and `"` survive too. The
  config is `SpawnPlan::container_env`, never `env`: `SpawnPlan::prepare` writes
  it to a 0600 env file in the broker's directory and splices `--env-file` in
  after `run`, and the returned `EnvFile` removes it on drop. Every spawn site
  holds it until the handshake (`run` until the child exits). So config
  **values** never enter argv (`/proc/<pid>/cmdline`), and a config key named
  `DOCKER_HOST`/`HOME`/`PATH` reaches the container without redirecting the
  docker client. What docker's env-file format cannot carry (a line break in a
  value; `=`, whitespace or a leading `#` in a key) fails the spawn by key name.
- **wrapper** → `wrapper… <command> <args …>`, an argv prefix dmcp knows nothing
  about. The config env is set on the **wrapper** process and goes no further by
  itself: an `ssh` hop does not carry it, and saying so is the contract.

`cwd` stays the host install dir in both cases — harmless under docker (`-w`
governs inside) and needed so a relative wrapper script resolves.

**Parse-level rejection, never silent ignore** (`deny_unknown_fields` on the
execution struct, plus form validation): both forms at once, an empty `wrapper`,
`docker` without `image`, docker-only keys beside `wrapper`, an unsupported
`type`, neither form, or an unknown key. This is deliberately stricter than
`platforms`, which degrades to "covers no host": absence of `execution` means
"spawn unwrapped on the host", so a typo (`"imgae"`) reading as absent is the one
outcome a manifest asking for a container must never get. The cost is that such a
manifest does not parse at all — `dmcp list` warns by name, and `uninstall` reads
the index, so the server is still removable.

**Install refuses a manifest it could not load** (`Manifest::load_error`, in
`install` and `connect`, ahead of the wipe), and `discovery::get_server` answers
from the first scope whose index lists the id, so a user-scope manifest that does
not load is "not found" rather than resolving to a same-id system-scope server.
`info`, `list`, `browse` (and `--json`) and `get_server_info` show the backend
through `Execution::describe`, extra docker flags and wrapper argv in full.

**System scope + `execution` is refused** (`execution::system_scope_refusal`),
before any spawn *and* before the elevation re-exec, on all of `call`, `tools` and
`run`: a system-scope server is launched by re-execing dmcp as root (pkexec, or
`sudo -A` when `DMCP_SUDO_ASKPASS` answers), and
the elevated process would be the docker client or the wrapper, not the server,
so "the tool runs as root" would silently stop being true. `needs_system_elevation`
is unchanged — the guard stops the re-exec, rather than the predicate quietly
deciding such a server no longer needs root.

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

*2026-11-18:* execution backends, after Toufic's review of #63. **Install:** `install` and `connect` copied the manifest JSON to disk unparsed, so a malformed `execution` block installed cleanly (after wiping any working copy) and was "Server not found" from then on; both now refuse up front through `Manifest::load_error`. `discovery::get_server`/`get_manifest_path` no longer fall through to system scope when the user scope lists the id but its manifest does not load, which resolved the name to a same-id server running as root. **Docker env:** config and the docker client shared one environment (bare `-e KEY` read from the client's own), so a setting named `DOCKER_HOST`, `DOCKER_CONFIG`, `HOME` or `PATH` redirected docker itself. `SpawnPlan` now carries `container_env` separately; `SpawnPlan::prepare` writes it to a 0600 `--env-file` in the broker's directory, held by an `EnvFile` guard until the handshake (`run`: until exit), and the docker client gets no config at all. What the env-file format cannot carry is refused by key name rather than dropped. **Mount:** `-v dir:dir:mode` could not express a `:` in the path; `--mount type=bind` with quoted CSV fields can, and survives `,` and `"`. **Visibility:** `info`, `list`, `browse` (and its `--json`) and `get_server_info` show the backend via `Execution::describe`, extra docker flags and wrapper argv in full. **Tests:** `plan_for` asserts on `stdio_spawn_plan`, which does no PATH lookup, so the planning tests pass on Windows; the real-container test now uses an install dir containing `:` and `,` and a `DOCKER_HOST` setting pointing nowhere, which reaches the container while docker still reaches its daemon. Not yet addressed: an older dmcp ignores `execution` and launches on the host (needs a format decision), container cleanup on kill, first-run image pulls inside the handshake, and wrapper quoting.

*2026-11-17:* execution backends (#62). `src/execution.rs` added: `Execution` (validated `Docker`/`Wrapper` variants, parsed through a `deny_unknown_fields` repr so a typo can never read as "no backend"), `plan_spawn` → `SpawnPlan` (program, argv, cwd, env), and `system_scope_refusal`. `Transport::Stdio` gains `execution` plus a `Transport::execution()` accessor; `call::stdio_spawn_plan` is the single decision `build_stdio_command` (one-shot `call`, `tools`, the broker) and `run::run_stdio` both build from, so `dmcp run` cannot be the one surface that ignores a container. Docker argv is `run -i --rm [-v dir:dir:ro|rw] -w dir [-e KEY …] [extraArgs …] image command args…`, with config **values** carried by the environment (bare `-e KEY`, keys sorted, `=`-bearing keys dropped) rather than argv; a wrapper is a plain prefix and the config env stops at the wrapper process. System scope plus a backend is refused before the spawn and before the pkexec re-exec in `call_tool`, `elevate_call_for_system_scope` and `run`, so nobody authenticates their way to a refusal. Verified red-then-green: the integration file does not compile against the pre-feature source; targeted mutations (dropping `deny_unknown_fields`, the mutual-exclusion arm, the required-field checks, the bare `-e` form, the `extraArgs` position, the scope guard, and honouring `execution` at all) each fail exactly the test that names the behaviour, while the no-execution regression pin keeps passing. `tests/execution_backends.rs` runs a real container when `docker ps` works and the image is present or pullable (skips cleanly otherwise), asserting the launch line ran inside the container, the install dir is mounted and current, the config value arrived, and the value is absent from argv. Merged onto the November elevation and Windows work: `pathext::launch_program` resolves only the program the host starts (`docker` or the wrapper's first word, never the launch line that runs inside), the config env is `config_to_env(manifest)`'s declared keys, so an undeclared stored key reaches a container no more than the host, and the system-scope refusal stands in front of `re_exec_elevated`, so it saves a `sudo -A` popup as well as a polkit prompt.

*2026-11-15:* the root half of an elevation re-exec no longer reads `.env`. `main` ran `dotenvy::dotenv()` in every process, and `sudo -A` keeps the caller's working directory, so a `.env` in that directory or a parent set `LD_PRELOAD`, `PATH` or the `MCP_*` paths for root and for the server root spawned. pkexec changes to `/root` first, so only root-owned `.env` files could reach it there, but it is skipped too. `elevation::is_root_half` is the check `announce_auth_granted` already made (euid 0 and `PKEXEC_UID`, or `SUDO_UID` behind `SUDO_REEXEC_FLAG`); a dmcp deliberately started as root still reads `.env`. `tests/dotenv_root_half.rs` runs dmcp as root in a user namespace (`unshare --map-root-user`) with each marker, plus a user run and a marker-less root run that must still load it.

*2026-11-15:* `dmcp run` resolves `.cmd`/`.bat` shims on Windows, and a miss names where it looked (gaps left by #66). Only `call::build_stdio_command` (the one-shot call and the broker) went through the PATHEXT resolver; `run::run_stdio` spawned the manifest's command as written, so `dmcp run` of an `npx`/`uvx` server still failed with "Command not found". Both now spawn `pathext::launch_program`, which replaced call.rs's private copy. `NotFound` printed only a count ("2 directories searched"); it now lists the directories, or says PATH is unset or empty. Tests: the message cases on every host, and `run_stdio` launching `srv.cmd` named as `srv` (Windows only; checked locally under wine).

*2026-11-15:* a refused `sudo -A` elevation says so instead of blaming polkit (Project-JARVIS#310). On the delegated path (`call_tool_elevated`), a child that elevated with `sudo -A` and was cancelled or refused (JARVIS's popup kills that sudo on cancel) came back as `ElevationFailed`, whose text explains polkit denying an inactive session. `result_from_cli_exit` now reads the child's own `AUTH_WAITING_NOTICE_SUDO` line and returns `SudoElevationFailed`, which names sudo and the askpass program in `DMCP_SUDO_ASKPASS`; pkexec's text is unchanged. That notice is printed before sudo or pkexec asks, so it is in stderr after an accepted password too; when the elevation sentinel went by (`RetainedStderr::authenticated`, or the `PreAuth::Authenticated` arm), a non-0/2 exit is `ToolCallFailed` "was elevated (the password was accepted), then failed: …" for either program, never a refused password.

*2026-11-11:* elevation may use `sudo -A` with an askpass program instead of pkexec (Project-JARVIS#310): JARVIS asks for the sudo password in jarvis-app and on phones when its `SUDO_PASSWORD_POPUP` is on, since polkit's dialog shows only on the host's desktop. `elevation::elevating` picks sudo when `DMCP_SUDO_ASKPASS` answers `--check`, pkexec otherwise, for the re-exec and the elevated `cp`/`rm`; `re_exec_with_pkexec` is now `re_exec_elevated`. `invoking_uid` reads `PKEXEC_UID`, or `SUDO_UID` behind the new internal flag. Tests: the HOME policy for both, a foreign `SUDO_UID` ignored, and pkexec kept for an unset, empty, missing or refusing askpass program.

*2026-10-19:* a setup script can read its own `manifest.json`. `install` (and `update`, which reuses it) wrote the manifest only after the setup script ran, into a directory it had just recreated, so the registry's remote-server setup template, which reads `./manifest.json` for config and transport URLs, failed on every install and update (hello-sse). The manifest is now written before the script runs, as `connect` already did, and again afterwards with the setup's outcome. Test: a setup script that requires `installDir` in `./manifest.json` installs cleanly.

*2026-10-19:* `dmcp update` sees a change to a server's setup script, not only to its manifest. The registry fixed fetch-guard by capping `mcp<2` in its `setup.sh` (mcp-registry#121), and `update --check` called the broken install up to date: drift compared only `integrity.manifestSha256`. The installed manifest already records the registry's `integrity` from install time, so `update::setup_hash` reads the hash of the setup script this host runs (`setupScriptSha256`, or `setupScriptWindowsSha256` on Windows) from both sides, `RegistryEntryInfo.setup_sha256` carries the registry's, and `assess` (hence `update`, `update --check`, the agent's `update_server` and `get_server_info`) and `browse`'s `update_available` flag either difference. A registry entry with no setup hash asserts nothing, as with the manifest hash. `update --check` says "the manifest is unchanged; its setup script changed" instead of printing two equal hashes. Tests: the comparison's four cases, the host's key read from a registry entry, and `assess_servers` against an on-disk registry where only the setup hash moved.

*2026-10-18:* `config set` checks a value against its setting's type (mcp-registry#115). A `configurableProperties` entry may declare `type` (`boolean` with optional `values` spellings, `integer` with `min`/`max`, `choice` with `choices` and optional `multiple`/`separator`); `config::value_error` mirrors the registry validator's `property_value_error`, and `set_config_value` refuses an invalid value before writing (`READ_ONLY must be 'true' or 'false'`), on the CLI and through `dmcp serve`'s `set_config` alike, so a value set by hand, by JARVIS or by an agent gets the same check. A setting with no type, or one this dmcp does not know, takes any string: the registry gate checks types, and refusing a newer one would strand a server whose manifest is newer than dmcp. Tests: every type's accept/refuse cases, the manifest left untouched on a refusal, and the CLI refusing a port above 65535.

*2026-10-17:* a server gets only the settings its manifest declares. `dmcp config set` accepted any key and every spawn passed the whole `config` map as environment variables, so whatever could write a setting could set `NODE_OPTIONS` (`--import=data:…` runs code in any Node server) or `LD_PRELOAD` in the server. JARVIS's `configure_server` passes keys the model chooses, so a prompt injection could reach that without a confirmation prompt. `set_config_value` (the CLI and the `dmcp serve` `set_config` tool) now refuses a key `configurableProperties` doesn't list, naming the ones it does; `run::config_to_env` takes the manifest and leaves out a stored undeclared key, naming it on stderr. Setup scripts are unchanged: they get `MCP_CONFIG_<KEY>`, which no loader reads. Tests: the refusal in the library and the CLI, and `build_stdio_command`'s environment for a manifest storing a declared and an undeclared key.

*2026-10-15:* `dmcp config <id> set <KEY> --stdin` reads the value from standard input and prints only the key. JARVIS wrote server secrets as `config set KEY VALUE`, so an API key sat in the process's command line, readable by any local user while dmcp ran, and was echoed back as `Set KEY = value`. For a system-scope server the elevation check runs before stdin is read, because the pkexec re-exec reads stdin itself. The positional value still works; giving both, or neither, is an error. `tests/config_set_stdin.rs` covers it.

*2026-10-15:* `dmcp update` (and the agent's `update_server`) no longer wipes a server's configuration. `refresh_install` reinstalls from the registry's manifest, which carries no `config`, and the installed manifest is where `dmcp config set` values and API keys live, so every update silently dropped them. Found when updating the Microsoft 365 wrapper reset its read-only setting. `refresh_install_from` now copies the installed `config` into the new manifest before installing, so the setup script also runs with it, as at first install. Tested with a remote fixture server that is refreshed with and without settings.

*2026-10-14:* signed-in accounts are gone (Project-JARVIS#229, design v3). #69 had dmcp store accounts in the keyring, grant them to servers and inject them at spawn, with `dmcp login` running the device flow. v3 moves sign-in into each server instead: a server that works in the user's account ships its own `login` tool (a per-server wrapper in mcp-registry when the upstream has none) and keeps its own tokens. dmcp only spawns it and relays its stderr, which carries the sign-in link. This reverts #69: `accounts.rs`, `login.rs`, the `login`/`logout`/`grant`/`accounts` commands, credential injection in `build_stdio_command` (sync again), `credential_required` and exit 3, `Manifest.credentials`/`login`, and the keyring crates. A manifest that still carries `credentials` is read as before #69: the key is ignored.

*2026-08-01:* two elevation fixes (#52 polkit action, #51 timeout scope). **#52:** the polkit action never matched. `elevation::re_exec_with_pkexec` invoked `pkexec env HOME=… dmcp …`, so pkexec mapped the action by `/usr/bin/env`, not the `/usr/bin/dmcp` the policy annotates — `org.jarvisos.dmcp.run-system-server` (its `allow_gui` and `auth_admin_keep`) never fired and elevation fell back to the generic exec action, losing the graphical prompt a TTY-less `dmcp serve`/dispatch caller needs and the one-password window. The re-exec now runs `pkexec <dmcp> <args>` directly (`current_exe()` → `/proc/self/exe` → the annotated path for a packaged install); pointing the annotation at `/usr/bin/env` instead was rejected as it would grant the keep-window to every `pkexec env …` on the box. HOME — which the env wrapper carried so the elevated dmcp read the user's `sources.list`, not `/root`'s — is restored **inside** dmcp: `restore_invoking_user_home`, the first statement of `main` (before dotenvy, `Paths::resolve`, any thread; `set_var` is process-global), maps `PKEXEC_UID` back to a home via getpwuid (`nix`), guarded on euid 0 + a present `PKEXEC_UID` so a deliberately-root serve is untouched, and leaving HOME as-is (never `/root`) on any unresolvable uid. The pure `restored_home(euid, pkexec_uid, lookup)` carries the guard/fallbacks and is unit-tested without a real user database. **#51:** `DMCP_ELEVATION_TIMEOUT_SECS` bounded the whole call, not the auth. `call_tool_elevated` wrapped the child's stdout-read-to-EOF in the timeout, and stdout only EOFs when the child exits — so any system-scope job over 180s (`pacman -Syu`) died with a false "authentication prompt was never completed" ~179s after auth actually succeeded, while the double-forked + setsid jarvis-shell holder survived the process-group kill and kept running as root. Now the re-exec'd, now-root child emits an internal sentinel (`ELEVATION_SENTINEL`, `\x01`-bracketed) to stderr once past pkexec — carried on the argv (`--dmcp-internal-elevation-authenticated`, stripped in `main` before clap) since pkexec sanitizes the environment, gated on `DELEGATED_ENV` so a human `dmcp call`/`install` never emits it. `relay_stderr_watching_sentinel` notifies on the marker and strips it from both the teed stream and the retained detail; `wait_for_auth_or_deadline` races the sentinel, completion, and the deadline, so the deadline bounds only the pre-auth window and an authorized long run finishes unbounded. A genuine timeout now says authentication did not complete and warns that a detached holder may have outlived the kill, rather than implying the operation stopped. Exit-code mapping (0/2/else) and the retained-stderr failure detail are unchanged. Verified without pkexec: `restored_home` guard/fallbacks, `scan_sentinel` boundary cases, the sentinel-stripping relay (auth fires, marker absent from sink and retained), `wait_for_auth_or_deadline` under a paused clock (sentinel-before-deadline completes past the old 180s; no-sentinel times out; a fast call is taken directly), the honest timeout message, and an end-to-end `tests/elevation_sentinel.rs` that the flag emits the sentinel and never reaches clap.

*2026-07-31:* the retained stderr is a bounded tail. `call::relay_stderr` now keeps only the most recent 64 KiB (`STDERR_RETAIN_MAX`) of a child's stderr for failure detail — the end is where the failure reason lives, and the previous retain-everything let a flooding server balloon memory for the whole call and turn the error text (which `dmcp serve` delivers verbatim to the LLM as the tool-result error) into a prompt-sized payload. When bytes were dropped, the detail is prefixed with `[stderr truncated to last 64 KiB]` so a reader knows it is a tail, not the whole story; both retained consumers get this — `attach_stderr_detail` (`ConnectionFailed`/`ToolCallFailed`) and the delegated path's `ElevationFailed` detail. The live relay onto dmcp's own stderr stays an uncapped stream on purpose: the caller consumes it incrementally, so it costs nothing to keep whole. Verified by `flood_and_explode` in the fake logging server (~300 KiB flooded mid-call, then death without an answer): the error text carries the flood's last marker behind the truncation notice, never its first, and stays near the cap, while the full flood — first marker included — still reaches dmcp's stderr through the stream.

*2026-07-31:* server stderr is relayed live during a call (#49). `call::relay_stderr` — a spawned task teeing a child's stderr onto dmcp's own while retaining the bytes; raw chunks, never lines, so a newline-less "Proceed? [Y/n] " prompt flows through promptly, and stdout (the JSON-RPC wire) is untouched. Applied twice: the one-shot stdio call pipes the server's stderr through it instead of rmcp's inherited default (the same live view a caller already had, but a failed call's `ConnectionFailed`/`ToolCallFailed` now carries `server stderr: ...` detail it previously lost), and the elevated delegation replaces `wait_with_output` with a concurrent stdout-read + wait beside the relay — the same both-pipes-drained guarantee, so neither pipe filling can wedge the call — with the retained copy still becoming the `ElevationFailed` detail as before. `finish_relay` bounds the post-exit drain, so a stderr fd leaked into a longer-lived grandchild cannot hang a finished call. `dmcp run` already inherits stderr (live by construction) and is untouched; the broker/session path is deferred by the issue. Verified with `tests/fixtures/fake_logging_server.py`, which writes a newline-less marker to stderr mid-`tools/call` and then blocks until a sentinel file appears: the integration test creates the sentinel only after the marker shows up on dmcp's stderr, so an end-buffered relay fails a bounded wait instead of shipping green. The elevated half is covered at unit level (relay liveness before EOF, closed-sink survival, bounded drain, exit-code mapping), since pkexec cannot be driven from the suite.

*2026-07-30:* mid-call elicitation (Project-JARVIS#210, dmcp half). `src/elicit.rs` added: `ServerClient` replaces the `()` client handler at every site, `unattended` (one-shot: declares nothing, declines) vs `attended` (brokered: declares form-mode elicitation and routes prompts to a driver), `PromptRequest`/`PromptAnswer` wire types carrying the asking server's id for provenance, and a `DMCP_ELICIT_MAX_PROMPTS` budget claimed via `fetch_update`. The broker keeps each session's prompt receiver beside its cached client and `drive_call` drains it concurrently with the in-flight call (biased select) — awaiting the call first would deadlock against the question that must be answered to finish it. `BrokerRequest::Answer` + an interim `BrokerResponse.prompt` make a call a multi-message exchange; `send_request_with` / `PromptDriver` let a caller answer, and `dmcp call --session --interactive` exposes that over stdio as a tagged JSON stream. Every failure mode resolves to a decline rather than a hang. Verified with `tests/fixtures/fake_eliciting_server.py`, which issues a real `elicitation/create` mid-`tools/call` and blocks on the reply: 4 integration tests cover decline, a three-round wizard, session survival, and the interactive accept path carrying content verbatim into the server.

*2026-07-30:* the agent surface elevates by delegation (#45). `call::call_tool` gates on `plan_elevation` (Direct / Delegate / Refuse) before spawning: a system-scope stdio server invoked from `dmcp serve` or `dispatch_tasks` is handed to a child `dmcp call`, which re-execs through pkexec itself and lets polkit prompt (`allow_gui` exists for exactly this caller), so the tool runs as root instead of silently at the invoking user's uid. `dmcp serve` cannot re-exec itself — it would replace the daemon — but spawning a child that elevates costs nothing, and the privilege dies with the command. `DMCP_ELEVATION_DELEGATED` stops a denied child from delegating again (a denial would otherwise recurse); `DMCP_ELEVATION_TIMEOUT_SECS` (default 180) bounds an unanswered prompt, with the child in its own process group and killed on drop. Exit 0/2 map to success / tool-reported error; anything else is `ElevationFailed` carrying the child's stderr, which is where a non-active session lands since polkit denies rather than prompts there. Keyed on the same `needs_system_elevation` predicate as the CLI's re-exec, so the two surfaces cannot disagree on what needs root; user-scope and remote transports are untouched, and introspection (`list_tools`) stays unprivileged. Verified end-to-end by driving a real `dmcp serve` as an unprivileged uid: the user-scope call returned its result, the system-scope call delegated and reported the elevation failure.

*2026-07-25:* drift reaches the agent (#39, dmcp half). `serve.rs` gains `update_server` (id-only, mirroring `install_server`'s confinement) plus `serve::update_decision`, the extracted policy: disowned/`removed` → refuse + advise `uninstall_server`, no drift → up to date, drift → `update::refresh_install` with `ignore_platform: false` and an `old -> new` hash in the result; `deprecated` warns and proceeds because the update path uses `trust_gate_for_update` (CLI gate), not `agent_trust_gate`. `get_server_info` carries best-effort `update_available` / `revoked` / `trust_status` from a live registry read, omitted silently on any fetch failure. `update::tests` is `pub(crate)` so the serve tests reuse its `TempTree` + `file://` registry fixtures instead of copying them. The daemon-side half (a Project-JARVIS `update_server` action, the periodic `--check --all` sweep) is not in this repo.

*2026-07-25:* the vector-search surface carries the platform state. `VectorEntry.platforms` / `platforms_malformed` are copied from the registry entry by `sync-index` (both server- and tool-level entries, read through `platform::platform_decl`); `SearchResult` gains `platforms`, `platforms_malformed` and an always-serialized `unsupported_on_host`, computed at search time so a copied index still answers for the host doing the searching. `dmcp browse --vector`/`--vectors` mark in the table and in `--json`. **Migration:** an `index.json` synced before this reads as unrestricted until `dmcp sync-index` runs again. Locally indexed entries (`dmcp index-server`) declare nothing and stay unrestricted. `setup::run_setup` refuses a POSIX script on a Windows host with `SetupError::NoWindowsScript` instead of handing `setup.sh` to `powershell.exe -File`, which could only fail on the extension.

*2026-07-25:* `platform::PlatformDecl` — one three-state reading (absent / malformed / declared) shared by the raw-JSON and typed views; `Manifest.platforms` and `Transport.platforms` deserialize through it and never fail. `connect` takes `ignore_platform` and gates the fetched manifest, so `dmcp install <url>` and `dmcp connect --ignore-platform` behave like the by-id path; `Commands::Install` refuses before the elevation prompt; `connect` selects its setup script with `setup::script_for_host`; `setup::run_setup` falls back from `bash` to `sh` and names the interpreter it failed to spawn.

*2026-07-22:* `doc_comments.rs` added to the tree; elevation described per-OS; env-var path overrides documented; semantic-search commands added to Key Commands; stale line count dropped.

*2026-07-24:* `update.rs` added — hash-drift detection and the `dmcp update` subcommand (single id / `--all`, `--check`, `--json`); reuses the install flow and trust gates. `browse` now surfaces `update_available` for drifted installed servers.

*2026-07-25:* per-transport `platforms` (#42). `Transport::platforms()` plus `transport::select` / `select_for_host` (host injectable) — the single selection path used by `call`, `list_tools`, `run`, the broker, `install`'s transport-type read, and the `list`/`browse` displays; `CallError::NoTransportForHost` / `RunError::NoTransportForHost` carry the refusal. `Manifest.setup_script_windows` + `SetupScriptSpec` in `install.rs` (POSIX/Windows share one download-verify-write gate, `integrity.setupScriptWindowsSha256`); `setup.rs` chooses PowerShell for `.ps1`/Windows and honours a bash shebang instead of always invoking `sh`.

*2026-07-25:* `platform.rs` added — host detection (`macos` → `darwin`) and the registry `platforms` gate (#41). `Manifest.platforms`; `install`/`update` refuse an unvouched host before any clone or setup, with `--ignore-platform` on both; `RegistryServer` (browse) and `DriftReport` (`update --check --json`) carry `platforms` + `unsupported_on_host`. `install::install` and `update::refresh_install` take an `ignore_platform` argument; the `dmcp serve` agent path passes `false`.

*2026-07-24:* `broker.rs` added — session-scoped persistent-server broker (#36). `Manifest.stateful` flag; `dmcp call --session <sid>` thin client (gated on stateful + user scope) over a UDS/NDJSON protocol; `dmcp broker` (hidden, auto-started) and `dmcp session list|close|gc`. Spawn/env/install-dir resolution is factored into `call::build_stdio_command` / `call::resolve_stdio_install_dir`, shared by the one-shot path, `run`, and the broker. Integration tests use a stdlib-only fake stateful MCP server (`tests/fixtures/fake_stateful_server.py`).
