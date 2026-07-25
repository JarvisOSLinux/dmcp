# MCP System Integration Spec

This document specifies the layout, formats, and expected behavior of a system-level MCP (Model Context Protocol) server manager.

**Status:** Implemented by dmcp (this repo, `src/`). This document specifies the on-disk layout and formats that dmcp implements.

---

## 1. Overview

MCP servers can be installed in two scopes:

- **User scope:** Per-user, no root required
- **System scope:** System-wide, visible to all users, requires root for install/remove

A system package should provide:

1. **Discovery** — List all installed MCP servers (user + system)
2. **Invocation** — Spawn stdio servers with correct command, args, working dir, and config
3. **Config resolution** — Read and merge config from manifests
4. **Path abstraction** — Single API for user vs system scope and precedence

System-scope operations (install, remove, and spawning system-scoped stdio
servers — including `dmcp call`) require elevation; on Linux dmcp re-executes
itself via pkexec under the polkit action `org.jarvisos.dmcp.run-system-server`
(see `policy/org.jarvisos.dmcp.policy`), on macOS via sudo/osascript.

**Beyond this spec**, the implementation also provides tool invocation
(`dmcp tools` / `dmcp call`), MCP-server mode (`dmcp serve`), remote connect
(`dmcp connect`), and semantic vector search (`dmcp sync-index`,
`dmcp browse --vector`) — see `MCP-REGISTRY-GUIDE.md` and `docs/LLM-INTEGRATION.md`.

---

## 2. Paths (XDG Conventions)

All paths follow XDG Base Directory Specification where applicable.

### 2.1 Registry Sources (Config)

| Scope | Path | Notes |
|-------|------|-------|
| User | `$XDG_CONFIG_HOME/mcp/sources.list` | Default: `~/.config/mcp/sources.list` |
| System | `/etc/mcp/sources.list` | Admin-managed |

**Priority:** User sources are listed first, then system. Entries are **not** deduplicated across scopes; the same registry URL in both scopes is fetched from both, and server entries may appear multiple times in browse output.

### 2.2 Installed Servers (Data)

| Scope | Base path | Index | Manifest |
|-------|-----------|-------|----------|
| User | `$XDG_DATA_HOME/mcp/installed/` | `index.json` | `<id>/manifest.json` |
| System | `/usr/share/mcp/installed/` | `index.json` | `<id>/manifest.json` |

Default for `$XDG_DATA_HOME`: `~/.local/share`.

**Load order:** User index first, then system. If the same `id` appears in both, implementation may choose user over system (user override).

### 2.3 Vector Index (Semantic Search)

| Path | Purpose |
|------|---------|
| `$XDG_DATA_HOME/mcp/vector_index/index.json` | Cached registry embeddings for `dmcp browse --vector` (populated by `dmcp sync-index`) |

Registries themselves are fetched live on each browse/install — there is no
registry JSON cache.

**Env overrides:** all paths are overridable via `MCP_USER_SOURCES_PATH`,
`MCP_USER_INSTALL_DIR`, `MCP_SYSTEM_SOURCES_PATH`, `MCP_SYSTEM_INSTALL_DIR`,
and `MCP_VECTOR_INDEX_DIR` (loaded from `.env` via dotenvy).

---

## 3. sources.list Format

Plain text file. One URL per line. Lines starting with `#` and empty lines are ignored.

```
# MCP Registry Sources
# Each line is a URL to a registry JSON file

https://raw.githubusercontent.com/example/mcp-registry/main/registry.json
https://example.com/other-registry.json
```

---

## 4. Registry Format

A registry is a JSON file fetched from a URL. Structure:

```json
{
  "version": "1.0",
  "updated": "2025-02-03T00:00:00Z",
  "servers": [
    { "id": "...", "name": "...", ... }
  ]
}
```

| Field | Type | Description |
|-------|------|--------------|
| `version` | string | Format version (use `"1.0"`; not validated) |
| `updated` | string | ISO 8601 timestamp (not validated) |
| `servers` | array or object | Array of server entries, **or** an id-keyed object map (the JARVIS mcp-registry uses the map form) |

### 4.1 Server Entry (Registry)

Each server object in `servers`:

**Required:** `id`, `name`, `summary`, `version`, and either inline `transports` + `source` (for stdio) **or** a `manifest` URL pointing to the server's `manifest.json` (the form the JARVIS mcp-registry uses — transports/source are then fetched from the manifest).

