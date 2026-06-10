//! Install and uninstall MCP servers.

use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::discovery;
use crate::elevation::is_elevated;
use crate::paths::Paths;
use crate::setup;
use crate::sources::list_sources;

/// Install a server from registry by id.
/// When server_override is Some, uses it instead of fetching (avoids double fetch when main already fetched for scope resolution).
/// When run_setup is true and the server has a setupScript, runs it after writing the manifest.
pub fn install(
    paths: &Paths,
    id: &str,
    scope: crate::discovery::Scope,
    server_override: Option<serde_json::Value>,
    run_setup: bool,
) -> Result<(), InstallError> {
    let server = match server_override {
        Some(s) => s,
        None => fetch_server_from_registry(paths, id)?,
    };

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
    let transport_type = first_transport
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

    if run_setup {
        // --- setupScript delivery from registry ---
        // The script lives in the registry repo (servers/<id>/setup.sh), not in
        // the cloned server repo. fetch_server_from_registry set setupScriptUrl
        // when integrity.setupScriptSha256 is present; we download, verify, and
        // write setup.sh here so the run_setup block below finds it locally.
        let (script_delivered, deliver_label) = match deliver_setup_script(&server, &install_dir) {
            DeliverStatus::Downloaded => (true, "ok".to_string()),
            s => (false, s.label()),
        };
        if script_delivered {
            manifest["setupScript"] = serde_json::json!("setup.sh");
        }

        // --- run setup script ---
        if let Some(setup_script_ref) = manifest
            .get("setupScript")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
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
                }
            }
            // Install succeeds even if setup fails; user can retry via dmcp setup
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
                let manifest_resp = client
                    .get(&manifest_url)
                    .send()
                    .map_err(InstallError::FetchFailed)?;
                if manifest_resp.status().is_success() {
                    let mut manifest: serde_json::Value =
                        manifest_resp.json().map_err(InstallError::FetchFailed)?;
                    // Merge: registry entry overrides manifest (for scope, keywords, etc.)
                    merge_json(&mut manifest, &server);
                    server = manifest;
                }
            }

            // Derive setup script URL from the manifest URL when the registry
            // supplies an integrity hash. The script lives alongside the
            // manifest in the registry repo: .../servers/<id>/setup.sh
            let has_setup_hash = server
                .get("integrity")
                .and_then(|i| i.get("setupScriptSha256"))
                .and_then(|h| h.as_str())
                .filter(|h| !h.is_empty())
                .is_some();
            if has_setup_hash {
                if let Some(manifest_url) = server.get("manifest").and_then(|m| m.as_str()) {
                    if let Some(base) = manifest_url.strip_suffix("manifest.json") {
                        server["setupScriptUrl"] = serde_json::json!(format!("{}setup.sh", base));
                    }
                }
            }

            return Ok(server);
        }
    }

    Err(InstallError::ServerNotFound)
}

// ---------------------------------------------------------------------------
// Setup script delivery
// ---------------------------------------------------------------------------

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

