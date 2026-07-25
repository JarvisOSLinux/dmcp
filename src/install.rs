//! Install and uninstall MCP servers.

use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::discovery;
use crate::elevation::{remove_dir_elevated, write_file_elevated};
use crate::paths::Paths;
use crate::setup;
use crate::sources::list_sources;

/// Install a server from registry by id.
/// When server_override is Some, uses it instead of fetching (avoids double fetch when main already fetched for scope resolution).
/// When run_setup is true and the server has a setupScript, runs it after writing the manifest.
/// When ignore_platform is true, install even though the registry does not vouch
/// for this host (`--ignore-platform`).
pub fn install(
    paths: &Paths,
    id: &str,
    scope: crate::discovery::Scope,
    server_override: Option<serde_json::Value>,
    run_setup: bool,
    ignore_platform: bool,
) -> Result<(), InstallError> {
    let server = match server_override {
        Some(s) => s,
        None => fetch_server_from_registry(paths, id)?,
    };

    // The platform gate sits ahead of every side effect: the wipe-and-recreate
    // below would destroy a working install, and a clone or setup script on an
    // unvetted host runs code the registry never checked there.
    if !ignore_platform {
        if let Some(refusal) = crate::platform::check_host(&server) {
            return Err(InstallError::PlatformUnsupported(refusal));
        }
    }

    let install_dir = match scope {
        crate::discovery::Scope::User => paths.user_install_dir().join(id),
        crate::discovery::Scope::System => paths.system_install_dir().join(id),
    };

    // Clear any stale files from a previous failed install so that
    // copy_dir_all never hits EACCES on read-only remnants.
    let _ = std::fs::remove_dir_all(&install_dir);
    std::fs::create_dir_all(&install_dir).map_err(InstallError::CreateDir)?;

    let transports = server
        .get("transports")
        .and_then(|t| t.as_array())
        .ok_or(InstallError::InvalidRegistry)?;

    let first_transport = transports.first().ok_or(InstallError::InvalidRegistry)?;
    // Whether there is a repo to clone depends on the transport this host would
    // actually launch: a manifest may pair a stdio launch line for one platform
    // with a remote endpoint for another. When no transport is declared for this
    // host the first entry still decides, exactly as before — refusing an
    // unvouched host is the platform gate's job above, not this line's.
    let selected_transport = crate::transport::select_json(transports).unwrap_or(first_transport);
    let transport_type = selected_transport
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("");

    if transport_type == "stdio" {
        install_stdio(&server, &install_dir)?;
    } else if transport_type == "sse" || transport_type == "websocket" {
        // Remote: just write manifest
    } else {
        return Err(InstallError::UnsupportedTransport);
    }

    // Build manifest
    let mut manifest = server.clone();
    manifest["installDir"] = serde_json::Value::String(install_dir.to_string_lossy().to_string());
    if manifest.get("config").is_none() {
        manifest["config"] = serde_json::json!({});
    }

    let manifest_path = install_dir.join("manifest.json");
    let mut setup_failure: Option<String> = None;

    if run_setup {
        // --- setupScript delivery from registry ---
        // The script lives in the registry repo (servers/<id>/setup.sh, or
        // setup.ps1 on Windows), not in the cloned server repo.
        // fetch_server_from_registry set the URL when the matching integrity
        // hash is present; we download, verify, and write it here so the
        // run_setup block below finds it locally.
        let spec = host_setup_script(crate::platform::host_platform());
        let (script_delivered, deliver_label) =
            match deliver_setup_script(&server, &install_dir, spec) {
                DeliverStatus::Downloaded => (true, "ok".to_string()),
                s => (false, s.label()),
            };
        if script_delivered {
            manifest[spec.manifest_key] = serde_json::json!(spec.file_name);
        }

        // --- run setup script ---
        if let Some(setup_script_ref) = crate::setup::script_for_host(
            crate::platform::host_platform(),
            manifest
                .get(WINDOWS_SETUP.manifest_key)
                .and_then(|v| v.as_str()),
            manifest
                .get(POSIX_SETUP.manifest_key)
                .and_then(|v| v.as_str()),
        ) {
            // Clone to owned String to end the immutable borrow on `manifest`
            // before the mutable index assignments below.
            let setup_script = setup_script_ref.to_string();
            let config = manifest
                .get("config")
                .and_then(|c| c.as_object())
                .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            match setup::run_setup(&setup_script, &install_dir, &config) {
                Ok(()) => {
                    manifest["setupScriptStatus"] = serde_json::json!("ok");
                    manifest["setupScriptPath"] =
                        serde_json::json!(install_dir.join(&setup_script).to_string_lossy());
                    manifest["setupScriptRunAt"] = serde_json::Value::String(rfc3339_now());
                    manifest["setupScriptVersion"] = manifest
                        .get("setupScriptVersion")
                        .cloned()
                        .unwrap_or(serde_json::json!("1.0.0"));
                }
                Err(e) => {
                    manifest["setupScriptStatus"] = serde_json::json!(format!("failed: {}", e));
                    setup_failure = Some(e.to_string());
                }
            }
            // The manifest and index are still written on setup failure so the
            // install is retryable via `dmcp setup`, but the failure is returned
            // below: a server whose setup did not run is usually unlaunchable
            // (missing venv, missing deps), and reporting success would hand the
            // caller a broken install that only fails later at connect time.
        } else {
            manifest["setupScriptStatus"] = serde_json::json!(deliver_label);
        }
    }

    let output = serde_json::to_string_pretty(&manifest).map_err(InstallError::Serialize)?;
    std::fs::write(&manifest_path, output).map_err(InstallError::WriteManifest)?;

    // Update index with keywords
    let keywords: Vec<String> = manifest
        .get("keywords")
        .and_then(|k| k.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    update_index_add(paths, id, &manifest_path, scope, &keywords)?;

    if let Some(detail) = setup_failure {
        return Err(InstallError::SetupFailed(format!(
            "{} (the server is installed but likely unlaunchable; \
             fix the cause and re-run `dmcp setup {}`)",
            detail, id
        )));
    }

    Ok(())
}

/// Resolve install scope: from --system/--user override, or from registry's "scope" field (default "user").
pub fn scope_from_registry_server(server: &serde_json::Value) -> crate::discovery::Scope {
    let s = server
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("user");
    if s == "system" {
        crate::discovery::Scope::System
    } else {
        crate::discovery::Scope::User
    }
}

pub fn fetch_server_from_registry(
    paths: &Paths,
    id: &str,
) -> Result<serde_json::Value, InstallError> {
    let sources = list_sources(paths, true, true);
    if sources.is_empty() {
        return Err(InstallError::NoSources);
    }

    let client = reqwest::blocking::Client::builder()
        .user_agent("dmcp/1.0")
        .build()
        .map_err(InstallError::HttpClient)?;

    for (url, _) in sources {
        let resp = client.get(&url).send().map_err(InstallError::FetchFailed)?;
        if !resp.status().is_success() {
            continue;
        }
        let registry: serde_json::Value = resp.json().map_err(InstallError::FetchFailed)?;
        let servers_val = registry
            .get("servers")
            .ok_or(InstallError::InvalidRegistry)?;

        let servers: Vec<serde_json::Value> = if let Some(arr) = servers_val.as_array() {
            arr.clone()
        } else if let Some(obj) = servers_val.as_object() {
            obj.values().cloned().collect()
        } else {
            return Err(InstallError::InvalidRegistry);
        };

        for mut server in servers {
            if server.get("id").and_then(|i| i.as_str()) != Some(id) {
                continue;
            }

            // If entry has manifest URL, fetch and merge
            if let Some(manifest_url) = server
                .get("manifest")
                .and_then(|m| m.as_str())
                .filter(|s| s.starts_with("http"))
            {
                let manifest_url = manifest_url.to_string();
                // The registry entry binds this manifest to an exact content
                // hash. Capture it now, before the merge below overwrites
                // `server` with the manifest contents.
                let expected_manifest_hash = server
                    .get("integrity")
                    .and_then(|i| i.get("manifestSha256"))
                    .and_then(|h| h.as_str())
                    .filter(|h| !h.is_empty())
                    .map(|h| h.to_string());

                let manifest_resp = client
                    .get(&manifest_url)
                    .send()
                    .map_err(InstallError::FetchFailed)?;
                if manifest_resp.status().is_success() {
                    // Hash the raw bytes exactly as fetched, before any parse
                    // or merge, so the digest matches what the registry
                    // recorded (sync_registry.py hashes the raw manifest file).
                    let raw = manifest_resp.bytes().map_err(InstallError::FetchFailed)?;
                    verify_manifest_hash(&raw, expected_manifest_hash.as_deref())?;
                    if expected_manifest_hash.is_none() {
                        eprintln!(
                            "[warn] {}: registry entry has no integrity.manifestSha256; \
                             installing an unverified manifest",
                            id
                        );
                    }
                    let mut manifest: serde_json::Value =
                        serde_json::from_slice(&raw).map_err(|_| InstallError::InvalidRegistry)?;
                    // Merge: registry entry overrides manifest (for scope, keywords, etc.)
                    merge_json(&mut manifest, &server);
                    server = manifest;
                }
            }

            attach_setup_script_urls(&mut server);

            return Ok(server);
        }
    }

    Err(InstallError::ServerNotFound)
}

// ---------------------------------------------------------------------------
// Setup script delivery
// ---------------------------------------------------------------------------

/// Where one platform's setup script lives in a registry entry: the manifest
/// field that names it, the URL derived for it, the integrity hash that binds
/// its bytes, and the filename it is delivered as.
///
/// The two platforms share this one table so they cannot drift into different
/// rules. A per-platform script is not a hash-verification hole: whichever
/// script this host runs, it got here through the same download-verify-write
/// gate below.
#[derive(Clone, Copy)]
struct SetupScriptSpec {
    manifest_key: &'static str,
    url_key: &'static str,
    integrity_key: &'static str,
    file_name: &'static str,
}

const POSIX_SETUP: SetupScriptSpec = SetupScriptSpec {
    manifest_key: "setupScript",
    url_key: "setupScriptUrl",
    integrity_key: "setupScriptSha256",
    file_name: "setup.sh",
};

const WINDOWS_SETUP: SetupScriptSpec = SetupScriptSpec {
    manifest_key: "setupScriptWindows",
    url_key: "setupScriptWindowsUrl",
    integrity_key: "setupScriptWindowsSha256",
    file_name: "setup.ps1",
};

/// The setup script a given host installs.
fn host_setup_script(host: &str) -> SetupScriptSpec {
    if host == "windows" {
        WINDOWS_SETUP
    } else {
        POSIX_SETUP
    }
}

/// Derive the registry-hosted setup script URLs for an entry — one per platform
/// variant, each only when the registry recorded a hash to verify it against.
/// The scripts sit beside the manifest in the registry repo
/// (`.../servers/<id>/setup.sh`, `.../servers/<id>/setup.ps1`).
///
/// Both are derived regardless of host so this stays a property of the entry;
/// which one is downloaded and run is decided at install time.
fn attach_setup_script_urls(server: &mut serde_json::Value) {
    let Some(base) = server
        .get("manifest")
        .and_then(|m| m.as_str())
        .and_then(|url| url.strip_suffix("manifest.json"))
        .map(str::to_string)
    else {
        return;
    };
    for spec in [POSIX_SETUP, WINDOWS_SETUP] {
        let has_hash = server
            .get("integrity")
            .and_then(|i| i.get(spec.integrity_key))
            .and_then(|h| h.as_str())
            .filter(|h| !h.is_empty())
            .is_some();
        if has_hash {
            server[spec.url_key] = serde_json::json!(format!("{}{}", base, spec.file_name));
        }
    }
}

enum DeliverStatus {
    Downloaded,
    SkippedNoScript,
    SkippedDownloadFailed(String),
    SkippedHashMismatch,
    SkippedWriteFailed(String),
}

impl DeliverStatus {
    fn label(self) -> String {
        match self {
            DeliverStatus::Downloaded => "ok".into(),
            DeliverStatus::SkippedNoScript => "skipped: no script defined".into(),
            DeliverStatus::SkippedDownloadFailed(e) => {
                format!("skipped: download failed: {}", e)
            }
            DeliverStatus::SkippedHashMismatch => "skipped: hash mismatch".into(),
            DeliverStatus::SkippedWriteFailed(e) => format!("skipped: write failed: {}", e),
        }
    }
}

/// Download this host's registry-hosted setup script, SHA-256 verify it, and
/// write it into the install dir. Returns the delivery outcome so the caller can
/// set `setupScriptStatus` in the manifest accordingly.
fn deliver_setup_script(
    server: &serde_json::Value,
    install_dir: &Path,
    spec: SetupScriptSpec,
) -> DeliverStatus {
    let url = match server.get(spec.url_key).and_then(|u| u.as_str()) {
        Some(u) => u.to_string(),
        None => return DeliverStatus::SkippedNoScript,
    };

    let expected_hash = match server
        .get("integrity")
        .and_then(|i| i.get(spec.integrity_key))
        .and_then(|h| h.as_str())
        .filter(|h| !h.is_empty())
    {
        Some(h) => h.to_string(),
        None => return DeliverStatus::SkippedNoScript,
    };

    let client = match reqwest::blocking::Client::builder()
        .user_agent("dmcp/1.0")
        .build()
    {
        Ok(c) => c,
        Err(e) => return DeliverStatus::SkippedDownloadFailed(e.to_string()),
    };

    let resp = match client.get(&url).send() {
        Ok(r) => r,
        Err(e) => return DeliverStatus::SkippedDownloadFailed(e.to_string()),
    };
    if !resp.status().is_success() {
        return DeliverStatus::SkippedDownloadFailed(format!("HTTP {}", resp.status()));
    }
    let content = match resp.bytes() {
        Ok(b) => b,
        Err(e) => return DeliverStatus::SkippedDownloadFailed(e.to_string()),
    };

    accept_setup_script(&content, &expected_hash, &install_dir.join(spec.file_name))
}

/// The verification gate every delivered setup script passes: the bytes are
/// written only if they hash to what the registry recorded. Split from the
/// download so the gate itself is exercised without a network, and so both
/// platform variants provably share it.
fn accept_setup_script(content: &[u8], expected_hash: &str, dest: &Path) -> DeliverStatus {
    let actual_hash = format!("{:x}", Sha256::digest(content));
    if actual_hash != expected_hash {
        return DeliverStatus::SkippedHashMismatch;
    }

    if let Err(e) = std::fs::write(dest, content) {
        return DeliverStatus::SkippedWriteFailed(e.to_string());
    }

    // Mark executable on Unix so `sh setup.sh` isn't required by run_setup
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755));
    }

    DeliverStatus::Downloaded
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Verify fetched manifest bytes against the SHA-256 the registry recorded.
///
/// `expected == None` means the entry carries no `integrity.manifestSha256`;
/// the manifest is installed unverified (the caller warns). `Some(hash)` is a
/// binding commitment: a mismatch aborts the install so a tampered manifest
/// (altered command/args/source/scope) cannot land undetected.
fn verify_manifest_hash(raw: &[u8], expected: Option<&str>) -> Result<(), InstallError> {
    if let Some(expected) = expected {
        let actual = format!("{:x}", Sha256::digest(raw));
        if actual != expected {
            return Err(InstallError::ManifestHashMismatch {
                expected: expected.to_string(),
                actual,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Trust policy (see mcp-registry docs/TRUST-MODEL.md)
// ---------------------------------------------------------------------------

/// Outcome of a trust-policy check for an install.
pub enum TrustGate {
    Allow,
    Warn(String),
    Deny(String),
}

/// The trust tier recorded for a server entry, defaulting to the most cautious
/// tier when the field is absent.
pub fn trust_status(server: &serde_json::Value) -> &str {
    server
        .get("trustStatus")
        .and_then(|t| t.as_str())
        .unwrap_or("community")
}

/// Whether the agent path may install community-tier servers. Default is
/// permissive by policy: every registry entry is PR-vetted, so `community` is
/// reviewed-but-not-maintainer-endorsed, not unvetted, and the agent may install
/// it. A deployment can set `DMCP_AGENT_ALLOW_COMMUNITY=0` to opt into
/// official-only as a hardening posture. See mcp-registry TRUST-MODEL.md §2.2.
pub fn agent_allow_community_from_env() -> bool {
    match std::env::var("DMCP_AGENT_ALLOW_COMMUNITY") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// Policy for the autonomous-agent path (`dmcp serve`). The agent inherits the
/// human's configured sources and installs both `official` and (by default)
/// `community` entries from them — the confinement to human-configured sources,
/// not a tier allowlist, is the boundary. `community` installs carry a
/// not-maintainer-reviewed warning; `DMCP_AGENT_ALLOW_COMMUNITY=0` narrows the
/// agent to official-only. `deprecated`/`removed` servers are never installable
/// by the agent.
pub fn agent_trust_gate(status: &str, allow_community: bool) -> TrustGate {
    match status {
        "official" => TrustGate::Allow,
        "removed" => TrustGate::Deny("server has been removed from the registry".into()),
        "deprecated" => {
            TrustGate::Deny("server is deprecated; not installable by the agent".into())
        }
        other => {
            if allow_community {
                TrustGate::Warn(format!(
                    "installing '{}'-tier server: not maintainer-reviewed",
                    other
                ))
            } else {
                TrustGate::Deny(format!(
                    "agent policy allows official-tier only; '{}' refused \
                     (set DMCP_AGENT_ALLOW_COMMUNITY=1 to allow community-tier)",
                    other
                ))
            }
        }
    }
}

/// Policy for the human CLI path. The operator may install any tier; a
/// community install carries a "you are trusting the submitter" warning, and a
/// removed server is refused. See TRUST-MODEL.md §2.1.
pub fn cli_trust_gate(status: &str) -> TrustGate {
    match status {
        "official" => TrustGate::Allow,
        "removed" => TrustGate::Deny("server has been removed from the registry".into()),
        "deprecated" => TrustGate::Warn("server is deprecated".into()),
        other => TrustGate::Warn(format!(
            "'{}'-tier server: not maintainer-reviewed — you are trusting the submitter",
            other
        )),
    }
}

/// True for a full 40-hex-char commit SHA (vs. a tag or short ref).
fn is_full_commit_sha(rev: &str) -> bool {
    rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Confirm the checked-out HEAD matches the pinned commit SHA.
fn verify_rev(head: &str, requested: &str) -> Result<(), InstallError> {
    if head.eq_ignore_ascii_case(requested) {
        Ok(())
    } else {
        Err(InstallError::SourceRevMismatch {
            requested: requested.to_string(),
            got: head.to_string(),
        })
    }
}

/// Merge b into a. Keys in b override a.
fn merge_json(a: &mut serde_json::Value, b: &serde_json::Value) {
    if let (Some(a_obj), Some(b_obj)) = (a.as_object_mut(), b.as_object()) {
        for (k, v) in b_obj {
            if !v.is_null() {
                a_obj.insert(k.clone(), v.clone());
            }
        }
    }
}

fn install_stdio(server: &serde_json::Value, install_dir: &Path) -> Result<(), InstallError> {
    let source = server
        .get("source")
        .and_then(|s| s.as_object())
        .ok_or(InstallError::InvalidRegistry)?;
    let url = source
        .get("url")
        .and_then(|u| u.as_str())
        .ok_or(InstallError::InvalidRegistry)?;
    let path = source.get("path").and_then(|p| p.as_str()).unwrap_or("");
    let rev = source
        .get("rev")
        .and_then(|r| r.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let temp = std::env::temp_dir().join(format!("dmcp-clone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).map_err(InstallError::CreateDir)?;

    let temp_str = temp.to_str().ok_or(InstallError::InvalidRegistry)?;
    // With a pinned rev, fetch full history (blobless to stay cheap) so any
    // commit is reachable for checkout; otherwise take the fast shallow path.
    let clone_args: Vec<&str> = if rev.is_some() {
        vec!["clone", "--filter=blob:none", url, temp_str]
    } else {
        vec!["clone", "--depth", "1", "--filter=blob:none", url, temp_str]
    };
    let status = Command::new("git")
        .args(&clone_args)
        .status()
        .map_err(InstallError::GitFailed)?;
    if !status.success() {
        return Err(InstallError::GitFailed(std::io::Error::other(
            "git clone failed",
        )));
    }

    if let Some(rev) = rev {
        let checkout = Command::new("git")
            .args(["-C", temp_str, "checkout", "--detach", rev])
            .status()
            .map_err(InstallError::GitFailed)?;
        if !checkout.success() {
            return Err(InstallError::SourceCheckoutFailed(rev.to_string()));
        }
        // A full commit SHA is a binding pin: confirm HEAD is exactly it, so a
        // tampered or moved ref cannot substitute different code.
        if is_full_commit_sha(rev) {
            let head = Command::new("git")
                .args(["-C", temp_str, "rev-parse", "HEAD"])
                .output()
                .map_err(InstallError::GitFailed)?;
            let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
            verify_rev(&head, rev)?;
        }
    }

    let src = if path.is_empty() {
        temp.clone()
    } else {
        temp.join(path)
    };

    if !src.exists() {
        return Err(InstallError::InvalidRegistry);
    }

    copy_dir_all(&src, install_dir).map_err(InstallError::CopyFailed)?;
    std::fs::remove_dir_all(&temp).ok();

    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), dst_path)?;
        }
    }
    Ok(())
}

pub fn update_index_add(
    paths: &Paths,
    id: &str,
    manifest_path: &Path,
    scope: crate::discovery::Scope,
    keywords: &[String],
) -> Result<(), InstallError> {
    let index_path = match scope {
        crate::discovery::Scope::User => paths.user_install_dir().join("index.json"),
        crate::discovery::Scope::System => paths.system_install_dir().join("index.json"),
    };

    let content = std::fs::read_to_string(&index_path)
        .unwrap_or_else(|_| r#"{"servers":{},"version":"1.0"}"#.to_string());
    let mut index: serde_json::Value =
        serde_json::from_str(&content).map_err(InstallError::ParseIndex)?;

    if index.get("servers").is_none() {
        index["servers"] = serde_json::json!({});
    }
    index["servers"][id] = serde_json::json!({
        "location": manifest_path.to_string_lossy(),
        "keywords": keywords,
    });
    index["updated"] = serde_json::Value::String(rfc3339_now());

    let output = serde_json::to_string_pretty(&index).map_err(InstallError::Serialize)?;

    if scope == crate::discovery::Scope::System {
        write_file_elevated(&index_path, output.as_bytes()).map_err(InstallError::WriteIndex)?;
    } else {
        std::fs::write(&index_path, output).map_err(InstallError::WriteIndex)?;
    }

    Ok(())
}

#[derive(Debug)]
pub enum InstallError {
    NoSources,
    ServerNotFound,
    InvalidRegistry,
    UnsupportedTransport,
    HttpClient(reqwest::Error),
    FetchFailed(reqwest::Error),
    CreateDir(std::io::Error),
    GitFailed(std::io::Error),
    CopyFailed(std::io::Error),
    Serialize(serde_json::Error),
    WriteManifest(std::io::Error),
    SetupFailed(String),
    ParseIndex(serde_json::Error),
    WriteIndex(std::io::Error),
    ManifestHashMismatch { expected: String, actual: String },
    SourceCheckoutFailed(String),
    SourceRevMismatch { requested: String, got: String },
    TrustDenied(String),
    PlatformUnsupported(crate::platform::UnsupportedHost),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::NoSources => write!(f, "No registry sources configured"),
            InstallError::ServerNotFound => write!(f, "Server not found in any registry"),
            InstallError::InvalidRegistry => write!(f, "Invalid registry or server entry"),
            InstallError::UnsupportedTransport => write!(f, "Unsupported transport type"),
            InstallError::HttpClient(e) => write!(f, "HTTP client error: {}", e),
            InstallError::FetchFailed(e) => write!(f, "Failed to fetch registry: {}", e),
            InstallError::CreateDir(e) => write!(f, "Failed to create directory: {}", e),
            InstallError::GitFailed(e) => write!(f, "Git operation failed: {}", e),
            InstallError::CopyFailed(e) => write!(f, "Failed to copy files: {}", e),
            InstallError::Serialize(e) => write!(f, "Failed to serialize: {}", e),
            InstallError::WriteManifest(e) => write!(f, "Failed to write manifest: {}", e),
            InstallError::SetupFailed(s) => write!(f, "Setup failed: {}", s),
            InstallError::ParseIndex(e) => write!(f, "Failed to parse index: {}", e),
            InstallError::WriteIndex(e) => write!(f, "Failed to write index: {}", e),
            InstallError::ManifestHashMismatch { expected, actual } => write!(
                f,
                "Manifest integrity check failed: expected SHA-256 {}, got {} \
                 (the fetched manifest does not match what the registry recorded)",
                expected, actual
            ),
            InstallError::SourceCheckoutFailed(rev) => {
                write!(f, "Failed to check out pinned source revision {}", rev)
            }
            InstallError::SourceRevMismatch { requested, got } => write!(
                f,
                "Pinned source revision mismatch: requested {}, got {}",
                requested, got
            ),
            InstallError::TrustDenied(reason) => {
                write!(f, "Refused by trust policy: {}", reason)
            }
            InstallError::PlatformUnsupported(refusal) => write!(f, "{}", refusal),
        }
    }
}

impl std::error::Error for InstallError {}

/// Uninstall a server by id. Removes install dir and updates index.
pub fn uninstall(paths: &Paths, id: &str) -> Result<(), UninstallError> {
    let (manifest_path, install_dir, scope) =
        discovery::get_uninstall_info(paths, id).ok_or(UninstallError::ServerNotFound)?;

    // Remove install directory. An already-absent directory is not a failure:
    // the index entry is the thing that would otherwise be left behind, naming
    // a server no command can act on.
    if scope == crate::discovery::Scope::System {
        remove_dir_elevated(&install_dir).map_err(UninstallError::RmFailed)?;
    } else if let Err(e) = std::fs::remove_dir_all(&install_dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return Err(UninstallError::RmFailed(e));
        }
    }

    // Update index - remove the entry
    let index_path = if manifest_path.starts_with(paths.user_install_dir()) {
        paths.user_install_dir().join("index.json")
    } else {
        paths.system_install_dir().join("index.json")
    };

    update_index_remove(&index_path, id, scope)?;

    // Remove any vector index entries for this server (best-effort, no failure on error)
    let vector_index_path = paths.vector_index_path();
    if let Ok(mut vidx) = crate::vector_index::VectorIndex::load(&vector_index_path) {
        vidx.remove_server_entries(id);
        let _ = vidx.save(&vector_index_path);
    }

    Ok(())
}

fn update_index_remove(
    index_path: &Path,
    id: &str,
    scope: crate::discovery::Scope,
) -> Result<(), UninstallError> {
    let content = std::fs::read_to_string(index_path).map_err(UninstallError::ReadIndex)?;
    let mut index: serde_json::Value =
        serde_json::from_str(&content).map_err(UninstallError::ParseIndex)?;
    if let Some(servers) = index.get_mut("servers").and_then(|s| s.as_object_mut()) {
        servers.remove(id);
    }
    index["updated"] = serde_json::Value::String(rfc3339_now());
    let output = serde_json::to_string_pretty(&index).map_err(UninstallError::SerializeIndex)?;

    if scope == crate::discovery::Scope::System {
        write_file_elevated(index_path, output.as_bytes()).map_err(UninstallError::WriteIndex)?;
    } else {
        std::fs::write(index_path, output).map_err(UninstallError::WriteIndex)?;
    }

    Ok(())
}

#[derive(Debug)]
pub enum UninstallError {
    ServerNotFound,
    RmFailed(std::io::Error),
    ReadIndex(std::io::Error),
    ParseIndex(serde_json::Error),
    SerializeError(serde_json::Error),
    SerializeIndex(serde_json::Error),
    WriteIndex(std::io::Error),
}

impl std::fmt::Display for UninstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UninstallError::ServerNotFound => write!(f, "Server not found"),
            UninstallError::RmFailed(e) => write!(f, "Failed to remove: {}", e),
            UninstallError::ReadIndex(e) => write!(f, "Failed to read index: {}", e),
            UninstallError::ParseIndex(e) => write!(f, "Failed to parse index: {}", e),
            UninstallError::SerializeError(e) => write!(f, "Failed to serialize: {}", e),
            UninstallError::SerializeIndex(e) => write!(f, "Failed to serialize index: {}", e),
            UninstallError::WriteIndex(e) => write!(f, "Failed to write index: {}", e),
        }
    }
}

impl std::error::Error for UninstallError {}

pub fn rfc3339_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs() as i64;
    let nsecs = d.subsec_nanos();
    let (year, month, day, hour, min, sec) = epoch_to_datetime(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
        year, month, day, hour, min, sec, nsecs
    )
}

