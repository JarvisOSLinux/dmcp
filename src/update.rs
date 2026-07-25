//! Detect and apply updates when an installed server's registry manifest drifts.
//!
//! An installed manifest records the `integrity.manifestSha256` the registry
//! bound it to at install time (merged in from the registry entry). When the
//! registry later republishes that server's manifest — even keeping the same
//! `version`, as with a transport bug fix — the recorded hash and the current
//! registry hash diverge. That divergence, not a version bump, is the drift
//! signal: version comparison misses same-version fixes and security fixes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::discovery::{self, Scope};
use crate::install::{self, cli_trust_gate, fetch_server_from_registry, InstallError, TrustGate};
use crate::paths::Paths;
use crate::platform;
use crate::sources::list_sources;

/// Machine-readable drift record for one installed server. Field names are the
/// stable contract the JARVIS daemon polls via `dmcp update --check --json`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DriftReport {
    pub id: String,
    pub installed_hash: Option<String>,
    pub registry_hash: Option<String>,
    pub trust_status: String,
    pub update_available: bool,
    /// Platforms the registry vouches for; absent means unrestricted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
    /// True when `platforms` excludes this host. Orthogonal to
    /// `update_available`: the drift is real either way, but applying it needs
    /// `--ignore-platform`, and the daemon's drift view has to show that.
    pub unsupported_on_host: bool,
}

/// The subset of a registry entry needed to assess drift for one server.
#[derive(Debug, Clone)]
pub struct RegistryEntryInfo {
    pub manifest_sha256: Option<String>,
    pub trust_status: String,
    pub platforms: Option<Vec<String>>,
}

/// An installed server assessed against the live registry.
#[derive(Debug, Clone)]
pub struct AssessedServer {
    pub report: DriftReport,
    /// Registry revoked this server (`trustStatus == "removed"`): never refresh.
    pub revoked: bool,
    /// The registry still lists this id.
    pub in_registry: bool,
    /// Scope the server is installed at (kept for the refresh and elevation).
    pub scope: Scope,
}

/// Drift predicate shared by detection and the browse surface: the registry
/// binds a manifest hash and it differs from what was installed. A registry
/// entry without a recorded hash cannot assert drift.
pub fn is_drifted(installed_hash: Option<&str>, registry_hash: Option<&str>) -> bool {
    match registry_hash {
        Some(reg) => installed_hash != Some(reg),
        None => false,
    }
}

