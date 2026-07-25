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
//!
//! A declaration reads as one of three things — absent, malformed, or a list —
//! and only absence means unrestricted. A gate that cannot parse its input must
//! not conclude "nothing was restricted": `"platforms": "windows"` is one
//! keystroke away from `["windows"]`, and reading it as "no restriction" turns
//! a single typo into a silently disabled gate.

use serde::{Deserialize, Serialize};

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

/// How a `platforms` field reads. Three states, because "absent" and
/// "unreadable" are not the same claim about a server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PlatformDecl {
    /// No usable declaration: the key is missing, `null`, or a list that trims
    /// to nothing. Unrestricted — exactly the behavior of every manifest
    /// written before the field existed.
    ///
    /// An empty array lands here deliberately: a list that vouches for nothing
    /// is far more likely to be a serialization slip than "installable
    /// nowhere", and the strict reading would brick a server on every host at
    /// once.
    #[default]
    Absent,
    /// Present but not an array of platform names, carried verbatim so nothing
    /// is silently rewritten. Covers no host: `--ignore-platform` is the way
    /// past it, so a third-party registry with a slip is refused, not bricked.
    Malformed(serde_json::Value),
    /// The platforms named, blanks trimmed away.
    Declared(Vec<String>),
}

impl PlatformDecl {
    /// Classify one `platforms` value. Never fails — an unreadable declaration
    /// is a state, not a parse error, so one bad field cannot make an entire
    /// manifest unloadable and hide an installed server from every command.
    pub fn from_value(value: &serde_json::Value) -> Self {
        if value.is_null() {
            return PlatformDecl::Absent;
        }
        let Some(array) = value.as_array() else {
            return PlatformDecl::Malformed(value.clone());
        };
        let mut names = Vec::with_capacity(array.len());
        for item in array {
            match item.as_str() {
                Some(name) => names.push(name.to_string()),
                // Dropping the non-string element instead would read
                // `["linux", 5]` as a narrower list than was written.
                None => return PlatformDecl::Malformed(value.clone()),
            }
        }
        PlatformDecl::from_names(&names)
    }

    /// Classify an already-typed list. The empty/blank rule lives here once, so
    /// the raw-JSON and typed readings of `platforms` cannot drift apart.
    pub fn from_names(names: &[String]) -> Self {
        let list: Vec<String> = names
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if list.is_empty() {
            PlatformDecl::Absent
        } else {
            PlatformDecl::Declared(list)
        }
    }

    /// Whether this declaration covers `host`.
    pub fn supports(&self, host: &str) -> bool {
        match self {
            PlatformDecl::Absent => true,
            PlatformDecl::Malformed(_) => false,
            PlatformDecl::Declared(list) => list.iter().any(|p| p.eq_ignore_ascii_case(host)),
        }
    }

    /// The platform names declared, when the declaration could be read.
    pub fn names(&self) -> Option<&[String]> {
        match self {
            PlatformDecl::Declared(list) => Some(list),
            _ => None,
        }
    }

    pub fn is_absent(&self) -> bool {
        matches!(self, PlatformDecl::Absent)
    }

    pub fn is_malformed(&self) -> bool {
        matches!(self, PlatformDecl::Malformed(_))
    }
}

impl Serialize for PlatformDecl {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            PlatformDecl::Absent => serializer.serialize_none(),
            PlatformDecl::Declared(list) => list.serialize(serializer),
            // Write back what was read: rewriting an operator's file on the way
            // past it would destroy the evidence of what went wrong.
            PlatformDecl::Malformed(raw) => raw.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for PlatformDecl {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(PlatformDecl::from_value(&value))
    }
}

/// The `platforms` declared by a registry entry or a manifest.
pub fn platform_decl(entry: &serde_json::Value) -> PlatformDecl {
    match entry.get("platforms") {
        Some(value) => PlatformDecl::from_value(value),
        None => PlatformDecl::Absent,
    }
}

/// A refusal: the entry's `platforms` do not cover this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedHost {
    pub host: String,
    /// The platforms the entry vouches for; empty when the declaration itself
    /// could not be read.
    pub platforms: Vec<String>,
    /// The declaration was present but unreadable, so nothing at all was
    /// vouched for — a different fact from "vouched for other platforms".
    pub malformed: bool,
}

impl UnsupportedHost {
    /// Refusal for the running host against a declared platform list.
    pub fn new(platforms: Vec<String>) -> Self {
        UnsupportedHost {
            host: host_platform().to_string(),
            platforms,
            malformed: false,
        }
    }

    /// Refusal for an entry whose `platforms` field could not be read.
    pub fn malformed() -> Self {
        UnsupportedHost {
            host: host_platform().to_string(),
            platforms: Vec::new(),
            malformed: true,
        }
    }

    /// The refusal a declaration warrants on this host.
    pub fn for_decl(decl: &PlatformDecl) -> Self {
        match decl {
            PlatformDecl::Malformed(_) => UnsupportedHost::malformed(),
            other => UnsupportedHost::new(other.names().unwrap_or_default().to_vec()),
        }
    }
}

