# MCP Registry Guide

How to create and host an MCP server registry for dmcp (the JARVIS MCP server manager).

## Overview

dmcp fetches server listings from **registries** -- JSON files hosted at a URL. Users add your registry URL with `dmcp sources add <url>` (stored in `~/.config/mcp/sources.list`), and `dmcp browse`/`dmcp install` pull your server catalogue live on each call.

The flow looks like this:

```
Your GitHub repo                     User's machine
  registry.json         -->        dmcp
  (hosted via raw URL)              fetches & caches
                                    shows servers in UI
                                    user clicks Install
                                    manifest.json written to installed/{id}/
                                    index.json updated (id -> manifest location)
```

Registry sources are read from (in priority order):
- `~/.config/mcp/sources.list` (user)
- `/etc/mcp/sources.list` (system)

### Automatic vs Manual Setup

dmcp creates most files automatically; only the system sources list needs manual setup:

| File or directory | Created by | Notes |
|-------------------|------------|-------|
| `~/.config/mcp/sources.list` | dmcp | Created by `dmcp sources add <url>` (parent dirs auto-created). |
| `~/.local/share/mcp/installed/index.json` | dmcp | Created when the first server is installed. Updated on each install/remove. |
| `~/.local/share/mcp/installed/<id>/manifest.json` | dmcp | Written per server on install; updated by `dmcp config set`. |

| `/etc/mcp/sources.list` | Admin/distro | **Manual setup.** System-wide registry sources. Create this file if you want all users on the machine to see the same registries by default. |

## Registry File Format

A registry is a single JSON file with this structure:

```json
{
  "version": "1.0",
  "updated": "2025-02-03T00:00:00Z",
  "servers": [
    { ... },
    { ... }
  ]
}
```

| Field       | Type   | Description                              |
|-------------|--------|------------------------------------------|
| `version`   | string | Registry format version (use `"1.0"`)    |
| `updated`   | string | ISO 8601 timestamp of last update        |
| `servers`   | array  | Array of server entry objects (see below) |

## Server Entry Schema

Each entry in `servers` describes one MCP server. `servers` may be an **array**
of entries or an **id-keyed object map** (the JarvisOSLinux production registry
uses the map form).

### Manifest-referenced entries (recommended)

Instead of inlining `transports`/`source`, an entry may carry a `manifest` URL
pointing to the server's `manifest.json`, plus an `integrity` object:

```json
{
  "id": "com.yourorg.mcp.servername",
  "name": "My MCP Server",
  "summary": "One-line description",
  "version": "1.0.0",
  "keywords": ["..."],
  "trustStatus": "community",
  "manifest": "https://.../servers/<id>/manifest.json",
  "integrity": {
    "manifestSha256": "...",
    "setupScriptSha256": "...",
    "setupScriptWindowsSha256": "..."
  }
}
```

At install, dmcp fetches the manifest and **verifies its raw bytes against
`integrity.manifestSha256`** (hard failure on mismatch); setup scripts are
verified before running — `setup.sh` against `setupScriptSha256`, `setup.ps1`
against `setupScriptWindowsSha256`. Whichever script this host runs passes the
same gate: a per-platform script is not a way around verification. Hashes are
recomputed by registry CI (`scripts/sync_registry.py`), never hand-edited.

### Trust status