/// Read `integrity.manifestSha256` from a raw manifest.json on disk. The typed
/// `Manifest` drops unknown fields, so this parses the raw JSON instead.
pub fn read_installed_hash(manifest_path: &Path) -> Option<String> {
    let bytes = std::fs::read(manifest_path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    installed_manifest_hash(&value)
}

/// Extract `integrity.manifestSha256` from a parsed manifest value.
pub fn installed_manifest_hash(manifest: &serde_json::Value) -> Option<String> {
    manifest
        .get("integrity")
        .and_then(|i| i.get("manifestSha256"))
        .and_then(|h| h.as_str())
        .filter(|h| !h.is_empty())
        .map(|h| h.to_string())
}

/// Compare one installed server's recorded hash to its live registry entry.
pub fn assess(
    id: &str,
    installed_hash: Option<String>,
    entry: Option<&RegistryEntryInfo>,
    scope: Scope,
) -> AssessedServer {
    match entry {
        None => AssessedServer {
            report: DriftReport {
                id: id.to_string(),
                installed_hash,
                registry_hash: None,
                trust_status: "unknown".to_string(),
                update_available: false,
                platforms: None,
                unsupported_on_host: false,
            },
            revoked: false,
            in_registry: false,
            scope,
        },
        Some(e) => {
            let revoked = e.trust_status == "removed";
            // A revoked server is never "update available": it is uninstalled,
            // not refreshed. Drift on a revoked entry is still reported via the
            // trust_status field so the daemon sees the revocation.
            let update_available =
                !revoked && is_drifted(installed_hash.as_deref(), e.manifest_sha256.as_deref());
            let unsupported_on_host =
                !platform::supports_host(e.platforms.as_deref(), platform::host_platform());
            AssessedServer {
                report: DriftReport {
                    id: id.to_string(),
                    installed_hash,
                    registry_hash: e.manifest_sha256.clone(),
                    trust_status: e.trust_status.clone(),
                    update_available,
                    platforms: e.platforms.clone(),
                    unsupported_on_host,
                },
                revoked,
                in_registry: true,
                scope,
            }
        }
    }
}

/// Parse a registry.json value into id → drift-relevant facts. Accepts either
/// the array or object `servers` shape, mirroring the install/browse readers.
pub fn parse_registry_entries(registry: &serde_json::Value) -> HashMap<String, RegistryEntryInfo> {
    let mut map = HashMap::new();
    let servers_val = match registry.get("servers") {
        Some(v) => v,
        None => return map,
    };
    let entries: Vec<serde_json::Value> = if let Some(arr) = servers_val.as_array() {
        arr.clone()
    } else if let Some(obj) = servers_val.as_object() {
        obj.values().cloned().collect()
    } else {
        return map;
    };
    for entry in entries {
        let id = match entry.get("id").and_then(|i| i.as_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };
        let manifest_sha256 = entry
            .get("integrity")
            .and_then(|i| i.get("manifestSha256"))
            .and_then(|h| h.as_str())
            .filter(|h| !h.is_empty())
            .map(|h| h.to_string());
        let trust_status = install::trust_status(&entry).to_string();
        let info = RegistryEntryInfo {
            manifest_sha256,
            trust_status,
            platforms: platform::declared_platforms(&entry),
        };
        // First source wins; list_sources yields user scope before system, so a
        // user-configured source shadows a system one for the same id.
        map.entry(id).or_insert(info);
    }
    map
}

/// Fetch every configured registry and index its entries by id. Sources may be
/// `http(s)://` URLs or local (`file://` or bare-path) fixtures, so drift checks
/// can run offline against a local registry.
pub fn fetch_registry_entries(
    paths: &Paths,
) -> Result<HashMap<String, RegistryEntryInfo>, UpdateError> {
    let sources = list_sources(paths, true, true);
    if sources.is_empty() {
        return Err(UpdateError::NoSources);
    }
    let client = build_http_client().map_err(UpdateError::HttpClient)?;

    let mut map: HashMap<String, RegistryEntryInfo> = HashMap::new();
    let mut last_error: Option<UpdateError> = None;
    let mut any_ok = false;

    for (url, _scope) in sources {
        match fetch_registry_value(&client, &url) {
            Ok(registry) => {
                any_ok = true;
                for (id, info) in parse_registry_entries(&registry) {
                    map.entry(id).or_insert(info);
                }
            }
            Err(e) => last_error = Some(e),
        }
    }

    // Surface an error only when no source was readable at all; a single bad
    // source must not blind the check to the registries that did respond.
    if !any_ok {
        return Err(last_error.unwrap_or(UpdateError::NoSources));
    }
    Ok(map)
}

/// Assess a set of installed server ids against the live registry (one fetch).
pub fn assess_servers(paths: &Paths, ids: &[String]) -> Result<Vec<AssessedServer>, UpdateError> {
    let registry = fetch_registry_entries(paths)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let manifest_path = discovery::get_manifest_path(paths, id)
            .ok_or_else(|| UpdateError::NotInstalled(id.clone()))?;
        let scope = if manifest_path.starts_with(paths.user_install_dir()) {
            Scope::User
        } else {
            Scope::System
        };
        let installed_hash = read_installed_hash(&manifest_path);
        out.push(assess(id, installed_hash, registry.get(id), scope));
    }
    Ok(out)
}

/// Re-run the install flow for a drifted server: overwrite in place, re-verify
/// the manifest hash, re-clone at the pinned rev, re-run setup — all handled by
/// the existing install path. Trust gating is the caller's responsibility (it
/// mirrors the install command's gate and warnings); the platform gate is not,
/// so a refresh cannot bypass it by going through this path.
pub fn refresh_install(
    paths: &Paths,
    id: &str,
    scope: Scope,
    ignore_platform: bool,
) -> Result<(), UpdateError> {
    let server = fetch_server_from_registry(paths, id).map_err(UpdateError::Install)?;
    install::install(paths, id, scope, Some(server), true, ignore_platform)
        .map_err(UpdateError::Install)?;
    Ok(())
}

fn build_http_client() -> Result<reqwest::blocking::Client, reqwest::Error> {
    reqwest::blocking::Client::builder()
        .user_agent("dmcp/1.0")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()
}