**Optional:** `description`, `author`, `homepage`, `bugUrl`, `donationUrl`, `icon`, `categories`, `capabilities`, `permissions`, `tools`, `configurableProperties`, `license`, `releaseDate`, `size`, `screenshots`, `changelog`, `scope`, `keywords`, `trustStatus` (`community`/`official`; `deprecated`/`removed` for revocation), `embeddings` (semantic-search vectors), `integrity` (`manifestSha256`, `setupScriptSha256`, `setupScriptWindowsSha256`), `platforms` (vetted platforms), `setupScriptWindows` (Windows setup script)

**Integrity:** at install, dmcp verifies the fetched manifest's raw bytes
against `integrity.manifestSha256` (hard failure on mismatch) and setup scripts
before running them — `setup.sh` against `setupScriptSha256`, `setup.ps1`
against `setupScriptWindowsSha256`. Whichever script the host runs passes the
same gate.

**Platforms:** `platforms` lists the platforms the registry vouches for —
`"linux"`, `"darwin"`, `"windows"`. dmcp maps the host from
`std::env::consts::OS` (`macos` → `darwin`) and refuses to install or update an
entry that excludes the host, before any clone or setup script, unless
`--ignore-platform` is passed. Absent means unrestricted.

Individual entries in `transports` may carry the same field, selecting the
launch line for the host (see §9). The top-level list governs installability;
a transport's list governs which command starts the server.

**Icon:** Freedesktop icon name (e.g. `"utilities-terminal"`) or URL to image (e.g. `https://example.com/logo.png`).

---

## 5. Index Format

`index.json` lives at `<base>/mcp/installed/index.json`. It stores pointers only; full metadata is in each manifest.

```json
{
  "servers": {
    "com.example.my-server": {
      "location": "/home/user/.local/share/mcp/installed/com.example.my-server/manifest.json"
    }
  }
}
```

| Field | Type | Description |
|-------|------|--------------|
| `servers` | object | Map of server `id` → `{ "location": "<absolute path to manifest.json>", "keywords": [...] }` — `keywords` optional (used for search); `manifest` is accepted as a read alias for `location` |

---

## 6. Manifest Format

Each installed server has `manifest.json` in its install directory. The manifest is the full server metadata plus runtime config. At spawn time, dmcp injects the manifest's `config` map into the server process as environment variables (key names used verbatim).

Structure matches the registry server entry, plus:

- `config` — Object of `key` → `value` for configurable properties (user-provided values)
- `installDir` — Absolute path to the install directory (for servers to resolve paths)

### 6.1 Transport Types

**stdio (local process):**

```json
{
  "type": "stdio",
  "command": "python3",
  "args": ["server.py"],
  "description": "Main interface"
}
```

- `command`: Executable (e.g. `python3`, `node`)
- `args`: Arguments, relative to project root (install dir)
- Process is spawned with `cwd` = install dir

**sse (remote):**

```json
{
  "type": "sse",
  "url": "https://api.example.com/mcp/sse",
  "description": "Cloud endpoint"
}
```

**websocket (remote):**

```json
{
  "type": "websocket",
  "wsUrl": "wss://api.example.com/mcp/ws"
}
```

### 6.2 Source (for stdio)

```json
{
  "source": {
    "type": "git",
    "url": "https://github.com/example/repo.git",
    "path": "servers/my-server"
  }
}
```

- `path`: Project root within repo (optional; empty = repo root)
- Install dir contains the cloned/extracted project; `command` + `args` run from there

### 6.3 Configurable Properties

```json
{
  "configurableProperties": [
    {
      "key": "api_key",
      "label": "API Key",
      "description": "Your API key",
      "sensitive": true,
      "required": true
    },
    {
      "key": "timeout",
      "label": "Timeout (seconds)",
      "default": "30",
      "sensitive": false,
      "required": false
    }
  ],
  "config": {
    "api_key": "user-provided-value",
    "timeout": "60"
  }
}
```

- `config` holds user-provided values (defaults are not auto-applied)
- dmcp injects each `config` key/value into the server's environment at spawn

### 6.4 Setup Script

Manifests may carry `setupScript` (a filename inside the server folder for
local servers, or a URL for remote servers). dmcp records the related fields
`setupScriptPath`, `setupScriptRunAt`, `setupScriptVersion`, and
`setupScriptStatus` in the installed manifest, and `setupScriptWindows` names
the Windows counterpart (e.g. `setup.ps1`). Scripts run in the install directory
and receive `MCP_INSTALL_DIR` plus `MCP_CONFIG_<KEY>` (uppercased, `-`/`.` → `_`)
for each config key.