fn epoch_to_datetime(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs / 86400;
    let time = secs.rem_euclid(86400);
    let hour = (time / 3600) as u32;
    let min = ((time % 3600) / 60) as u32;
    let sec = (time % 60) as u32;
    let (y, m, d) = days_to_ymd(days);
    (y, m, d, hour, min, sec)
}

fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let days = days + 719468;
    let era = days / 146097;
    let day_of_era = days - era * 146097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let (month, day) = doy_to_md(day_of_year as u32);
    (year, month, day)
}

fn doy_to_md(doy: u32) -> (u32, u32) {
    let days_in_month = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut d = doy + 1;
    for (i, &dim) in days_in_month.iter().enumerate() {
        if d <= dim {
            return ((i + 1) as u32, d);
        }
        d -= dim;
    }
    (12, 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Temp tree removed on drop; keeps the install tree off the real XDG paths.
    struct TempTree {
        root: std::path::PathBuf,
    }

    impl TempTree {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!(
                "dmcp-install-test-{}-{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&root).unwrap();
            TempTree { root }
        }

        fn paths(&self) -> Paths {
            Paths {
                user_sources: self.root.join("user/sources.list"),
                user_install_dir: self.root.join("user/installed"),
                system_sources: self.root.join("system/sources.list"),
                system_install_dir: self.root.join("system/installed"),
                vector_index_dir: self.root.join("vector"),
            }
        }

        /// Write an executable script and return its absolute path. `run_setup`
        /// joins the install dir with the reference, and joining an absolute
        /// path yields it unchanged — so the script survives the install dir
        /// being wiped and recreated.
        fn script(&self, name: &str, body: &str) -> String {
            let path = self.root.join(name);
            std::fs::write(&path, body).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path.to_string_lossy().to_string()
        }
    }

    /// An sse-transport entry installs without a git clone or a registry fetch,
    /// so the setup-script outcome is the only thing under test.
    fn remote_server(setup_script: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "test.server",
            "version": "1.0.0",
            "scope": "user",
            "transports": [{ "type": "sse", "url": "https://example.invalid/sse" }],
            "setupScript": setup_script,
        })
    }

    #[test]
    fn setup_failure_is_reported_not_swallowed() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let script = tree.script("fail.sh", "#!/bin/sh\necho boom >&2\nexit 3\n");

        let err = install(
            &paths,
            "test.server",
            crate::discovery::Scope::User,
            Some(remote_server(&script)),
            true,
            false,
        )
        .unwrap_err();

        assert!(
            matches!(err, InstallError::SetupFailed(_)),
            "a failing setup script must surface as an error, got {:?}",
            err
        );
        assert!(err.to_string().contains("dmcp setup test.server"));
    }

    #[test]
    fn setup_failure_still_leaves_a_retryable_install() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let script = tree.script("fail.sh", "#!/bin/sh\nexit 1\n");

        let _ = install(
            &paths,
            "test.server",
            crate::discovery::Scope::User,
            Some(remote_server(&script)),
            true,
            false,
        );

        // The manifest is deliberately kept so `dmcp setup <id>` can retry.
        let manifest_path = paths.user_install_dir().join("test.server/manifest.json");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert!(manifest["setupScriptStatus"]
            .as_str()
            .unwrap()
            .starts_with("failed:"));
    }

    #[test]
    fn successful_setup_installs_cleanly() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let script = tree.script("ok.sh", "#!/bin/sh\nexit 0\n");

        install(
            &paths,
            "test.server",
            crate::discovery::Scope::User,
            Some(remote_server(&script)),
            true,
            false,
        )
        .expect("a setup script that exits 0 must install cleanly");

        let manifest_path = paths.user_install_dir().join("test.server/manifest.json");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["setupScriptStatus"], "ok");
    }

    /// A remote entry whose `platforms` list is passed straight through, plus a
    /// setup script that leaves a sentinel file: the sentinel is how a test tells
    /// "refused up front" from "refused after running the server's code".
    fn platform_server(platforms: Option<&[&str]>, setup_script: &str) -> serde_json::Value {
        let mut server = remote_server(setup_script);
        if let Some(p) = platforms {
            server["platforms"] = serde_json::json!(p);
        }
        server
    }

    #[test]
    fn unsupported_platform_is_refused_before_any_side_effect() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let sentinel = tree.root.join("setup-ran");
        let script = tree.script(
            "touch.sh",
            &format!("#!/bin/sh\ntouch {}\n", sentinel.display()),
        );

        let err = install(
            &paths,
            "test.server",
            crate::discovery::Scope::User,
            Some(platform_server(
                Some(&[crate::platform::foreign_platform()]),
                &script,
            )),
            true,
            false,
        )
        .unwrap_err();

        assert!(
            matches!(err, InstallError::PlatformUnsupported(_)),
            "a host outside the registry's platforms must be refused, got {:?}",
            err
        );
        let msg = err.to_string();
        assert!(
            msg.contains(crate::platform::foreign_platform()),
            "refusal names the supported platforms: {msg}"
        );
        assert!(
            msg.contains("--ignore-platform"),
            "refusal names the override: {msg}"
        );
        assert!(
            !sentinel.exists(),
            "setup must not run on a refused install"
        );
        assert!(
            !paths.user_install_dir().join("test.server").exists(),
            "a refused install must not create an install directory"
        );
    }

    #[test]
    fn refusal_leaves_an_existing_install_untouched() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let script = tree.script("ok.sh", "#!/bin/sh\nexit 0\n");

        install(
            &paths,
            "test.server",
            crate::discovery::Scope::User,
            Some(platform_server(None, &script)),
            true,
            false,
        )
        .expect("baseline install");
        let manifest_path = paths.user_install_dir().join("test.server/manifest.json");
        let before = std::fs::read_to_string(&manifest_path).unwrap();

        // A later registry entry that no longer vouches for this host must not
        // wipe the working copy on its way to refusing.
        let err = install(
            &paths,
            "test.server",
            crate::discovery::Scope::User,
            Some(platform_server(
                Some(&[crate::platform::foreign_platform()]),
                &script,
            )),
            true,
            false,
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::PlatformUnsupported(_)));
        assert_eq!(before, std::fs::read_to_string(&manifest_path).unwrap());
    }

    #[test]
    fn ignore_platform_overrides_the_refusal() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let script = tree.script("ok.sh", "#!/bin/sh\nexit 0\n");

        install(
            &paths,
            "test.server",
            crate::discovery::Scope::User,
            Some(platform_server(
                Some(&[crate::platform::foreign_platform()]),
                &script,
            )),
            true,
            true,
        )
        .expect("--ignore-platform must install on an unvouched host");

        let manifest_path = paths.user_install_dir().join("test.server/manifest.json");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["setupScriptStatus"], "ok");
        // The declared coverage is recorded as installed, not rewritten.
        assert_eq!(
            manifest["platforms"],
            serde_json::json!([crate::platform::foreign_platform()])
        );
    }

    #[test]
    fn absent_platforms_installs_unrestricted() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let script = tree.script("ok.sh", "#!/bin/sh\nexit 0\n");

        install(
            &paths,
            "test.server",
            crate::discovery::Scope::User,
            Some(platform_server(None, &script)),
            true,
            false,
        )
        .expect("an entry without platforms installs on any host");

        let manifest_path = paths.user_install_dir().join("test.server/manifest.json");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert!(manifest.get("platforms").is_none());
    }

    /// The shape that used to strand a server on disk: a manifest field dmcp
    /// cannot read. `platforms` no longer sinks the parse, and even a manifest
    /// that is unreadable for some other reason stays removable, because
    /// uninstall reads the index rather than the manifest.
    #[test]
    fn a_server_with_an_unreadable_manifest_is_still_removable() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let script = tree.script("ok.sh", "#!/bin/sh\nexit 0\n");

        let mut server = platform_server(None, &script);
        server["platforms"] = serde_json::json!("windows");
        install(
            &paths,
            "test.server",
            crate::discovery::Scope::User,
            Some(server),
            true,
            true,
        )
        .expect("--ignore-platform installs the entry as written");

        // The malformed field survives to disk and still parses back.
        let manifest_path = paths.user_install_dir().join("test.server/manifest.json");
        let raw = std::fs::read_to_string(&manifest_path).unwrap();
        assert!(raw.contains(r#""platforms": "windows""#));
        let typed: crate::models::Manifest = serde_json::from_str(&raw).unwrap();
        assert!(typed.platforms.is_malformed());
        assert!(
            discovery::get_server(&paths, "test.server").is_some(),
            "the server stays visible to every command"
        );

        // And an outright unparseable manifest is still removable.
        std::fs::write(&manifest_path, "{not json").unwrap();
        uninstall(&paths, "test.server").expect("uninstall must not need a readable manifest");
        assert!(!paths.user_install_dir().join("test.server").exists());
        let index: serde_json::Value = serde_json::from_slice(
            &std::fs::read(paths.user_install_dir().join("index.json")).unwrap(),
        )
        .unwrap();
        assert!(index["servers"].get("test.server").is_none());
    }

    #[test]
    fn declared_host_platform_installs() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let script = tree.script("ok.sh", "#!/bin/sh\nexit 0\n");

        install(
            &paths,
            "test.server",
            crate::discovery::Scope::User,
            Some(platform_server(
                Some(&[crate::platform::host_platform()]),
                &script,
            )),
            true,
            false,
        )
        .expect("an entry vouching for this host installs without the override");
    }

    // -----------------------------------------------------------------------
    // Per-platform setup scripts (#42)
    // -----------------------------------------------------------------------

    #[test]
    fn each_host_installs_its_own_setup_script() {
        let windows = host_setup_script("windows");
        assert_eq!(windows.manifest_key, "setupScriptWindows");
        assert_eq!(windows.integrity_key, "setupScriptWindowsSha256");
        assert_eq!(windows.file_name, "setup.ps1");

        for host in ["linux", "darwin", "freebsd"] {
            let posix = host_setup_script(host);
            assert_eq!(posix.manifest_key, "setupScript", "host {host}");
            assert_eq!(posix.integrity_key, "setupScriptSha256", "host {host}");
            assert_eq!(posix.file_name, "setup.sh", "host {host}");
        }
    }

    /// Both scripts sit beside the manifest in the registry repo, and each URL
    /// appears only when there is a hash to verify it against — an unverifiable
    /// script is not fetched at all.
    #[test]
    fn setup_script_urls_are_derived_per_platform_only_when_hashed() {
        let base = "https://raw.example.invalid/servers/test.server/";
        let mut server = serde_json::json!({
            "manifest": format!("{}manifest.json", base),
            "integrity": {
                "setupScriptSha256": "aa",
                "setupScriptWindowsSha256": "bb",
            },
        });
        attach_setup_script_urls(&mut server);
        assert_eq!(server["setupScriptUrl"], format!("{}setup.sh", base));
        assert_eq!(
            server["setupScriptWindowsUrl"],
            format!("{}setup.ps1", base)
        );

        let mut posix_only = serde_json::json!({
            "manifest": format!("{}manifest.json", base),
            "integrity": { "setupScriptSha256": "aa" },
        });
        attach_setup_script_urls(&mut posix_only);
        assert_eq!(posix_only["setupScriptUrl"], format!("{}setup.sh", base));
        assert!(posix_only.get("setupScriptWindowsUrl").is_none());

        let mut unhashed = serde_json::json!({
            "manifest": format!("{}manifest.json", base),
            "integrity": { "setupScriptSha256": "", "setupScriptWindowsSha256": "" },
        });
        attach_setup_script_urls(&mut unhashed);
        assert!(unhashed.get("setupScriptUrl").is_none());
        assert!(unhashed.get("setupScriptWindowsUrl").is_none());
    }

    /// The Windows script goes through the same gate as the POSIX one: bytes
    /// that do not match the registry's SHA-256 are never written, so a
    /// per-platform script cannot become a hash-verification hole.
    #[test]
    fn a_setup_script_is_written_only_when_its_hash_matches() {
        let tree = TempTree::new();
        for spec in [POSIX_SETUP, WINDOWS_SETUP] {
            let content = format!("# {}\n", spec.file_name).into_bytes();
            let expected = format!("{:x}", Sha256::digest(&content));
            let dest = tree.root.join(spec.file_name);

            let status = accept_setup_script(&content, &expected, &dest);
            assert!(
                matches!(status, DeliverStatus::Downloaded),
                "{} with a matching hash must be delivered",
                spec.file_name
            );
            assert_eq!(std::fs::read(&dest).unwrap(), content);

            std::fs::remove_file(&dest).unwrap();
            let tampered = b"# rm -rf /\n";
            let status = accept_setup_script(tampered, &expected, &dest);
            assert!(
                matches!(status, DeliverStatus::SkippedHashMismatch),
                "{} whose bytes changed must be refused",
                spec.file_name
            );
            assert!(
                !dest.exists(),
                "{} must not be written when the hash mismatches",
                spec.file_name
            );
        }
    }

    /// A single-byte edit to a delivered script is caught, the same way it is
    /// for a manifest.
    #[test]
    fn a_tampered_windows_script_is_rejected() {
        let tree = TempTree::new();
        let reviewed = b"Install-Module -Name Safe\n";
        let expected = format!("{:x}", Sha256::digest(reviewed));
        let tampered = b"Install-Module -Name Evil\n";
        let dest = tree.root.join("setup.ps1");
        assert!(matches!(
            accept_setup_script(tampered, &expected, &dest),
            DeliverStatus::SkippedHashMismatch
        ));
        assert!(!dest.exists());
    }

    /// A manifest carrying both scripts runs exactly the one for this host.
    #[test]
    fn install_runs_only_this_hosts_setup_script() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let posix_ran = tree.root.join("posix-ran");
        let posix = tree.script(
            "setup.sh",
            &format!("#!/bin/sh\ntouch {}\n", posix_ran.display()),
        );
        let windows = tree.script("setup.ps1", "exit 0\n");

        let mut server = remote_server(&posix);
        server[WINDOWS_SETUP.manifest_key] = serde_json::json!(windows);

        install(
            &paths,
            "test.server",
            crate::discovery::Scope::User,
            Some(server),
            true,
            false,
        )
        .expect("install with both setup scripts");

        assert_eq!(
            posix_ran.exists(),
            crate::platform::host_platform() != "windows",
            "the POSIX script must run on POSIX hosts and only there"
        );
    }

    /// An entry that only carries the Windows script delivers nothing on a
    /// POSIX host — and an install with no script at all is unchanged.
    #[test]
    fn a_windows_only_script_is_not_delivered_to_a_posix_host() {
        let tree = TempTree::new();
        let server = serde_json::json!({
            "setupScriptWindowsUrl": "https://example.invalid/setup.ps1",
            "integrity": { "setupScriptWindowsSha256": "aa" },
        });
        assert!(matches!(
            deliver_setup_script(&server, &tree.root, POSIX_SETUP),
            DeliverStatus::SkippedNoScript
        ));
        assert!(!tree.root.join("setup.sh").exists());
        assert!(!tree.root.join("setup.ps1").exists());
    }

    #[test]
    fn manifest_hash_matches_recorded_digest() {
        let raw = br#"{"version":"1.0","scope":"user"}"#;
        let expected = format!("{:x}", Sha256::digest(raw));
        assert!(verify_manifest_hash(raw, Some(&expected)).is_ok());
    }

    #[test]
    fn manifest_hash_mismatch_is_rejected() {
        let raw = br#"{"version":"1.0"}"#;
        let wrong = "0".repeat(64);
        let err = verify_manifest_hash(raw, Some(&wrong)).unwrap_err();
        assert!(matches!(err, InstallError::ManifestHashMismatch { .. }));
    }

    #[test]
    fn absent_hash_installs_unverified() {
        let raw = br#"{"version":"1.0"}"#;
        assert!(verify_manifest_hash(raw, None).is_ok());
    }

    #[test]
    fn single_byte_tamper_is_detected() {
        // A manifest reviewed with command "safe" must not install if the
        // bytes served have been swapped to "evil".
        let reviewed = br#"{"transports":[{"type":"stdio","command":"safe"}]}"#;
        let expected = format!("{:x}", Sha256::digest(reviewed));
        let tampered = br#"{"transports":[{"type":"stdio","command":"evil"}]}"#;
        let err = verify_manifest_hash(tampered, Some(&expected)).unwrap_err();
        assert!(matches!(err, InstallError::ManifestHashMismatch { .. }));
    }

    #[test]
    fn trust_status_defaults_to_community() {
        assert_eq!(trust_status(&serde_json::json!({})), "community");
        assert_eq!(
            trust_status(&serde_json::json!({"trustStatus": "official"})),
            "official"
        );
    }

    #[test]
    fn agent_gate_allows_official_only_when_strict() {
        assert!(matches!(
            agent_trust_gate("official", false),
            TrustGate::Allow
        ));
        assert!(matches!(
            agent_trust_gate("community", false),
            TrustGate::Deny(_)
        ));
        assert!(matches!(
            agent_trust_gate("community", true),
            TrustGate::Warn(_)
        ));
        // deprecated/removed are never installable by the agent
        assert!(matches!(
            agent_trust_gate("deprecated", true),
            TrustGate::Deny(_)
        ));
        assert!(matches!(
            agent_trust_gate("removed", true),
            TrustGate::Deny(_)
        ));
    }

    #[test]
    fn cli_gate_warns_on_community_and_refuses_removed() {
        assert!(matches!(cli_trust_gate("official"), TrustGate::Allow));
        assert!(matches!(cli_trust_gate("community"), TrustGate::Warn(_)));
        assert!(matches!(cli_trust_gate("deprecated"), TrustGate::Warn(_)));
        assert!(matches!(cli_trust_gate("removed"), TrustGate::Deny(_)));
    }

    #[test]
    fn full_commit_sha_is_recognized() {
        assert!(is_full_commit_sha(
            "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0"
        ));
        assert!(!is_full_commit_sha("v1.2.3")); // tag
        assert!(!is_full_commit_sha("a1b2c3d")); // short
        assert!(!is_full_commit_sha(
            "z1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0"
        )); // non-hex
    }

    #[test]
    fn pinned_rev_mismatch_is_rejected() {
        let want = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0";
        assert!(verify_rev(want, want).is_ok());
        assert!(verify_rev(&want.to_uppercase(), want).is_ok()); // case-insensitive
        let got = "0000000000000000000000000000000000000000";
        assert!(matches!(
            verify_rev(got, want).unwrap_err(),
            InstallError::SourceRevMismatch { .. }
        ));
    }
}