impl std::fmt::Display for UnsupportedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.malformed {
            return write!(
                f,
                "unsupported platform: this host is {}, and the entry's `platforms` \
                 field is not an array of platform names (\"linux\", \"darwin\", \
                 \"windows\") — pass --ignore-platform to proceed anyway",
                self.host
            );
        }
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
    let decl = platform_decl(entry);
    (!decl.supports(host_platform())).then(|| UnsupportedHost::for_decl(&decl))
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
        let declared =
            PlatformDecl::from_names(&["linux".to_string(), "darwin".into(), "windows".into()]);
        assert!(!declared.supports(os));
    }

    #[test]
    fn host_platform_is_the_mapped_build_target() {
        assert_eq!(host_platform(), normalize_os(std::env::consts::OS));
        assert!(!host_platform().is_empty());
    }

    /// Every host is covered by a declaration that is not there.
    fn covers(decl: &PlatformDecl, host: &str) -> bool {
        decl.supports(host)
    }

    #[test]
    fn absent_list_is_unrestricted() {
        assert!(covers(&PlatformDecl::Absent, "linux"));
        assert!(covers(&PlatformDecl::Absent, "darwin"));
        assert_eq!(
            platform_decl(&serde_json::json!({"id": "s"})),
            PlatformDecl::Absent
        );
        assert_eq!(
            platform_decl(&serde_json::json!({"platforms": null})),
            PlatformDecl::Absent
        );
        assert!(check_host(&serde_json::json!({"id": "s"})).is_none());
    }

    #[test]
    fn empty_list_reads_as_absent() {
        let entry = serde_json::json!({"platforms": []});
        assert_eq!(platform_decl(&entry), PlatformDecl::Absent);
        assert!(check_host(&entry).is_none());

        let blanks = serde_json::json!({"platforms": ["", "  "]});
        assert_eq!(platform_decl(&blanks), PlatformDecl::Absent);
        assert!(check_host(&blanks).is_none());

        // The same rule for a list that arrives already typed.
        assert_eq!(PlatformDecl::from_names(&[]), PlatformDecl::Absent);
        assert_eq!(
            PlatformDecl::from_names(&["".to_string(), "  ".into()]),
            PlatformDecl::Absent
        );
        for host in ["linux", "darwin", "windows"] {
            assert!(covers(&PlatformDecl::from_names(&[]), host), "host {host}");
            assert!(
                covers(
                    &PlatformDecl::from_names(&["".to_string(), "  ".into()]),
                    host
                ),
                "host {host}"
            );
        }
    }

    /// A declaration that cannot be read is not a declaration of nothing: the
    /// gate refuses instead of switching itself off.
    #[test]
    fn a_malformed_declaration_refuses_every_host() {
        for value in [
            serde_json::json!("windows"),
            serde_json::json!({"windows": true}),
            serde_json::json!([123]),
            serde_json::json!([["windows"]]),
            serde_json::json!(["linux", 5]),
            serde_json::json!([host_platform(), 5]),
            serde_json::json!(7),
            serde_json::json!(true),
        ] {
            let entry = serde_json::json!({"platforms": value.clone()});
            let decl = platform_decl(&entry);
            assert!(decl.is_malformed(), "{value} must read as malformed");
            assert_eq!(decl.names(), None);
            for host in ["linux", "darwin", "windows"] {
                assert!(!decl.supports(host), "{value} must cover no host");
            }
            let refusal = check_host(&entry).expect("malformed must refuse");
            assert!(refusal.malformed);
            let msg = refusal.to_string();
            assert!(
                msg.contains("`platforms`") && msg.contains("--ignore-platform"),
                "refusal explains itself and names the override: {msg}"
            );
        }
    }

    /// The malformed value survives a round trip: a manifest is not silently
    /// rewritten on its way through dmcp.
    #[test]
    fn a_malformed_declaration_is_preserved_verbatim() {
        let raw = serde_json::json!("windows");
        let decl = PlatformDecl::from_value(&raw);
        assert_eq!(serde_json::to_value(&decl).unwrap(), raw);

        let declared = PlatformDecl::Declared(vec!["linux".to_string()]);
        assert_eq!(
            serde_json::to_value(&declared).unwrap(),
            serde_json::json!(["linux"])
        );
        assert_eq!(
            serde_json::to_value(PlatformDecl::Absent).unwrap(),
            serde_json::Value::Null
        );
    }

    #[test]
    fn declared_list_is_read_and_matched_case_insensitively() {
        let entry = serde_json::json!({"platforms": ["Linux", " darwin "]});
        let decl = platform_decl(&entry);
        let declared = decl.names().unwrap();
        assert_eq!(declared, vec!["Linux".to_string(), "darwin".into()]);
        assert!(covers(&decl, "linux"));
        assert!(covers(&decl, "darwin"));
        assert!(!covers(&decl, "windows"));
    }

    #[test]
    fn foreign_platform_is_refused_on_this_host() {
        let entry = serde_json::json!({"platforms": [foreign_platform()]});
        let decl = platform_decl(&entry);
        assert_eq!(
            decl.names(),
            Some([foreign_platform().to_string()].as_slice())
        );
        assert!(!decl.supports(host_platform()));

        let refusal = check_host(&entry).expect("foreign platform must refuse");
        assert_eq!(refusal.host, host_platform());
        assert!(!refusal.malformed);
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
        assert!(platform_decl(&entry).supports(host_platform()));
    }
}