`trustStatus` is `community` (default) or `official`; `deprecated`/`removed`
mark revocation. At install dmcp warns on `community` ("you are trusting the
submitter") and `deprecated`, and refuses `removed`. The autonomous agent path
(`dmcp serve` → `install_server`) is stricter: `deprecated`/`removed` are always
refused, and `community` installs are flagged as not maintainer-reviewed. See
`mcp-registry/docs/TRUST-MODEL.md`.

### Required Fields

```json
{
  "id": "com.yourorg.mcp.servername",
  "name": "My MCP Server",
  "summary": "One-line description shown in the listing",
  "version": "1.0.0",
  "transports": [ ... ],
  "source": { ... }
}
```

| Field       | Type   | Description                                              |
|-------------|--------|----------------------------------------------------------|
| `id`        | string | Unique identifier. Use reverse-domain notation.          |
| `name`      | string | Display name in the catalogue.                           |
| `summary`   | string | Short description shown in listings.                     |
| `version`   | string | Semantic version of the server.                          |
| `transports`| array  | Array of entrypoints (stdio, SSE, or WebSocket).         |
| `source`    | object | Git source for local servers; omit or use empty for remote. |

### Optional Fields

| Field                | Type   | Description                                                     |
|----------------------|--------|-----------------------------------------------------------------|
| `description`        | string | Long description. Supports `\n` for line breaks.                |
| `author`             | string | Author name.                                                    |
| `homepage`           | string | URL to the project homepage.                                    |
| `bugUrl`             | string | URL to the issue tracker.                                       |
| `donationUrl`        | string | URL for donations.                                              |
| `icon`               | string | Icon for display: Freedesktop icon name or URL to an image (see Icons below). |
| `keywords`           | array  | Search keywords (e.g. `["calculator", "math"]`) for easier discovery. |
| `capabilities`       | array  | What the server can do (freeform strings for display).          |
| `permissions`        | array  | Permissions the server requires (freeform strings for display). |
| `tools`              | array  | Tools provided (strings or `{"name": ..., "description": ...}`).|
| `configurableProperties` | array | Configuration properties (required and optional, see below). |
| `license`            | object | `{"name": "MIT", "url": "https://..."}`.                       |
| `releaseDate`        | string | ISO 8601 date string (`"2025-01-15"`).                          |
| `size`               | number | Approximate size in bytes (for display).                        |
| `screenshots`        | array  | Screenshot URLs or `{"thumbnail": ..., "url": ...}` objects.   |
| `changelog`          | string | Changelog text.                                                 |
| `scope`              | string | `"user"` (default) or `"system"`. See Scope below.              |
| `platforms`          | array  | Platforms the registry vouches for: `"linux"`, `"darwin"`, `"windows"`. Omit for unrestricted. See Platforms below. |
| `setupScript`        | string | For local servers: filename of a shell script inside the server folder (e.g. `"setup.sh"`). For remote servers: URL to download the script. See Setup Script below. |
| `setupScriptWindows` | string | Windows counterpart of `setupScript` (e.g. `"setup.ps1"`), run through PowerShell. Omit when the server has no Windows setup. |

### Setup Script

The setup script runs in the server’s **install directory** (where `manifest.json` lives), so the script can read the user’s config from the manifest and set up the environment.

**Local servers (stdio):** The script **must be inside the folder that the MCP project is in** (i.e. inside `source.path`). When dmcp clones the repo, it copies the server folder contents to the install directory—the setup script is included. Specify the filename in `setupScript` (e.g. `"setup.sh"`). Use it to install dependencies (e.g. `pip install -r requirements.txt`, `npm install`, `cargo build --release`). The script runs with `sh` in the install directory; dmcp exports `MCP_INSTALL_DIR` plus `MCP_CONFIG_<KEY>` (uppercased, `-`/`.` replaced by `_`) for each config key.
**Remote servers (SSE/WebSocket):** There is no clone; the install directory only contains `manifest.json`. Provide a URL in `setupScript`; dmcp downloads the script (verifying `integrity.setupScriptSha256` when the registry provides it). The script runs locally with `sh` and receives the same `MCP_INSTALL_DIR`/`MCP_CONFIG_<KEY>` environment. 
- **Install flow**: Clone/download first, then run setup automatically. If the setup script fails, the install still succeeds; the main action button becomes "Run Setup" so the user can retry.
- **Re-run**: If setup failed or was skipped, the main action button shows "Run Setup" instead of "Copy ID".
- **Execution**: The script runs in the install directory. For system scope, it runs with elevated privileges.
- **Interpreter**: `sh`, unless the script's shebang names bash (`#!/usr/bin/env bash`, `#!/bin/bash`) — then bash. Write the shebang if the script uses `set -o pipefail`, arrays or `[[ ]]`: `/bin/sh` is dash on Debian and Ubuntu and those are hard errors there.
- **Windows**: on a Windows host dmcp runs `setupScriptWindows` (e.g. `setup.ps1`) through PowerShell (`-NoProfile -ExecutionPolicy Bypass -File`), falling back to `setupScript` when no Windows script is declared. On every other host `setupScript` is the only one considered.
- **Storage**: The manifest stores `setupScript` / `setupScriptWindows` (filename or URL), `setupScriptPath` (local path), `setupScriptVersion`, and `setupScriptRunAt` (last run timestamp).

Example (local server):

```json
{
  "id": "com.example.mcp.my-server",
  "setupScript": "setup.sh",
  "source": { "type": "git", "url": "...", "path": "servers/my-server" },
  "transports": [{ "type": "stdio", "command": "python3", "args": ["server.py"] }]
}
```

Your repo layout: `servers/my-server/setup.sh` must exist alongside `server.py`, `pyproject.toml`, etc.

### Icons

Registry owners define each server's icon in the `icon` field. Two formats are supported:

1. **Freedesktop icon name** – Use a standard icon from the user's icon theme (e.g. Breeze, Adwaita):
   - `"network-server"` – good for remote SSE/WebSocket servers
   - `"utilities-terminal"` – for CLI/dev tools
   - `"accessories-calculator"` – for calculator-style tools
   - `"applications-development"` – generic development

2. **URL to an image** – Use a custom logo hosted anywhere:
   - GitHub raw URL: `"https://raw.githubusercontent.com/yourorg/mcp-registry/main/logos/my-server.png"`
   - Any public image URL (PNG, SVG, etc.)

If omitted, dmcp falls back to `"application-x-executable"`. Prefer Freedesktop names when a suitable one exists; use URLs for custom branding.

## Transports (Entrypoints)

The `transports` array lists one or more entrypoints. Each entrypoint can be stdio (local process), SSE, or WebSocket.

### stdio (Local Process)

Runs as a local process. The `command` and `args` are executed from the project root (install dir).

```json
{
  "type": "stdio",
  "command": "python3",
  "args": ["server.py"],
  "description": "Main calculator interface"
}
```

| Field         | Type   | Description                                    |
|---------------|--------|------------------------------------------------|
| `command`     | string | Executable (e.g. `python3`, `node`).          |
| `args`        | array  | Arguments, relative to project root.          |
| `description` | string | Optional description of this entrypoint.       |
| `platforms`   | array  | Platforms this entrypoint is for (`"linux"`, `"darwin"`, `"windows"`). Omit to match every host. See Per-transport platforms below. |

### sse (Server-Sent Events)

Remote endpoint. No local installation.

```json
{
  "type": "sse",
  "url": "https://api.example.com/mcp/sse",
  "description": "Cloud API endpoint"
}
```

### websocket

```json
{
  "type": "websocket",
  "wsUrl": "wss://api.example.com/mcp/ws"
}
```

### Per-transport platforms

One capability is one server, but the line that starts it is not the same
everywhere: `python3` vs `python`, `.venv/bin/x` vs `.venv\Scripts\x.exe`. Each
transport may therefore declare its own `platforms`, using the same three-value
vocabulary as the top-level field:

```json
"transports": [
  { "type": "stdio", "command": "python3", "args": ["server.py"], "platforms": ["linux", "darwin"] },
  { "type": "stdio", "command": "python",  "args": ["server.py"], "platforms": ["windows"] }
]
```

dmcp launches the **first transport whose `platforms` include the host**; a
transport that declares nothing matches every host, so single-entry manifests
are unaffected. Order matters: an entry without `platforms` is a catch-all, so
platform-specific entries belong before it.

A transport's `platforms` reads by the same rules as the top-level field:
absent or empty matches every host, and a value that is not an array of platform
names matches none — a transport dmcp cannot read is skipped, never used as a
catch-all. The rule is one implementation, so the raw-JSON readers (`install`,
`browse`) and the parsed manifest the spawn sites use can never pick different
transports for the same file.

If no transport covers the host, `dmcp call`, `dmcp tools`, `dmcp run` and
session calls all fail with a message naming the platforms the server *does*
declare, rather than spawning a command written for another OS. The listing
surfaces (`dmcp browse`, `dmcp list`) still report the transport such a server
would launch elsewhere instead of blanking it out — they describe the entry,
they do not start it. This is per-transport dispatch, not a second trust
decision — the top-level `platforms` list is what decides whether the server may
be installed here at all.

### Legacy Format (unsupported)

Older registries using a single top-level `type` + `transport` object are **not**
accepted — no code path reads them. Convert to the `transports` array.

## Scope

The `scope` field controls where the server is installed:

| Scope    | Base path                         | Privileges        |
|----------|-----------------------------------|--------------------|
| `user`   | `~/.local/share/mcp/installed/`   | None (user-local) |
| `system` | `/usr/share/mcp/installed/`       | pkexec (root)     |

Default is `"user"`. System-scope installs are visible to all users on the machine and require password authentication via polkit.

SSE/WebSocket servers also support scope. A system-scope SSE entry puts its manifest in `/usr/share/mcp/installed/<id>/manifest.json` so all users see the configured endpoint.

```json
{
  "id": "com.example.shared-tool",
  "scope": "system",
  ...
}
```

## Platforms

`platforms` declares which platforms the registry has vetted the server on:

```json
{
  "id": "com.example.mcp.thing",
  "platforms": ["linux", "darwin"],
  ...
}
```

The vocabulary is exactly `"linux"`, `"darwin"`, `"windows"`. dmcp derives the host
from `std::env::consts::OS`, mapping `macos` → `darwin`; any other host name matches
nothing and counts as unsupported.

- **Absent means unrestricted** — dmcp installs the entry on any host, so existing
  manifests and third-party registries are unaffected. An empty list (`[]`, or one
  that is all blanks) reads the same way: a list vouching for nothing is a
  serialization slip, not a server installable nowhere.
- **A malformed value is refused everywhere** — `"windows"` instead of
  `["windows"]`, or an array with a non-string in it, vouches for nothing dmcp can
  read, so it is treated as excluding every host (`--ignore-platform` still
  overrides, and `browse --json` adds `platforms_malformed: true`). Only absence
  means "no restriction declared"; a gate that cannot parse its input must not
  switch itself off.
- **A declared list that excludes the host is refused**: `dmcp install` (by id
  *and* by manifest URL), `dmcp connect` and `dmcp update` exit non-zero *before*
  creating a directory, cloning anything or running a setup script,
  naming the vouched-for platforms. `--ignore-platform` overrides the refusal — use
  it to verify the server on a new OS, then PR that platform into the entry.
- `dmcp browse` marks excluded entries in the table and sets `unsupported_on_host`
  in `--json`, and `dmcp update --check --json` rows carry the same state.
- Individual transports may narrow further (see Per-transport platforms): the
  top-level list says where the server may be installed, a transport's list says
  which launch line runs there.

It is coverage, not aspiration: list a platform once the server has been vetted
there, so the field stays a fact a user can trust. One capability is one server —
growing `platforms` is how coverage expands, not per-OS sibling entries. dmcp reads
the list from the registry entry, so a registry that keeps entries in sync with
their manifests (as `sync_registry.py` does) never forces a manifest fetch just to
filter by host.

## Source Configuration

For **local servers** (stdio), the `source` object specifies a Git repository to clone:

```json
"source": {
  "type": "git",
  "url": "https://github.com/yourorg/mcp-registry.git",
  "path": "servers/calculator-py"
}
```

| Field  | Type   | Description                                                      |
|--------|--------|------------------------------------------------------------------|
| `url`  | string | Git repository URL.                                              |
| `path` | string | Project root within the repo (optional). Empty = repo root.      |
| `rev`  | string | Optional git revision to pin (tag, branch, or full commit SHA). A full 40-char SHA is a binding pin — dmcp verifies the checked-out HEAD matches exactly and aborts the install on mismatch. |

dmcp clones the repo, extracts the project root (`path` or repo root), and runs the transport's `command` + `args` from that directory. The registry author specifies the exact launcher (e.g. `python3 server.py`, `node index.js`) — any language works.

For **remote servers** (SSE/WebSocket), omit `source` or use an empty object. No clone and no endpoint probe happen — the manifest with the connection details is written as-is, so verify your endpoint URL yourself before publishing.

## Configuration Properties

Servers can declare configurable properties in a single `configurableProperties` array. Each property has a `required` flag to indicate whether it must be filled before installation.

dmcp stores `configurableProperties` as metadata for wrapper UIs (e.g. the JARVIS daemon, which shows a config modal). dmcp itself does not prompt, validate required fields, or auto-apply `default` values — set values with `dmcp config <id> set <key> <value>`.

```json
"configurableProperties": [
  {
    "key": "api_key",
    "label": "API Key",
    "description": "Your API key from https://example.com/settings",
    "sensitive": true,
    "required": true
  },
  {
    "key": "timeout",
    "label": "Timeout (seconds)",
    "description": "Request timeout in seconds",
    "default": "30",
    "sensitive": false,
    "required": false
  },
  {
    "key": "endpoint",
    "label": "Endpoint URL",
    "description": "API endpoint (defaults to production)",
    "default": "https://api.example.com/v1",
    "sensitive": false,
    "required": false
  }
]
```

### Property Fields

| Field         | Type    | Description                                                |
|---------------|---------|------------------------------------------------------------|
| `key`         | string  | Internal identifier. Used as the key in config storage.    |
| `label`       | string  | Label shown in the configuration dialog.                   |
| `description` | string  | Help text shown below the input field.                     |
| `default`     | string  | Default value. Pre-filled in the UI (mainly for optional). |
| `sensitive`   | boolean | If `true`, field is shown as a password input.             |
| `required`    | boolean | If `true`, must be filled before installation.             |

User-provided values are stored in the per-server manifest at `<installDir>/manifest.json` in the `config` object and injected into the server process as **environment variables**, using each key verbatim as the variable name — name keys after the env var the server expects (e.g. `BRAVE_API_KEY`). dmcp does not auto-apply `default` values.

Use `keywords` for searchability in `dmcp browse -k` and the semantic index.


## Hosting Your Registry

### Option 1: GitHub Raw URL (Simplest)

1. Create a `registry.json` in your repo.
2. Use the raw GitHub URL as your registry source:

```
https://raw.githubusercontent.com/yourorg/mcp-registry/main/registry.json
```

Users add this URL to their sources:

```bash
echo "https://raw.githubusercontent.com/yourorg/mcp-registry/main/registry.json" \
  >> ~/.config/mcp/sources.list
```

### Option 2: GitHub Pages

If you want a cleaner URL, serve `registry.json` via GitHub Pages:

```
https://yourorg.github.io/mcp-registry/registry.json
```

### Option 3: Your Own Server

Host `registry.json` on any web server. dmcp sends a standard HTTP GET with the User-Agent `dmcp/1.0`. Ensure HTTPS is used and redirects are followed.

## Minimal Working Example

Here is a complete minimal registry with one local server (Git) and one remote SSE server:

```json
{
  "version": "1.0",
  "updated": "2025-02-09T00:00:00Z",
  "servers": [
    {
      "id": "com.yourorg.mcp.calculator",
      "name": "Calculator MCP",
      "summary": "A simple calculator MCP server",
      "version": "1.0.0",
      "transports": [
        {
          "type": "stdio",
          "command": "python3",
          "args": ["server.py"],
          "description": "Main interface"
        }
      ],
      "source": {
        "type": "git",
        "url": "https://github.com/yourorg/mcp-registry.git",
        "path": "servers/calculator-py"
      },
      "keywords": ["calculator", "math"]
    },
    {
      "id": "com.yourorg.mcp.cloud-api",
      "name": "Cloud API",
      "summary": "Remote SSE server for cloud API access",
      "version": "1.0.0",
      "transports": [
        {
          "type": "sse",
          "url": "https://api.yourorg.com/mcp/sse"
        }
      ],
      "keywords": ["cloud", "api", "sse"],
      "configurableProperties": [
        {
          "key": "api_key",
          "label": "API Key",
          "description": "Get your key at https://yourorg.com/settings",
          "sensitive": true,
          "required": true
        }
      ]
    }
  ]
}
```

## How dmcp Processes Your Registry

1. **Fetch**: On each `dmcp browse`/`dmcp install`, dmcp fetches each URL from `sources.list` (both scopes, no deduplication).
2. There is no local cache — every browse/install fetches the sources fresh.
3. **Parse**: Each server entry in the `servers` array becomes a resource in the MCP Servers catalogue.
4. **Merge**: dmcp marks a registry entry as installed when its `id` matches an installed server (installed entries sort first). There is no automatic upgrade detection yet; rerun `dmcp install <id>` to update in place.
5. **Display**: `dmcp browse` lists entries, searchable by name, summary, id, and keywords (or by semantic vector search).

## What Happens on Install

When a user clicks Install on your server:

1. If the entry declares `platforms` and this host is not among them, the install is refused here — before any directory, clone, or setup script (see Platforms).
2. `configurableProperties` are stored as metadata; wrapper UIs (e.g. JARVIS) may prompt for them — dmcp itself does not.
3. If `scope` is `"system"`, the user authenticates via polkit (password prompt for pkexec).
4. A dedicated directory is created at `<base>/mcp/installed/<id>/`.
5. For **local servers** (stdio): `git clone` fetches the repo, then the project root (`source.path` or repo root) is extracted into the install dir. The transport's `command` + `args` run from that directory. Which transport that is follows the host when the manifest declares per-transport `platforms`.
6. For **remote servers** (SSE/WebSocket): the manifest with the connection details is written as-is — no local clone and no endpoint probe.
7. A manifest is written to `<installDir>/manifest.json` with full metadata and config; the `config` map is injected as environment variables when the server is spawned.
8. The index at `<base>/mcp/installed/index.json` is updated: top-level `{"servers": {...}, "version": "1.0", "updated": <RFC3339>}` with per-entry `{"location": "<path>/manifest.json", "keywords": ["..."]}` (`manifest` is accepted as a read alias for `location`). The index stores pointers plus keywords for search; full metadata lives in each manifest.
9. For user-scope, `<base>` is `~/.local/share`. For system-scope, `<base>` is `/usr/share`.

### Directory Layout After Install

**User-scope** (`~/.local/share/mcp/installed/`):

```
~/.local/share/mcp/installed/
├── index.json                                 (id -> manifest + keywords)
├── com.example.calculator/                     (local server — Git clone)
│   ├── manifest.json                           (full metadata + config; MCP servers read this)
│   ├── server.py                               (project root contents)
│   └── ...                                     (other project files)
└── com.example.remote-api/                     (SSE server)
    └── manifest.json                           (full metadata + config)
```

**System-scope** (`/usr/share/mcp/installed/`) has the same structure but is owned by root and managed via pkexec.

### Uninstall

Removal is a simple `rm -rf <installDir>`. All files are self-contained. For system-scope, `pkexec rm -rf` is used.

### Troubleshooting: No Servers or Install Fails

- **Wrong branch in registry URL**: Many GitHub repos use `main` as the default branch. If your registry URL uses `master` and your repo uses `main`, the fetch will 404. Update `~/.config/mcp/sources.list` to use the correct branch, e.g. `https://raw.githubusercontent.com/yourorg/mcp-registry/main/registry.json`.
- **Registry JSON format**: Ensure your `registry.json` has a `servers` array (or id-keyed object — what the JarvisOSLinux registry uses). `version` and `updated` are recommended metadata but not required by dmcp. Each server needs `id`, `name`, `summary`, `version`, `transports`, and (for local servers) `source` with `type`, `url`, and `path`.

## Tips

- **Keep IDs stable.** The `id` field is how dmcp tracks a server across registry updates. Changing it creates a "new" server.
- **Use semantic versioning.** Versions are informational today — dmcp does not compare versions to detect upgrades.
- **Test your JSON.** A malformed registry file is silently skipped. Validate your JSON before publishing.
- **Update the `updated` timestamp** when you publish changes, so users know the registry is maintained.
- **Provide a `bugUrl`.** Wrapper UIs can surface it as a "Report Bug" link.

## Changelog — corrected claims

*2026-07-25:* the `platforms` reading rules documented for both the entry and the transport — absent/empty is unrestricted, malformed is refused everywhere (`platforms_malformed` in `browse --json`), the refusal now covers `dmcp install <url>` and `dmcp connect`, and the listing surfaces still name the transport a foreign-only server would launch.

*2026-07-25:* per-transport `platforms`, `setupScriptWindows` and
`integrity.setupScriptWindowsSha256` documented (#42); the setup-script
interpreter corrected — it is `sh`, or bash when the shebang says so, or
PowerShell for the Windows script, never unconditionally `bash` as this guide
previously claimed.

*2026-07-25:* `platforms` documented (#41) — the vetted-platform list, the pre-clone install/update refusal, `--ignore-platform`, and the browse marking; the install steps now start with the platform gate.

*2026-07-22:* reframed from "KDE Discover" to dmcp (the actual consumer) throughout; manifest-referenced entries with SHA-256 `integrity` and the `trustStatus` trust model documented; index schema corrected (`servers` map, `location` key, `version`/`updated` top-level); setup scripts run with `sh` and receive `MCP_INSTALL_DIR`/`MCP_CONFIG_<KEY>` env vars; config is delivered to servers as environment variables (defaults not auto-applied, no pre-install dialog in dmcp); no registry cache and no remote-endpoint probe; unsupported legacy transport format marked as such; `source.rev` pinning documented; no automatic upgrade detection; User-Agent is `dmcp/1.0`.