The interpreter is `sh`, except that a shebang naming bash (`#!/usr/bin/env
bash`, `#!/bin/bash`) is honoured — `/bin/sh` is dash on several distributions,
where `pipefail`, arrays and `[[ ]]` are hard errors. A Windows host runs
`setupScriptWindows` through PowerShell (`-NoProfile -ExecutionPolicy Bypass
-File`), falling back to `setupScript`; other hosts only consider `setupScript`.
Registry-referenced setup scripts are verified against
`integrity.setupScriptSha256` / `integrity.setupScriptWindowsSha256` before
running.

### 6.5 Scope

- `"scope": "user"` (default) → `$XDG_DATA_HOME/mcp/installed/<id>/`
- `"scope": "system"` → `/usr/share/mcp/installed/<id>/`

---

## 7. Directory Layout After Install

**User scope:**

```
~/.local/share/mcp/installed/
├── index.json
├── com.example.calculator/          (stdio — Git clone)
│   ├── manifest.json
│   ├── server.py
│   └── ...                          (project files)
└── com.example.remote-api/          (SSE — metadata only)
    └── manifest.json
```

**System scope:** Same structure under `/usr/share/mcp/installed/`.

---

## 8. API Requirements (for Implementation)

A reference implementation should provide at least:

### 8.1 Discovery

- `list_servers()` — Return all installed servers (user + system, with precedence)
- `get_server(id)` — Return metadata for a server by id, or null if not found
- `get_manifest_path(id)` — Return path to manifest.json for a server

### 8.2 Invocation (stdio only)

- `spawn_server(id)` — Start the stdio process for a server
  - Working directory: install dir
  - Environment: inherit from parent, plus each key/value in the manifest `config` object injected as an environment variable (key name used verbatim, e.g. `BRAVE_API_KEY`)
  - Command + args from primary transport

### 8.3 Config

- `get_config(id)` — Return merged config (defaults + user values)
- `set_config_value(id, key, value)` — Update config and persist to manifest (requires write access)

### 8.4 Path Helpers

- `user_install_dir()` — `$XDG_DATA_HOME/mcp/installed/`
- `system_install_dir()` — `/usr/share/mcp/installed/`
- `user_sources_path()` — `$XDG_CONFIG_HOME/mcp/sources.list`
- `system_sources_path()` — `/etc/mcp/sources.list`

---

## 9. Invocation Behavior (stdio)

When spawning a stdio server:

1. Resolve manifest path from index
2. Load manifest.json
3. Get the primary transport: the first entry in `transports` whose `platforms` include the host (an entry without `platforms` matches every host) with `type == "stdio"`. If no entry covers the host, the invocation fails naming the platforms the manifest declares
4. `cwd` = `installDir` from manifest (or dir containing manifest.json)
5. Execute `command` with `args`
6. Config is delivered via environment variables (the manifest `config` map, keys verbatim)

Environment: inherit from parent, plus the injected config variables.

---

## 10. Backward Compatibility (Planned)

For robustness, an implementation may support:

1. **Missing index:** Scan `<base>/mcp/installed/` for subdirs with `manifest.json`; build index in memory
2. **Legacy index:** If `servers` is an array of full objects instead of id→location map, parse and optionally migrate
3. **Unknown servers:** Log and optionally surface; do not silently drop

---

## 11. References

- **MCP Registry Guide:** `MCP-REGISTRY-GUIDE.md` — registry format, transports, scope
- **XDG Base Directory:** https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html
- **Model Context Protocol:** https://modelcontextprotocol.io/

---

## Changelog — corrected claims

*2026-07-25:* per-transport `platforms` documented (#42) — host-selected launch line at every spawn site; `setupScriptWindows` / `integrity.setupScriptWindowsSha256`; the setup-script interpreter corrected (`sh`, bash when the shebang asks, PowerShell for the Windows script).

*2026-07-25:* registry `platforms` documented (#41) — vetted-platform list, host mapping, and the pre-clone install/update refusal with `--ignore-platform`.

*2026-07-22:* status corrected — this spec is implemented by dmcp (this repo); config delivery documented as env-var injection (servers do not read manifest.json); sources are not deduplicated across scopes; registry `servers` accepts array or id-keyed map, with manifest-referenced entries, `trustStatus`, and SHA-256 `integrity` verification documented; elevation (pkexec/polkit on Linux, sudo/osascript on macOS) and the vector index documented; env-var path overrides listed; index entries carry `keywords` with `manifest` as a read alias for `location`; setup-script fields documented; Discover-specific cache and `libdiscover`/`backward_compatibility.md` references removed; tool invocation / serve / connect / semantic search noted as beyond-spec capabilities.