fn fetch_registry_value(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<serde_json::Value, UpdateError> {
    if let Some(path) = local_path_from_url(url) {
        let bytes =
            std::fs::read(&path).map_err(|e| UpdateError::RegistryRead(url.to_string(), e))?;
        return serde_json::from_slice(&bytes)
            .map_err(|_| UpdateError::RegistryParse(url.to_string()));
    }
    let resp = client.get(url).send().map_err(UpdateError::Fetch)?;
    if !resp.status().is_success() {
        return Err(UpdateError::RegistryStatus(
            url.to_string(),
            resp.status().as_u16(),
        ));
    }
    resp.json().map_err(UpdateError::Fetch)
}

/// Map a source URL to a local path for offline reads: a `file://` URL or a
/// bare filesystem path. Returns `None` for `http(s)://` URLs (fetched over the
/// network).
fn local_path_from_url(url: &str) -> Option<PathBuf> {
    if let Some(rest) = url.strip_prefix("file://") {
        // file:///abs → "/abs" (empty host). file://host/abs → strip the host.
        let path = if rest.starts_with('/') {
            rest.to_string()
        } else {
            let i = rest.find('/')?;
            rest[i..].to_string()
        };
        return Some(PathBuf::from(path));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return None;
    }
    Some(PathBuf::from(url))
}

/// Apply the trust gate for an update, mirroring the install command's CLI gate.
/// `Allow`/`Warn` proceed (the warning is returned for the caller to print);
/// `Deny` blocks the refresh.
pub fn trust_gate_for_update(status: &str) -> Result<Option<String>, UpdateError> {
    match cli_trust_gate(status) {
        TrustGate::Allow => Ok(None),
        TrustGate::Warn(msg) => Ok(Some(msg)),
        TrustGate::Deny(reason) => Err(UpdateError::TrustDenied(reason)),
    }
}

#[derive(Debug)]
pub enum UpdateError {
    NoSources,
    NotInstalled(String),
    HttpClient(reqwest::Error),
    Fetch(reqwest::Error),
    RegistryRead(String, std::io::Error),
    RegistryParse(String),
    RegistryStatus(String, u16),
    Install(InstallError),
    TrustDenied(String),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::NoSources => write!(f, "No registry sources configured"),
            UpdateError::NotInstalled(id) => write!(f, "Server not installed: {}", id),
            UpdateError::HttpClient(e) => write!(f, "HTTP client error: {}", e),
            UpdateError::Fetch(e) => write!(f, "Failed to fetch registry: {}", e),
            UpdateError::RegistryRead(url, e) => {
                write!(f, "Failed to read registry {}: {}", url, e)
            }
            UpdateError::RegistryParse(url) => write!(f, "Invalid registry JSON: {}", url),
            UpdateError::RegistryStatus(url, code) => {
                write!(f, "Registry {} returned HTTP {}", url, code)
            }
            UpdateError::Install(e) => write!(f, "{}", e),
            UpdateError::TrustDenied(reason) => write!(f, "Refused by trust policy: {}", reason),
        }
    }
}

