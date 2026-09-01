//! Connect to remote (SSE/WebSocket) MCP servers by URL, without a registry.
//!
//! Tries to fetch manifest from URL first; if valid JSON with id+transports, uses it.
//! Otherwise falls back to treating URL as raw endpoint.

use std::time::Duration;

use crate::manifest_io::{create_install_dir, write_manifest_atomic, Readers};
use crate::paths::Paths;
use crate::setup;

/// Connect to a remote MCP server. Tries to fetch manifest from URL; falls back to raw endpoint.
///
/// `ignore_platform` is the `--ignore-platform` override. The gate lives here
/// rather than in the caller so `dmcp install <url>` and `dmcp connect <url>`
/// cannot diverge, and so the manifest is fetched once: checking it in the
/// caller would mean a second fetch and a window between the two.
#[allow(clippy::too_many_arguments)]
pub fn connect(
    paths: &Paths,
    url: &str,
    id_override: Option<&str>,
    name: Option<&str>,
    summary: Option<&str>,
    version: Option<&str>,
    config: &[(String, String)],
    scope: crate::discovery::Scope,
    run_setup: bool,
    ignore_platform: bool,
) -> Result<String, ConnectError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(ConnectError::InvalidUrl);
    }

    if let Some(manifest) = try_fetch_manifest(url) {
        return connect_manifest(
            paths,
            manifest,
            id_override,
            name,
            summary,
            version,
            config,
            scope,
            run_setup,
            ignore_platform,
        );
    }

    // Raw fallback: treat URL as endpoint
    connect_raw(
        paths,
        url,
        id_override,
        name,
        summary,
        version,
        config,
        scope,
    )
}