/// Download the registry-hosted setup script, SHA-256 verify it, and write it
/// to `install_dir/setup.sh`. Returns the delivery outcome so the caller can
/// set `setupScriptStatus` in the manifest accordingly.
fn deliver_setup_script(server: &serde_json::Value, install_dir: &Path) -> DeliverStatus {
    let url = match server.get("setupScriptUrl").and_then(|u| u.as_str()) {
        Some(u) => u.to_string(),
        None => return DeliverStatus::SkippedNoScript,
    };

    let expected_hash = match server
        .get("integrity")
        .and_then(|i| i.get("setupScriptSha256"))
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

    // Verify SHA-256 against the registry-supplied hash
    let actual_hash = format!("{:x}", Sha256::digest(&content));
    if actual_hash != expected_hash {
        return DeliverStatus::SkippedHashMismatch;
    }

    let dest = install_dir.join("setup.sh");
    if let Err(e) = std::fs::write(&dest, &content) {
        return DeliverStatus::SkippedWriteFailed(e.to_string());
    }

    // Mark executable on Unix so `sh setup.sh` isn't required by run_setup
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755));
    }

    DeliverStatus::Downloaded
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

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

    let temp = std::env::temp_dir().join(format!("dmcp-clone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).map_err(InstallError::CreateDir)?;

    let status = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--filter=blob:none",
            url,
            temp.to_str().unwrap(),
        ])
        .status()
        .map_err(InstallError::GitFailed)?;
    if !status.success() {
        return Err(InstallError::GitFailed(std::io::Error::new(
            std::io::ErrorKind::Other,
            "git clone failed",
        )));
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

    if scope == crate::discovery::Scope::System && is_elevated() {
        std::fs::write(&index_path, output).map_err(InstallError::WriteIndex)?;
    } else if scope == crate::discovery::Scope::System {
        let temp = std::env::temp_dir().join(format!("dmcp-index-{}.json", std::process::id()));
        std::fs::write(&temp, &output).map_err(InstallError::WriteIndex)?;
        let status = Command::new("pkexec")
            .arg("cp")
            .arg(&temp)
            .arg(&index_path)
            .status()
            .map_err(InstallError::WriteIndex)?;
        let _ = std::fs::remove_file(&temp);
        if !status.success() {
            return Err(InstallError::WriteIndex(std::io::Error::new(
                std::io::ErrorKind::Other,
                "pkexec cp failed",
            )));
        }
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
        }
    }
}

impl std::error::Error for InstallError {}

/// Uninstall a server by id. Removes install dir and updates index.
pub fn uninstall(paths: &Paths, id: &str) -> Result<(), UninstallError> {
    let (manifest_path, install_dir, scope) =
        discovery::get_uninstall_info(paths, id).ok_or(UninstallError::ServerNotFound)?;

    // Remove install directory
    if scope == crate::discovery::Scope::System && is_elevated() {
        std::fs::remove_dir_all(&install_dir).map_err(UninstallError::RmFailed)?;
    } else if scope == crate::discovery::Scope::System {
        let status = Command::new("pkexec")
            .arg("rm")
            .arg("-rf")
            .arg(&install_dir)
            .status()
            .map_err(UninstallError::RmFailed)?;
        if !status.success() {
            return Err(UninstallError::RmFailed(std::io::Error::new(
                std::io::ErrorKind::Other,
                "pkexec rm -rf failed",
            )));
        }
    } else {
        std::fs::remove_dir_all(&install_dir).map_err(UninstallError::RmFailed)?;
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

    if scope == crate::discovery::Scope::System && is_elevated() {
        std::fs::write(index_path, output).map_err(UninstallError::WriteIndex)?;
    } else if scope == crate::discovery::Scope::System {
        let temp = std::env::temp_dir().join(format!("dmcp-index-{}.json", std::process::id()));
        std::fs::write(&temp, &output).map_err(UninstallError::WriteIndex)?;
        let status = Command::new("pkexec")
            .arg("cp")
            .arg(&temp)
            .arg(index_path)
            .status()
            .map_err(UninstallError::WriteIndex)?;
        let _ = std::fs::remove_file(&temp);
        if !status.success() {
            return Err(UninstallError::WriteIndex(std::io::Error::new(
                std::io::ErrorKind::Other,
                "pkexec cp failed",
            )));
        }
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
    let days = (secs / 86400) as i64;
    let time = (secs % 86400 + 86400) % 86400;
    let hour = (time / 3600) as u32;
    let min = ((time % 3600) / 60) as u32;
    let sec = (time % 60) as u32;
    let (y, m, d) = days_to_ymd(days);
    (y, m, d, hour, min, sec)
}

fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let days = days + 719468;
    let era = days / 146097;
    let day_of_era = (days - era * 146097) as i64;
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
