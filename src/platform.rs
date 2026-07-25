//! Host platform identity and the registry `platforms` gate.
//!
//! A registry entry's `platforms` list is the set of platforms the registry has
//! actually vetted the server on — coverage state, not a submitter aspiration.
//! dmcp refuses to install or refresh a server whose list excludes the host, so
//! the agent above it never has to reason about platforms at all.
//!
//! The vocabulary is `"linux"`, `"darwin"`, `"windows"`. `std::env::consts::OS`
//! spells macOS `"macos"`, so it has to be mapped; every consumer (the install
//! gate, the browse marking, transport selection) goes through the one helper
//! here so those readings cannot drift apart.

/// This host's name in the registry vocabulary.
pub fn host_platform() -> &'static str {
    normalize_os(std::env::consts::OS)
}

/// Map a `std::env::consts::OS` value into the registry vocabulary. Names other
/// than the three known ones pass through unchanged and therefore match no
/// declared list: a host nobody vetted is unsupported, not implicitly fine.
pub fn normalize_os(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        other => other,
    }
}

/// The `platforms` declared by a registry entry or a manifest, if any.
///
/// An empty array reads as absent. The field's absence already means
/// unrestricted, and a list that vouches for nothing is far more likely to be a
/// serialization slip than a deliberate "installable nowhere" — the strict
/// reading would brick a server on every host at once.
pub fn declared_platforms(entry: &serde_json::Value) -> Option<Vec<String>> {
    let list: Vec<String> = entry
        .get("platforms")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (!list.is_empty()).then_some(list)
}

/// Whether a declared platform list covers `host`. `None` is unrestricted, which
/// is what keeps every pre-`platforms` manifest and every third-party registry
/// behaving exactly as before.
pub fn supports_host(platforms: Option<&[String]>, host: &str) -> bool {
    match platforms {
        None => true,
        Some(list) => list.iter().any(|p| p.trim().eq_ignore_ascii_case(host)),
    }
}

/// The declared platform list for a registry entry plus whether it excludes this
/// host. The single reading shared by the install gate and the browse surface.
pub fn entry_platform_state(entry: &serde_json::Value) -> (Option<Vec<String>>, bool) {
    let platforms = declared_platforms(entry);
    let unsupported = !supports_host(platforms.as_deref(), host_platform());
    (platforms, unsupported)
}

/// A refusal: the registry vouches for platforms that do not include this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedHost {
    pub host: String,
    pub platforms: Vec<String>,
}

impl UnsupportedHost {
    /// Refusal for the running host against a declared platform list.
    pub fn new(platforms: Vec<String>) -> Self {
        UnsupportedHost {
            host: host_platform().to_string(),
            platforms,
        }
    }
}

impl std::fmt::Display for UnsupportedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unsupported platform: this host is {}, the registry vouches for {} \
             — pass --ignore-platform to proceed anyway",
            self.host,
            self.platforms.join(", ")
        )
    }
}

/// Check a registry entry (or manifest) against this host. `None` means the
/// entry is installable here; `Some` carries the refusal to report.
pub fn check_host(entry: &serde_json::Value) -> Option<UnsupportedHost> {
    match entry_platform_state(entry) {
        (Some(platforms), true) => Some(UnsupportedHost::new(platforms)),
        _ => None,
    }
}

/// A platform that is never the host, so refusal paths are exercised the same
/// way whichever OS the suite runs on.
#[cfg(test)]
pub(crate) fn foreign_platform() -> &'static str {
    if host_platform() == "linux" {
        "windows"
    } else {
        "linux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_maps_to_darwin() {
        assert_eq!(normalize_os("macos"), "darwin");
    }

    #[test]
    fn known_platform_names_pass_through() {
        assert_eq!(normalize_os("linux"), "linux");
        assert_eq!(normalize_os("windows"), "windows");
    }

    #[test]
    fn unknown_host_matches_no_declared_platform() {
        // freebsd is not in the vocabulary, so it stays itself and is covered by
        // nothing — the escape hatch, not an implicit allow, is the way in.
        let os = normalize_os("freebsd");
        assert_eq!(os, "freebsd");
        let declared = vec!["linux".to_string(), "darwin".into(), "windows".into()];
        assert!(!supports_host(Some(&declared), os));
    }

    #[test]
    fn host_platform_is_the_mapped_build_target() {
        assert_eq!(host_platform(), normalize_os(std::env::consts::OS));
        assert!(!host_platform().is_empty());
    }

    #[test]
    fn absent_list_is_unrestricted() {
        assert!(supports_host(None, "linux"));
        assert!(supports_host(None, "darwin"));
        assert_eq!(declared_platforms(&serde_json::json!({"id": "s"})), None);
        assert!(check_host(&serde_json::json!({"id": "s"})).is_none());
    }

    #[test]
    fn empty_list_reads_as_absent() {
        let entry = serde_json::json!({"platforms": []});
        assert_eq!(declared_platforms(&entry), None);
        assert!(check_host(&entry).is_none());

        let blanks = serde_json::json!({"platforms": ["", "  "]});
        assert_eq!(declared_platforms(&blanks), None);
    }

    #[test]
    fn declared_list_is_read_and_matched_case_insensitively() {
        let entry = serde_json::json!({"platforms": ["Linux", " darwin "]});
        let declared = declared_platforms(&entry).unwrap();
        assert_eq!(declared, vec!["Linux".to_string(), "darwin".into()]);
        assert!(supports_host(Some(&declared), "linux"));
        assert!(supports_host(Some(&declared), "darwin"));
        assert!(!supports_host(Some(&declared), "windows"));
    }

    #[test]
    fn foreign_platform_is_refused_on_this_host() {
        let entry = serde_json::json!({"platforms": [foreign_platform()]});
        let (platforms, unsupported) = entry_platform_state(&entry);
        assert_eq!(
            platforms.as_deref(),
            Some([foreign_platform().to_string()].as_slice())
        );
        assert!(unsupported);

        let refusal = check_host(&entry).expect("foreign platform must refuse");
        assert_eq!(refusal.host, host_platform());
        let msg = refusal.to_string();
        assert!(
            msg.contains(foreign_platform()),
            "message names the supported platforms: {msg}"
        );
        assert!(
            msg.contains("--ignore-platform"),
            "message names the override: {msg}"
        );
    }

    #[test]
    fn host_platform_is_supported_by_its_own_entry() {
        let entry = serde_json::json!({"platforms": [host_platform()]});
        assert!(check_host(&entry).is_none());
        let (_, unsupported) = entry_platform_state(&entry);
        assert!(!unsupported);
    }
}
