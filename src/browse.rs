//! Browse MCP servers from registry sources.

use std::error::Error;
use std::time::Duration;

use crate::paths::Paths;
use crate::sources::list_sources;
use crate::transport;

fn build_http_client() -> Result<reqwest::blocking::Client, reqwest::Error> {
    reqwest::blocking::Client::builder()
        .user_agent("dmcp/1.0")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()
}

/// A server entry from a registry (for display).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RegistryServer {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub version: String,
    pub transport: String,
    pub source: String,
    /// Whether this server is installed (user or system scope).
    pub installed: bool,
    /// Search keywords for discovery.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// For an installed server, whether the local copy has drifted from this
    /// entry's manifest hash. `None` when not installed or the entry records no
    /// integrity hash; populated by the browse command, not the fetch itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_available: Option<bool>,
    /// Registry-recorded manifest hash, used to compute `update_available`.
    /// Internal to the browse command; never serialized.
    #[serde(skip)]
    pub registry_manifest_sha256: Option<String>,
    /// Platforms the registry vouches for. Absent means unrestricted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<String>>,
    /// True when `platforms` excludes this host, so installing needs
    /// `--ignore-platform`. Always serialized: the `--json` browse output is what
    /// reaches the agent, and an agent that cannot see this would keep proposing
    /// servers that cannot run here.
    pub unsupported_on_host: bool,
    /// The entry declares `platforms`, but not as an array of platform names.
    /// Such an entry is unsupported here (it vouches for nothing readable);
    /// this says why, so the operator can see it is a broken entry rather than
    /// a foreign one. Omitted for every well-formed entry.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub platforms_malformed: bool,
}

/// Fetch and list servers from a specific registry URL.
pub fn list_registry_servers_from_url(url: &str) -> Result<Vec<RegistryServer>, BrowseError> {
    let client = build_http_client().map_err(BrowseError::HttpClient)?;
    fetch_registry(&client, url).map_err(|e| BrowseError::FetchFailed {
        url: url.to_string(),
        cause: e,
    })
}

/// Fetch and list all servers from configured registry sources.
/// Returns (servers, errors). Servers may be duplicated across sources.
pub fn list_registry_servers(
    paths: &Paths,
    include_user: bool,
    include_system: bool,
) -> (Vec<RegistryServer>, Vec<BrowseError>) {
    let sources = list_sources(paths, include_user, include_system);
    let mut servers = Vec::new();
    let mut errors = Vec::new();

    let client = match build_http_client() {
        Ok(c) => c,
        Err(e) => {
            errors.push(BrowseError::HttpClient(e));
            return (servers, errors);
        }
    };

    for (url, _scope) in sources {
        match fetch_registry(&client, &url) {
            Ok(registry_servers) => {
                servers.extend(registry_servers);
            }
            Err(e) => {
                errors.push(BrowseError::FetchFailed {
                    url: url.clone(),
                    cause: e,
                });
            }
        }
    }

    (servers, errors)
}

/// Filter servers by keywords. A server matches if any keyword (case-insensitive) appears in
/// id, name, summary, or the server's keywords.
pub fn filter_servers_by_keywords(
    servers: Vec<RegistryServer>,
    keywords: &[String],
) -> Vec<RegistryServer> {
    if keywords.is_empty() {
        return servers;
    }
    let keywords_lower: Vec<String> = keywords
        .iter()
        .map(|k| k.to_lowercase())
        .filter(|k| !k.is_empty())
        .collect();
    if keywords_lower.is_empty() {
        return servers;
    }
    servers
        .into_iter()
        .filter(|s| {
            let searchable = [
                s.id.as_str(),
                s.name.as_str(),
                s.summary.as_str(),
                &s.keywords.join(" "),
            ]
            .join(" ")
            .to_lowercase();
            keywords_lower.iter().any(|kw| searchable.contains(kw))
        })
        .collect()
}