/// Install a server from an already-fetched manifest. Split from the fetch so
/// the gate below sits on the one path every manifest takes, and so it can be
/// exercised without a network.
#[allow(clippy::too_many_arguments)]
fn connect_manifest(
    paths: &Paths,
    mut manifest: serde_json::Value,
    id_override: Option<&str>,
    name: Option<&str>,
    summary: Option<&str>,
    version: Option<&str>,
    config: &[(String, String)],
    scope: crate::discovery::Scope,
    run_setup: bool,
    ignore_platform: bool,
) -> Result<String, ConnectError> {
    // Ahead of every side effect — the install dir, the manifest write and the
    // setup script all leave state or run code on a host the manifest says it
    // was not built for.
    if !ignore_platform {
        if let Some(refusal) = crate::platform::check_host(&manifest) {
            return Err(ConnectError::PlatformUnsupported(refusal));
        }
    }

    let id = id_override
        .map(String::from)
        .or_else(|| {
            manifest
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| {
            next_connected_server_id(paths, scope)
                .unwrap_or_else(|_| "com.user.connected.server1".to_string())
        });

    let install_dir = match scope {
        crate::discovery::Scope::User => paths.user_install_dir().join(&id),
        crate::discovery::Scope::System => paths.system_install_dir().join(&id),
    };
    let readers = Readers::for_scope(scope);

    create_install_dir(&install_dir, readers).map_err(ConnectError::CreateDir)?;

    manifest["installDir"] = serde_json::Value::String(install_dir.to_string_lossy().to_string());
    manifest["id"] = serde_json::Value::String(id.clone());
    // A connected server has no registry entry, so no tier was ever reviewed —
    // and the fetched manifest is attacker-controlled, so a trustStatus it
    // carries must not survive to disk (a URL manifest could otherwise
    // self-declare "official" and be believed by everything that reads the
    // installed manifest). "unknown" is the established no-registry spelling.
    manifest["trustStatus"] = serde_json::Value::String("unknown".to_string());

    if let Some(n) = name {
        manifest["name"] = serde_json::Value::String(n.to_string());
    } else if manifest.get("name").is_none() {
        manifest["name"] = serde_json::Value::String(id.clone());
    }

    if let Some(s) = summary {
        manifest["summary"] = serde_json::Value::String(s.to_string());
    } else if manifest.get("summary").is_none() {
        manifest["summary"] = serde_json::Value::String("Connected via dmcp connect".to_string());
    }

    if let Some(v) = version {
        manifest["version"] = serde_json::Value::String(v.to_string());
    } else if manifest.get("version").is_none() {
        manifest["version"] = serde_json::Value::String("1.0.0".to_string());
    }

    // Merge config overrides
    let mut config_obj = manifest
        .get("config")
        .and_then(|c| c.as_object().cloned())
        .unwrap_or_default();
    for (k, v) in config {
        config_obj.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    manifest["config"] = serde_json::Value::Object(config_obj);

    let manifest_path = install_dir.join("manifest.json");
    let output = serde_json::to_string_pretty(&manifest).map_err(ConnectError::Serialize)?;
    write_manifest_atomic(&manifest_path, output.as_bytes(), readers)
        .map_err(ConnectError::WriteManifest)?;

    // Run setup script if present — the one this host runs, chosen by the same
    // selector `install` and `dmcp setup` use, so a manifest that ships only
    // `setupScriptWindows` is not silently skipped on Windows.
    if run_setup {
        if let Some(setup_script) = crate::setup::script_for_host(
            crate::platform::host_platform(),
            manifest.get("setupScriptWindows").and_then(|v| v.as_str()),
            manifest.get("setupScript").and_then(|v| v.as_str()),
        ) {
            // Owned, to end the borrow on `manifest` before the config read.
            let setup_script = setup_script.to_string();
            let config_map = manifest
                .get("config")
                .and_then(|c| c.as_object())
                .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            if let Err(e) = setup::run_setup(&setup_script, &install_dir, &config_map) {
                return Err(ConnectError::SetupFailed(e.to_string()));
            }
        }
    }

    let keywords: Vec<String> = manifest
        .get("keywords")
        .and_then(|k| k.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    crate::install::update_index_add(paths, &id, &manifest_path, scope, &keywords)
        .map_err(|e| ConnectError::IndexError(e.to_string()))?;

    Ok(id)
}

/// Try to fetch URL as JSON manifest. Returns Some if valid (has id and transports).
fn try_fetch_manifest(url: &str) -> Option<serde_json::Value> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("dmcp/1.0")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()
        .ok()?;

    let resp = client.get(url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let manifest: serde_json::Value = resp.json().ok()?;
    let id = manifest.get("id").and_then(|v| v.as_str())?;
    let transports = manifest.get("transports").and_then(|t| t.as_array())?;
    if id.is_empty() || transports.is_empty() {
        return None;
    }

    Some(manifest)
}

/// Raw endpoint mode: infer transport from URL, auto-generate metadata.
#[allow(clippy::too_many_arguments)]
fn connect_raw(
    paths: &Paths,
    url: &str,
    id_override: Option<&str>,
    name: Option<&str>,
    summary: Option<&str>,
    version: Option<&str>,
    config: &[(String, String)],
    scope: crate::discovery::Scope,
) -> Result<String, ConnectError> {
    let transport_type = if url.starts_with("wss://") || url.starts_with("ws://") {
        "websocket"
    } else {
        "sse"
    };

    let id = id_override.map(String::from).unwrap_or_else(|| {
        next_connected_server_id(paths, scope)
            .unwrap_or_else(|_| "com.user.connected.server1".into())
    });

    let install_dir = match scope {
        crate::discovery::Scope::User => paths.user_install_dir().join(&id),
        crate::discovery::Scope::System => paths.system_install_dir().join(&id),
    };
    let readers = Readers::for_scope(scope);

    create_install_dir(&install_dir, readers).map_err(ConnectError::CreateDir)?;

    let transport = if transport_type == "websocket" {
        serde_json::json!({
            "type": "websocket",
            "wsUrl": url
        })
    } else {
        serde_json::json!({
            "type": "sse",
            "url": url
        })
    };

    let mut config_obj = serde_json::Map::new();
    for (k, v) in config {
        config_obj.insert(k.clone(), serde_json::Value::String(v.clone()));
    }

    let manifest = serde_json::json!({
        "id": id,
        "name": name.unwrap_or(&id),
        "summary": summary.unwrap_or("Connected via dmcp connect"),
        "version": version.unwrap_or("1.0.0"),
        "transports": [transport],
        "installDir": install_dir.to_string_lossy(),
        "trustStatus": "unknown",
        "config": config_obj
    });

    let manifest_path = install_dir.join("manifest.json");
    let output = serde_json::to_string_pretty(&manifest).map_err(ConnectError::Serialize)?;
    write_manifest_atomic(&manifest_path, output.as_bytes(), readers)
        .map_err(ConnectError::WriteManifest)?;

    crate::install::update_index_add(paths, &id, &manifest_path, scope, &[])
        .map_err(|e| ConnectError::IndexError(e.to_string()))?;

    Ok(id)
}

fn next_connected_server_id(
    paths: &Paths,
    scope: crate::discovery::Scope,
) -> Result<String, ConnectError> {
    let index_path = match scope {
        crate::discovery::Scope::User => paths.user_install_dir().join("index.json"),
        crate::discovery::Scope::System => paths.system_install_dir().join("index.json"),
    };

    let content = std::fs::read_to_string(&index_path)
        .unwrap_or_else(|_| r#"{"servers":{},"version":"1.0"}"#.to_string());
    let index: serde_json::Value =
        serde_json::from_str(&content).map_err(ConnectError::ParseIndex)?;

    let empty = serde_json::Map::new();
    let servers = index
        .get("servers")
        .and_then(|s| s.as_object())
        .unwrap_or(&empty);

    let mut max_n = 0u32;
    for (id, _) in servers {
        if let Some(n) = id
            .strip_prefix("com.user.connected.server")
            .and_then(|s| s.parse::<u32>().ok())
        {
            if n > max_n {
                max_n = n;
            }
        }
    }

    Ok(format!("com.user.connected.server{}", max_n + 1))
}

#[derive(Debug)]
pub enum ConnectError {
    InvalidUrl,
    CreateDir(std::io::Error),
    Serialize(serde_json::Error),
    WriteManifest(std::io::Error),
    SetupFailed(String),
    ParseIndex(serde_json::Error),
    IndexError(String),
    PlatformUnsupported(crate::platform::UnsupportedHost),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::InvalidUrl => write!(f, "Invalid or empty URL"),
            ConnectError::CreateDir(e) => write!(f, "Failed to create directory: {}", e),
            ConnectError::Serialize(e) => write!(f, "Failed to serialize: {}", e),
            ConnectError::WriteManifest(e) => write!(f, "Failed to write manifest: {}", e),
            ConnectError::SetupFailed(s) => write!(f, "Setup failed: {}", s),
            ConnectError::ParseIndex(e) => write!(f, "Failed to parse index: {}", e),
            ConnectError::IndexError(s) => write!(f, "{}", s),
            ConnectError::PlatformUnsupported(refusal) => write!(f, "{}", refusal),
        }
    }
}

impl std::error::Error for ConnectError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempTree {
        root: std::path::PathBuf,
    }

    impl TempTree {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!(
                "dmcp-connect-test-{}-{}",
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
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn manifest(platforms: Option<&str>, setup_script: Option<(&str, &str)>) -> serde_json::Value {
        let mut m = serde_json::json!({
            "id": "com.test.connected",
            "transports": [{"type": "sse", "url": "https://example.invalid/sse"}],
        });
        if let Some(p) = platforms {
            m["platforms"] = serde_json::json!([p]);
        }
        if let Some((key, value)) = setup_script {
            m[key] = serde_json::json!(value);
        }
        m
    }

    fn connect_with(
        paths: &Paths,
        manifest: serde_json::Value,
        run_setup: bool,
        ignore_platform: bool,
    ) -> Result<String, ConnectError> {
        connect_manifest(
            paths,
            manifest,
            None,
            None,
            None,
            None,
            &[],
            crate::discovery::Scope::User,
            run_setup,
            ignore_platform,
        )
    }

    /// `dmcp install <url>` and `dmcp install <id>` must refuse the same
    /// manifest — and refuse it before anything is written or executed.
    #[test]
    fn unsupported_platform_is_refused_before_any_side_effect() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let sentinel = tree.root.join("setup-ran");
        let script = tree.root.join("touch.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch {}\n", sentinel.display()),
        )
        .unwrap();

        let err = connect_with(
            &paths,
            manifest(
                Some(crate::platform::foreign_platform()),
                Some(("setupScript", script.to_string_lossy().as_ref())),
            ),
            true,
            false,
        )
        .unwrap_err();

        assert!(
            matches!(err, ConnectError::PlatformUnsupported(_)),
            "a manifest declaring other platforms must be refused, got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains(crate::platform::foreign_platform()) && msg.contains("--ignore-platform"),
            "the refusal matches the registry path's: {msg}"
        );
        assert!(
            !sentinel.exists(),
            "setup must not run on a refused connect"
        );
        assert!(
            !paths.user_install_dir().join("com.test.connected").exists(),
            "nothing is written before the refusal"
        );
    }

    /// A `platforms` field that cannot be read is refused here too, so the URL
    /// path cannot be the one place a malformed declaration installs.
    #[test]
    fn a_malformed_platforms_field_is_refused_too() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let mut m = manifest(None, None);
        m["platforms"] = serde_json::json!(crate::platform::host_platform());

        let err = connect_with(&paths, m, false, false).unwrap_err();
        assert!(matches!(err, ConnectError::PlatformUnsupported(_)));
        assert!(!paths.user_install_dir().join("com.test.connected").exists());
    }

    #[test]
    fn ignore_platform_overrides_the_refusal() {
        let tree = TempTree::new();
        let paths = tree.paths();
        std::fs::create_dir_all(paths.user_install_dir()).unwrap();

        let id = connect_with(
            &paths,
            manifest(Some(crate::platform::foreign_platform()), None),
            false,
            true,
        )
        .expect("--ignore-platform must connect on an unvouched host");
        assert_eq!(id, "com.test.connected");
        assert!(paths
            .user_install_dir()
            .join("com.test.connected/manifest.json")
            .exists());
    }

    #[test]
    fn a_manifest_without_platforms_connects_anywhere() {
        let tree = TempTree::new();
        let paths = tree.paths();
        std::fs::create_dir_all(paths.user_install_dir()).unwrap();

        connect_with(&paths, manifest(None, None), false, false)
            .expect("an absent platforms list is unrestricted, exactly as before");
    }

    /// A raw endpoint carries no manifest and therefore declares no platforms,
    /// so it stays unrestricted — the gate has nothing to read there.
    #[test]
    fn a_raw_endpoint_is_never_platform_gated() {
        let tree = TempTree::new();
        let paths = tree.paths();
        std::fs::create_dir_all(paths.user_install_dir()).unwrap();
        let id = connect_raw(
            &paths,
            "wss://example.invalid/ws",
            Some("com.test.raw"),
            None,
            None,
            None,
            &[],
            crate::discovery::Scope::User,
        )
        .expect("a raw endpoint has no platforms to check");
        assert_eq!(id, "com.test.raw");
    }

    /// The connect path picks the setup script for this host through the same
    /// selector as `install`, so a Windows-only script is not silently skipped.
    /// Asserted through the selector rather than the host, so it means the same
    /// thing on a POSIX CI runner.
    #[test]
    fn the_setup_script_is_chosen_by_the_host_not_by_the_posix_field() {
        let windows_only = manifest(None, Some(("setupScriptWindows", "setup.ps1")));
        assert_eq!(
            crate::setup::script_for_host(
                "windows",
                windows_only
                    .get("setupScriptWindows")
                    .and_then(|v| v.as_str()),
                windows_only.get("setupScript").and_then(|v| v.as_str()),
            ),
            Some("setup.ps1"),
            "a Windows host must run the script the manifest ships for it"
        );

        let both = serde_json::json!({
            "setupScript": "setup.sh",
            "setupScriptWindows": "setup.ps1",
        });
        assert_eq!(
            crate::setup::script_for_host(
                "windows",
                both.get("setupScriptWindows").and_then(|v| v.as_str()),
                both.get("setupScript").and_then(|v| v.as_str()),
            ),
            Some("setup.ps1"),
            "never hand setup.sh to PowerShell when a .ps1 was shipped"
        );
    }

    /// A fetched manifest is attacker-controlled: a trustStatus it carries must
    /// not survive to disk, or a URL install could self-declare "official" and
    /// be believed by everything that reads the installed manifest.
    #[test]
    fn a_url_manifests_self_declared_tier_does_not_survive() {
        let tree = TempTree::new();
        let paths = tree.paths();
        let mut m = manifest(None, None);
        m["trustStatus"] = serde_json::json!("official");
        connect_with(&paths, m, false, false).unwrap();
        let written: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                paths
                    .user_install_dir()
                    .join("com.test.connected/manifest.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            written["trustStatus"], "unknown",
            "no registry reviewed a connected server, whatever its manifest claims"
        );
    }

    #[cfg(unix)]
    fn mode_of(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// `dmcp connect --config token=…` writes the same credential store the
    /// install path does, by either route — a fetched manifest or a raw
    /// endpoint — so both must leave it owner-only.
    #[cfg(unix)]
    #[test]
    fn a_connected_servers_credential_store_is_private() {
        let tree = TempTree::new();
        let paths = tree.paths();

        connect_with(&paths, manifest(None, None), false, false).unwrap();
        let from_manifest = paths.user_install_dir().join("com.test.connected");
        assert_eq!(mode_of(&from_manifest.join("manifest.json")), 0o600);
        assert_eq!(mode_of(&from_manifest), 0o700);

        connect_raw(
            &paths,
            "wss://example.invalid/ws",
            Some("com.test.raw"),
            None,
            None,
            None,
            &[("token".to_string(), "ghp_secret".to_string())],
            crate::discovery::Scope::User,
        )
        .unwrap();
        let from_raw = paths.user_install_dir().join("com.test.raw");
        assert_eq!(
            mode_of(&from_raw.join("manifest.json")),
            0o600,
            "a raw connect stores --config values verbatim; they are secrets too"
        );
        assert_eq!(mode_of(&from_raw), 0o700);
    }
}