impl std::error::Error for UpdateError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    // ---- pure logic -------------------------------------------------------

    #[test]
    fn drift_needs_a_registry_hash() {
        assert!(!is_drifted(Some("aaa"), None));
        assert!(!is_drifted(None, None));
    }

    #[test]
    fn drift_flagged_when_hashes_differ() {
        assert!(is_drifted(Some("aaa"), Some("bbb")));
        // Same version, different manifest bytes -> different recorded hash.
        assert!(is_drifted(Some("old"), Some("new")));
        assert!(is_drifted(None, Some("bbb")));
    }

    #[test]
    fn no_drift_when_hashes_match() {
        assert!(!is_drifted(Some("same"), Some("same")));
    }

    #[test]
    fn installed_hash_reads_integrity_field() {
        let m = serde_json::json!({"integrity": {"manifestSha256": "deadbeef"}});
        assert_eq!(installed_manifest_hash(&m).as_deref(), Some("deadbeef"));
        assert_eq!(installed_manifest_hash(&serde_json::json!({})), None);
        let empty = serde_json::json!({"integrity": {"manifestSha256": ""}});
        assert_eq!(installed_manifest_hash(&empty), None);
    }

    #[test]
    fn assess_detects_drift_on_trusted_server() {
        let entry = RegistryEntryInfo {
            manifest_sha256: Some("new".into()),
            trust_status: "community".into(),
            platforms: None,
        };
        let a = assess("s", Some("old".into()), Some(&entry), Scope::User);
        assert!(a.in_registry);
        assert!(!a.revoked);
        assert!(a.report.update_available);
        assert_eq!(a.report.registry_hash.as_deref(), Some("new"));
        assert_eq!(a.report.installed_hash.as_deref(), Some("old"));
    }

    #[test]
    fn assess_no_drift_is_not_updatable() {
        let entry = RegistryEntryInfo {
            manifest_sha256: Some("same".into()),
            trust_status: "official".into(),
            platforms: None,
        };
        let a = assess("s", Some("same".into()), Some(&entry), Scope::User);
        assert!(!a.report.update_available);
        assert!(!a.revoked);
    }

    #[test]
    fn assess_removed_is_revoked_never_updatable() {
        let entry = RegistryEntryInfo {
            manifest_sha256: Some("new".into()),
            trust_status: "removed".into(),
            platforms: None,
        };
        let a = assess("s", Some("old".into()), Some(&entry), Scope::User);
        assert!(a.revoked);
        assert!(!a.report.update_available);
        // trust_status still surfaces the revocation to a polling daemon.
        assert_eq!(a.report.trust_status, "removed");
    }

    #[test]
    fn assess_deprecated_still_updates() {
        let entry = RegistryEntryInfo {
            manifest_sha256: Some("new".into()),
            trust_status: "deprecated".into(),
            platforms: None,
        };
        let a = assess("s", Some("old".into()), Some(&entry), Scope::User);
        assert!(!a.revoked);
        assert!(a.report.update_available);
        // deprecated warns but proceeds.
        assert!(matches!(
            trust_gate_for_update(&a.report.trust_status),
            Ok(Some(_))
        ));
    }

    #[test]
    fn assess_absent_from_registry() {
        let a = assess("s", Some("old".into()), None, Scope::User);
        assert!(!a.in_registry);
        assert!(!a.report.update_available);
    }

    #[test]
    fn assess_marks_a_host_the_registry_does_not_vouch_for() {
        let entry = RegistryEntryInfo {
            manifest_sha256: Some("new".into()),
            trust_status: "official".into(),
            platforms: Some(vec![platform::foreign_platform().to_string()]),
        };
        let a = assess("s", Some("old".into()), Some(&entry), Scope::User);
        assert!(a.report.unsupported_on_host);
        // Drift is a fact about the manifest, not about the host: the row still
        // reports the update, and the apply path is what refuses.
        assert!(a.report.update_available);
        assert_eq!(
            a.report.platforms.as_deref(),
            Some([platform::foreign_platform().to_string()].as_slice())
        );
    }

    #[test]
    fn assess_leaves_supported_and_undeclared_hosts_unmarked() {
        let vouched = RegistryEntryInfo {
            manifest_sha256: Some("new".into()),
            trust_status: "official".into(),
            platforms: Some(vec![platform::host_platform().to_string()]),
        };
        assert!(
            !assess("s", Some("old".into()), Some(&vouched), Scope::User)
                .report
                .unsupported_on_host
        );

        let undeclared = RegistryEntryInfo {
            manifest_sha256: Some("new".into()),
            trust_status: "official".into(),
            platforms: None,
        };
        let a = assess("s", Some("old".into()), Some(&undeclared), Scope::User);
        assert!(!a.report.unsupported_on_host);
        assert_eq!(a.report.platforms, None);
    }

    #[test]
    fn check_json_rows_carry_platform_state() {
        let entry = RegistryEntryInfo {
            manifest_sha256: Some("new".into()),
            trust_status: "official".into(),
            platforms: Some(vec![platform::foreign_platform().to_string()]),
        };
        let row =
            serde_json::to_value(assess("s", Some("old".into()), Some(&entry), Scope::User).report)
                .unwrap();
        assert_eq!(row["unsupported_on_host"], serde_json::json!(true));
        assert_eq!(
            row["platforms"],
            serde_json::json!([platform::foreign_platform()])
        );

        // An entry that declares nothing keeps the row's shape stable for the
        // daemon: the verdict is always there, the list only when declared.
        let plain = RegistryEntryInfo {
            manifest_sha256: Some("new".into()),
            trust_status: "official".into(),
            platforms: None,
        };
        let row =
            serde_json::to_value(assess("s", Some("old".into()), Some(&plain), Scope::User).report)
                .unwrap();
        assert_eq!(row["unsupported_on_host"], serde_json::json!(false));
        assert!(row.get("platforms").is_none());
    }

    #[test]
    fn trust_gate_blocks_removed_and_allows_official() {
        assert!(trust_gate_for_update("removed").is_err());
        assert!(matches!(trust_gate_for_update("official"), Ok(None)));
        assert!(matches!(trust_gate_for_update("community"), Ok(Some(_))));
    }

    #[test]
    fn file_url_maps_to_local_path() {
        assert_eq!(
            local_path_from_url("file:///reg/registry.json"),
            Some(PathBuf::from("/reg/registry.json"))
        );
        assert_eq!(
            local_path_from_url("file://localhost/reg/registry.json"),
            Some(PathBuf::from("/reg/registry.json"))
        );
        assert_eq!(
            local_path_from_url("/reg/registry.json"),
            Some(PathBuf::from("/reg/registry.json"))
        );
        assert_eq!(
            local_path_from_url("https://example.com/registry.json"),
            None
        );
        assert_eq!(
            local_path_from_url("http://example.com/registry.json"),
            None
        );
    }

    #[test]
    fn parse_registry_entries_reads_object_and_array() {
        let obj = serde_json::json!({
            "servers": {
                "a": {"id": "a", "integrity": {"manifestSha256": "h1"}, "trustStatus": "official"},
                "b": {"id": "b", "trustStatus": "removed"}
            }
        });
        let m = parse_registry_entries(&obj);
        assert_eq!(m["a"].manifest_sha256.as_deref(), Some("h1"));
        assert_eq!(m["a"].trust_status, "official");
        assert_eq!(m["b"].manifest_sha256, None);
        assert_eq!(m["b"].trust_status, "removed");

        let arr = serde_json::json!({
            "servers": [ {"id": "a", "integrity": {"manifestSha256": "h1"}} ]
        });
        let m2 = parse_registry_entries(&arr);
        assert_eq!(m2["a"].manifest_sha256.as_deref(), Some("h1"));
        // Absent trustStatus defaults to the cautious tier.
        assert_eq!(m2["a"].trust_status, "community");
        assert_eq!(m2["a"].platforms, None);
    }

    #[test]
    fn parse_registry_entries_reads_platforms() {
        let reg = serde_json::json!({
            "servers": {
                "a": {"id": "a", "platforms": ["linux", "darwin"]},
                "b": {"id": "b"}
            }
        });
        let m = parse_registry_entries(&reg);
        assert_eq!(
            m["a"].platforms.as_deref(),
            Some(["linux".to_string(), "darwin".to_string()].as_slice())
        );
        assert_eq!(m["b"].platforms, None);
    }

    // ---- offline fixtures (temp dirs mirror the MCP_USER_* env overrides) --

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A temp directory removed on drop; isolates each test's install tree and
    /// sources.list without touching the real XDG paths or the network.
    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let root =
                std::env::temp_dir().join(format!("dmcp-update-test-{}-{}", std::process::id(), n));
            std::fs::create_dir_all(&root).unwrap();
            TempTree { root }
        }

        /// Build a `Paths` rooted in this temp tree. The fields are exactly the
        /// targets of MCP_USER_SOURCES_PATH / MCP_USER_INSTALL_DIR /
        /// MCP_SYSTEM_* / MCP_VECTOR_INDEX_DIR, kept off the real XDG locations.
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

    /// Install a fake user-scope server: index.json entry + manifest.json whose
    /// recorded `integrity.manifestSha256` is `installed_hash`.
    fn install_fake_server(paths: &Paths, id: &str, installed_hash: &str) {
        let dir = paths.user_install_dir().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest_path = dir.join("manifest.json");
        let manifest = serde_json::json!({
            "id": id,
            "name": id,
            "version": "1.0.0",
            "transports": [{"type": "stdio", "command": "echo"}],
            "integrity": {"manifestSha256": installed_hash},
            "installDir": dir.to_string_lossy(),
        });
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let index_path = paths.user_install_dir().join("index.json");
        let mut index: serde_json::Value = std::fs::read_to_string(&index_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({"servers": {}, "version": "1.0"}));
        index["servers"][id] = serde_json::json!({
            "location": manifest_path.to_string_lossy(),
            "keywords": [],
        });
        std::fs::write(&index_path, serde_json::to_string_pretty(&index).unwrap()).unwrap();
    }

    /// Write a registry.json fixture and point the user sources.list at it via a
    /// `file://` URL, so all fetches stay on disk.
    fn write_registry(paths: &Paths, registry: &serde_json::Value) {
        let reg_path = paths.user_install_dir().join("registry-fixture.json");
        std::fs::create_dir_all(reg_path.parent().unwrap()).unwrap();
        std::fs::write(&reg_path, serde_json::to_string_pretty(registry).unwrap()).unwrap();

        let sources = paths.user_sources_path();
        std::fs::create_dir_all(sources.parent().unwrap()).unwrap();
        std::fs::write(sources, format!("file://{}\n", reg_path.display())).unwrap();
    }

    #[test]
    fn drift_detected_against_local_registry_fixture() {
        let tree = TempTree::new();
        let paths = tree.paths();
        install_fake_server(&paths, "com.example.fetchguard", "OLD_HASH");
        write_registry(
            &paths,
            &serde_json::json!({
                "servers": {
                    "com.example.fetchguard": {
                        "id": "com.example.fetchguard",
                        "version": "1.4.0",
                        "trustStatus": "official",
                        "integrity": {"manifestSha256": "NEW_HASH"}
                    }
                }
            }),
        );

        let assessed = assess_servers(&paths, &["com.example.fetchguard".to_string()]).unwrap();
        assert_eq!(assessed.len(), 1);
        let a = &assessed[0];
        assert!(
            a.report.update_available,
            "same-version hash change must drift"
        );
        assert_eq!(a.report.installed_hash.as_deref(), Some("OLD_HASH"));
        assert_eq!(a.report.registry_hash.as_deref(), Some("NEW_HASH"));
        assert!(!a.revoked);
    }

    #[test]
    fn no_drift_when_registry_hash_matches() {
        let tree = TempTree::new();
        let paths = tree.paths();
        install_fake_server(&paths, "s", "SAME");
        write_registry(
            &paths,
            &serde_json::json!({
                "servers": {"s": {"id": "s", "trustStatus": "official",
                    "integrity": {"manifestSha256": "SAME"}}}
            }),
        );
        let assessed = assess_servers(&paths, &["s".to_string()]).unwrap();
        assert!(!assessed[0].report.update_available);
    }

    #[test]
    fn removed_server_is_flagged_revoked_and_check_is_read_only() {
        let tree = TempTree::new();
        let paths = tree.paths();
        install_fake_server(&paths, "s", "OLD");
        write_registry(
            &paths,
            &serde_json::json!({
                "servers": {"s": {"id": "s", "trustStatus": "removed",
                    "integrity": {"manifestSha256": "NEW"}}}
            }),
        );

        let manifest_path = paths.user_install_dir().join("s/manifest.json");
        let before = std::fs::read_to_string(&manifest_path).unwrap();

        let assessed = assess_servers(&paths, &["s".to_string()]).unwrap();
        let a = &assessed[0];
        assert!(a.revoked);
        assert!(!a.report.update_available);

        // Assessment (the --check path) must not touch the install tree.
        let after = std::fs::read_to_string(&manifest_path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn drift_check_against_local_registry_reports_platform_state() {
        let tree = TempTree::new();
        let paths = tree.paths();
        install_fake_server(&paths, "s", "OLD");
        write_registry(
            &paths,
            &serde_json::json!({
                "servers": {"s": {"id": "s", "trustStatus": "official",
                    "platforms": [platform::foreign_platform()],
                    "integrity": {"manifestSha256": "NEW"}}}
            }),
        );

        let assessed = assess_servers(&paths, &["s".to_string()]).unwrap();
        let report = &assessed[0].report;
        assert!(report.update_available);
        assert!(
            report.unsupported_on_host,
            "a registry that stopped vouching for this host must show in the drift view"
        );
        assert_eq!(
            report.platforms.as_deref(),
            Some([platform::foreign_platform().to_string()].as_slice())
        );
    }

    #[test]
    fn drift_check_leaves_a_vouched_host_unmarked() {
        let tree = TempTree::new();
        let paths = tree.paths();
        install_fake_server(&paths, "s", "OLD");
        write_registry(
            &paths,
            &serde_json::json!({
                "servers": {"s": {"id": "s", "trustStatus": "official",
                    "platforms": [platform::host_platform()],
                    "integrity": {"manifestSha256": "NEW"}}}
            }),
        );

        let assessed = assess_servers(&paths, &["s".to_string()]).unwrap();
        assert!(!assessed[0].report.unsupported_on_host);
    }

    #[test]
    fn assess_unknown_server_errors() {
        let tree = TempTree::new();
        let paths = tree.paths();
        write_registry(&paths, &serde_json::json!({"servers": {}}));
        let err = assess_servers(&paths, &["ghost".to_string()]).unwrap_err();
        assert!(matches!(err, UpdateError::NotInstalled(_)));
    }
}