fn fetch_registry(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Result<Vec<RegistryServer>, reqwest::Error> {
    let resp = client.get(url).send()?;
    if !resp.status().is_success() {
        return Err(resp.error_for_status().unwrap_err());
    }
    let registry: serde_json::Value = resp.json()?;
    let servers_val = registry.get("servers");
    let servers_array: Vec<serde_json::Value> = match servers_val {
        Some(s) if s.is_array() => s.as_array().unwrap().clone(),
        Some(s) if s.is_object() => {
            let obj = s.as_object().unwrap();
            obj.values().cloned().collect()
        }
        _ => return Ok(vec![]),
    };

    let mut result = Vec::new();
    for server in servers_array {
        let mut entry = registry_server_from_entry(&server, url);

        if entry.transport == "?" {
            if let Some(manifest_url) = server.get("manifest").and_then(|m| m.as_str()) {
                if let Some(t) = transport::transport_from_manifest_url(client, manifest_url) {
                    entry.transport = t;
                }
            }
        }

        result.push(entry);
    }

    Ok(result)
}

/// Map one registry entry to its display form. Everything readable from the
/// entry itself is resolved here — including the platform state, which the
/// registry mirrors from the manifest so browsing never has to fetch manifests
/// just to know what runs on this host.
fn registry_server_from_entry(server: &serde_json::Value, source_url: &str) -> RegistryServer {
    let str_field = |key: &str, fallback: &str| {
        server
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or(fallback)
            .to_string()
    };

    // The transport this host would launch, so the column matches what an
    // install here actually starts; the first entry still stands in when the
    // list declares nothing for this host.
    let transport = server
        .get("transports")
        .and_then(|t| t.as_array())
        .and_then(|a| crate::transport::select_json(a).ok().or_else(|| a.first()))
        .and_then(|t| t.get("type").and_then(|x| x.as_str()))
        .map(String::from)
        .unwrap_or_else(|| "?".to_string());

    let keywords: Vec<String> = server
        .get("keywords")
        .and_then(|k| k.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let registry_manifest_sha256 = server
        .get("integrity")
        .and_then(|i| i.get("manifestSha256"))
        .and_then(|h| h.as_str())
        .filter(|h| !h.is_empty())
        .map(String::from);

    let decl = crate::platform::platform_decl(server);
    let unsupported_on_host = !decl.supports(crate::platform::host_platform());
    let platforms_malformed = decl.is_malformed();
    let platforms = decl.names().map(<[String]>::to_vec);

    RegistryServer {
        id: str_field("id", "?"),
        name: str_field("name", "?"),
        summary: str_field("summary", ""),
        version: str_field("version", "?"),
        transport,
        source: source_url.to_string(),
        installed: false,
        keywords,
        update_available: None,
        registry_manifest_sha256,
        platforms,
        unsupported_on_host,
        platforms_malformed,
    }
}

#[derive(Debug)]
pub enum BrowseError {
    HttpClient(reqwest::Error),
    FetchFailed { url: String, cause: reqwest::Error },
}

impl std::fmt::Display for BrowseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrowseError::HttpClient(e) => {
                write!(f, "HTTP client error: {}", e)?;
                if let Some(s) = e.source() {
                    write!(f, "\n  Caused by: {}", s)?;
                }
                Ok(())
            }
            BrowseError::FetchFailed { url, cause } => {
                write!(f, "Failed to fetch {}: {}", url, cause)?;
                // Show error chain for more diagnostic detail
                let mut source: Option<&(dyn Error + '_)> = cause.source();
                while let Some(s) = source {
                    write!(f, "\n  Caused by: {}", s)?;
                    source = s.source();
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for BrowseError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{foreign_platform, host_platform};

    const SOURCE: &str = "file:///reg/registry.json";

    fn entry(platforms: Option<serde_json::Value>) -> serde_json::Value {
        let mut e = serde_json::json!({
            "id": "com.example.mcp.thing",
            "name": "Thing",
            "summary": "Does a thing",
            "version": "1.0.0",
            "transports": [{"type": "stdio", "command": "thing"}],
        });
        if let Some(p) = platforms {
            e["platforms"] = p;
        }
        e
    }

    #[test]
    fn entry_unsupported_on_this_host_is_marked() {
        let s = registry_server_from_entry(
            &entry(Some(serde_json::json!([foreign_platform()]))),
            SOURCE,
        );
        assert!(s.unsupported_on_host);
        assert_eq!(
            s.platforms.as_deref(),
            Some([foreign_platform().to_string()].as_slice())
        );
        // The rest of the mapping is untouched by the platform read.
        assert_eq!(s.id, "com.example.mcp.thing");
        assert_eq!(s.transport, "stdio");
        assert_eq!(s.source, SOURCE);
    }

    #[test]
    fn entry_vouching_for_this_host_is_not_marked() {
        let s =
            registry_server_from_entry(&entry(Some(serde_json::json!([host_platform()]))), SOURCE);
        assert!(!s.unsupported_on_host);
        assert_eq!(
            s.platforms.as_deref(),
            Some([host_platform().to_string()].as_slice())
        );
    }

    #[test]
    fn entry_without_platforms_is_unrestricted() {
        let s = registry_server_from_entry(&entry(None), SOURCE);
        assert!(!s.unsupported_on_host);
        assert_eq!(s.platforms, None);
    }

    /// The `--json` payload is what JARVIS's search_servers hands the agent. An
    /// entry whose `platforms` cannot be read must never be reported as running
    /// here — the agent has no way to check for itself.
    #[test]
    fn a_malformed_platforms_entry_is_reported_as_unsupported() {
        for value in [
            serde_json::json!(host_platform()),
            serde_json::json!({"linux": true}),
            serde_json::json!([123]),
        ] {
            let s = registry_server_from_entry(&entry(Some(value.clone())), SOURCE);
            assert!(s.unsupported_on_host, "{value} must be marked unsupported");
            assert!(s.platforms_malformed, "{value} must be flagged malformed");
            assert_eq!(s.platforms, None, "there is no readable list to report");

            let json = serde_json::to_value(&s).unwrap();
            assert_eq!(json["unsupported_on_host"], serde_json::json!(true));
            assert_eq!(json["platforms_malformed"], serde_json::json!(true));
        }
    }

    #[test]
    fn json_output_always_carries_the_host_verdict() {
        // This is the payload JARVIS's search_servers hands the agent: the flag
        // must be present whether or not the entry declares platforms.
        let marked = serde_json::to_value(registry_server_from_entry(
            &entry(Some(serde_json::json!([foreign_platform()]))),
            SOURCE,
        ))
        .unwrap();
        assert_eq!(marked["unsupported_on_host"], serde_json::json!(true));
        assert_eq!(marked["platforms"], serde_json::json!([foreign_platform()]));

        let plain = serde_json::to_value(registry_server_from_entry(&entry(None), SOURCE)).unwrap();
        assert_eq!(plain["unsupported_on_host"], serde_json::json!(false));
        assert!(
            plain.get("platforms").is_none(),
            "an undeclared list stays absent rather than becoming an empty one"
        );
        assert!(
            plain.get("platforms_malformed").is_none(),
            "the flag is noise on every well-formed entry"
        );
    }

    /// A server declared only for other platforms still reports the transport
    /// it would launch there, the same way `dmcp list` does for the installed
    /// copy — one manifest, one answer, whichever surface is asked.
    #[test]
    fn a_foreign_only_entry_still_reports_its_transport() {
        let mut e = entry(Some(serde_json::json!([foreign_platform()])));
        e["transports"] = serde_json::json!([
            {"type": "sse", "url": "https://example.invalid", "platforms": [foreign_platform()]}
        ]);
        let s = registry_server_from_entry(&e, SOURCE);
        assert_eq!(s.transport, "sse");
        assert!(s.unsupported_on_host);
    }
}
