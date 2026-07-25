//! Transport selection and transport-type extraction from manifests.
//!
//! One server entry, one capability, but not necessarily one launch line: the
//! command that starts a server is `python3` on POSIX and `python` on Windows,
//! `.venv/bin/x` here and `.venv\Scripts\x.exe` there. A transport may therefore
//! declare `platforms`, and the transport that runs is the first one this host
//! is in. A transport that declares nothing matches every host, so manifests
//! written before the field behave exactly as they did.
//!
//! Selection lives here, once. Every spawn site — the one-shot `call`, `run`,
//! `tools`, and the session broker — goes through `select`, so none of them can
//! grow its own idea of which transport is current. When nothing matches, that
//! is an error naming the platforms the server was written for: silently taking
//! the first entry only moves the failure to spawn time, where it surfaces as an
//! unreadable "No such file or directory".

use std::path::Path;

use crate::models::Transport;
use crate::platform::{declared_platforms, host_platform, supports_host};

/// Transports exist, but every one of them is declared for other platforms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoTransportForHost {
    pub host: String,
    /// The platforms the server's transports are declared for.
    pub platforms: Vec<String>,
}

impl std::fmt::Display for NoTransportForHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no transport for this host: this host is {}, the server declares \
             transports for {}",
            self.host,
            self.platforms.join(", ")
        )
    }
}

/// Why no transport could be selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// The manifest declares no transports at all.
    Missing,
    /// Transports exist, but none of them covers this host.
    ForeignHost(NoTransportForHost),
}

impl std::fmt::Display for SelectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelectError::Missing => write!(f, "no transports defined"),
            SelectError::ForeignHost(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for SelectError {}

/// The one selection rule, shared by the typed and the raw-JSON views of a
/// transports list: the first entry whose declared platforms cover `host`, where
/// an entry that declares nothing covers every host.
fn select_by<'a, T>(
    transports: &'a [T],
    host: &str,
    declared: impl Fn(&'a T) -> Option<Vec<String>>,
) -> Result<&'a T, SelectError> {
    if transports.is_empty() {
        return Err(SelectError::Missing);
    }
    let mut available: Vec<String> = Vec::new();
    for transport in transports {
        let platforms = declared(transport);
        if supports_host(platforms.as_deref(), host) {
            return Ok(transport);
        }
        for p in platforms.into_iter().flatten() {
            if !available.iter().any(|a| a.eq_ignore_ascii_case(&p)) {
                available.push(p);
            }
        }
    }
    Err(SelectError::ForeignHost(NoTransportForHost {
        host: host.to_string(),
        platforms: available,
    }))
}

/// The transport to launch on `host`. `host` is a parameter so the choice is
/// testable for every platform from one machine.
pub fn select_for_host<'a>(
    transports: Option<&'a [Transport]>,
    host: &str,
) -> Result<&'a Transport, SelectError> {
    let transports = transports.ok_or(SelectError::Missing)?;
    select_by(transports, host, |t| t.platforms().map(<[String]>::to_vec))
}

/// The transport to launch on this host.
pub fn select(transports: Option<&[Transport]>) -> Result<&Transport, SelectError> {
    select_for_host(transports, host_platform())
}

/// Same rule over an unparsed transports array, for the registry-entry paths
/// that work in raw JSON.
pub fn select_json_for_host<'a>(
    transports: &'a [serde_json::Value],
    host: &str,
) -> Result<&'a serde_json::Value, SelectError> {
    select_by(transports, host, declared_platforms)
}

/// Same rule over an unparsed transports array, for this host.
pub fn select_json(transports: &[serde_json::Value]) -> Result<&serde_json::Value, SelectError> {
    select_json_for_host(transports, host_platform())
}

/// Extract the transport type this host would use (e.g. "stdio", "sse",
/// "websocket") from manifest JSON.
pub fn transport_from_manifest_json(json: &serde_json::Value) -> Option<String> {
    let transports = json.get("transports")?.as_array()?;
    let selected = select_json(transports).ok()?;
    selected
        .get("type")
        .and_then(|t| t.as_str())
        .map(String::from)
}

/// Fetch manifest from URL and extract transport type.
pub fn transport_from_manifest_url(
    client: &reqwest::blocking::Client,
    url: &str,
) -> Option<String> {
    let resp = client.get(url).send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().ok()?;
    transport_from_manifest_json(&json)
}

/// Read manifest from local path and extract transport type.
pub fn transport_from_manifest_path(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    transport_from_manifest_json(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stdio(command: &str, platforms: Option<&[&str]>) -> Transport {
        Transport::Stdio {
            command: command.to_string(),
            args: None,
            description: None,
            platforms: platforms.map(|p| p.iter().map(|s| s.to_string()).collect()),
        }
    }

    fn command_of(t: &Transport) -> &str {
        match t {
            Transport::Stdio { command, .. } => command,
            _ => panic!("expected a stdio transport"),
        }
    }

    /// The same manifest, read on three different hosts, launches three
    /// different commands — the point of the field.
    #[test]
    fn each_host_gets_its_own_launch_line() {
        let transports = vec![
            stdio("python3", Some(&["linux", "darwin"])),
            stdio("python.exe", Some(&["windows"])),
        ];
        for (host, expected) in [
            ("linux", "python3"),
            ("darwin", "python3"),
            ("windows", "python.exe"),
        ] {
            let selected = select_for_host(Some(&transports), host).unwrap();
            assert_eq!(command_of(selected), expected, "host {host}");
        }
    }

    #[test]
    fn first_match_wins_not_first_entry() {
        let transports = vec![
            stdio("windows-only", Some(&["windows"])),
            stdio("linux-only", Some(&["linux"])),
        ];
        let selected = select_for_host(Some(&transports), "linux").unwrap();
        assert_eq!(command_of(selected), "linux-only");
    }

    #[test]
    fn absent_platforms_matches_every_host() {
        let transports = vec![stdio("anywhere", None)];
        for host in ["linux", "darwin", "windows", "freebsd"] {
            let selected = select_for_host(Some(&transports), host).unwrap();
            assert_eq!(command_of(selected), "anywhere", "host {host}");
        }
    }

    /// An unrestricted entry is a catch-all, so a platform-specific line must
    /// come first to be reachable.
    #[test]
    fn a_specific_entry_is_preferred_over_a_later_catch_all() {
        let transports = vec![
            stdio("specific", Some(&["windows"])),
            stdio("generic", None),
        ];
        assert_eq!(
            command_of(select_for_host(Some(&transports), "windows").unwrap()),
            "specific"
        );
        assert_eq!(
            command_of(select_for_host(Some(&transports), "linux").unwrap()),
            "generic"
        );
    }

    #[test]
    fn matching_is_case_and_whitespace_insensitive() {
        let transports = vec![stdio("py", Some(&["Linux", " Darwin "]))];
        assert!(select_for_host(Some(&transports), "darwin").is_ok());
    }

    /// The refusal has to name what the server *does* support: "it will not run
    /// here" without that is a dead end for whoever has to fix the manifest.
    #[test]
    fn no_match_is_an_error_naming_the_available_platforms() {
        let transports = vec![
            stdio("win", Some(&["windows"])),
            stdio("mac", Some(&["darwin"])),
        ];
        let err = select_for_host(Some(&transports), "linux").unwrap_err();
        let SelectError::ForeignHost(ref detail) = err else {
            panic!("expected a foreign-host refusal, got {err:?}");
        };
        assert_eq!(detail.host, "linux");
        assert_eq!(
            detail.platforms,
            vec!["windows".to_string(), "darwin".into()]
        );
        let msg = err.to_string();
        assert!(msg.contains("linux"), "message names the host: {msg}");
        assert!(msg.contains("windows"), "message names windows: {msg}");
        assert!(msg.contains("darwin"), "message names darwin: {msg}");
    }

    /// Never silently fall through to entry zero: that spawns the wrong command
    /// and dies later with an error that points nowhere near the manifest.
    #[test]
    fn no_match_never_returns_the_first_entry() {
        let transports = vec![stdio("windows-only", Some(&["windows"]))];
        assert!(select_for_host(Some(&transports), "linux").is_err());
    }

    #[test]
    fn a_missing_or_empty_list_is_missing_not_foreign() {
        assert_eq!(
            select_for_host(None, "linux").unwrap_err(),
            SelectError::Missing
        );
        assert_eq!(
            select_for_host(Some(&[]), "linux").unwrap_err(),
            SelectError::Missing
        );
        assert_eq!(SelectError::Missing.to_string(), "no transports defined");
    }

    #[test]
    fn duplicate_platforms_are_named_once() {
        let transports = vec![
            stdio("a", Some(&["windows"])),
            stdio("b", Some(&["Windows"])),
        ];
        let err = select_for_host(Some(&transports), "linux").unwrap_err();
        let SelectError::ForeignHost(detail) = err else {
            panic!("expected a foreign-host refusal");
        };
        assert_eq!(detail.platforms, vec!["windows".to_string()]);
    }

    /// The raw-JSON view must select identically to the typed one — the registry
    /// paths read unparsed entries.
    #[test]
    fn json_selection_matches_the_typed_rule() {
        let transports = vec![
            serde_json::json!({"type": "stdio", "command": "py", "platforms": ["linux"]}),
            serde_json::json!({"type": "sse", "url": "https://example.invalid", "platforms": ["windows"]}),
        ];
        let linux = select_json_for_host(&transports, "linux").unwrap();
        assert_eq!(linux["type"], "stdio");
        let windows = select_json_for_host(&transports, "windows").unwrap();
        assert_eq!(windows["type"], "sse");
        assert!(select_json_for_host(&transports, "darwin").is_err());
    }

    /// Round-trip through serde: the field is read from a manifest and, when
    /// absent, is not written back into one.
    #[test]
    fn platforms_round_trip_through_serde() {
        let json = r#"{"type":"stdio","command":"py","platforms":["windows"]}"#;
        let parsed: Transport = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.platforms(), Some(["windows".to_string()].as_slice()));
        assert!(serde_json::to_string(&parsed).unwrap().contains("windows"));

        let bare: Transport = serde_json::from_str(r#"{"type":"stdio","command":"py"}"#).unwrap();
        assert_eq!(bare.platforms(), None);
        assert!(
            !serde_json::to_string(&bare).unwrap().contains("platforms"),
            "an absent list must not be written back as null"
        );
    }

    #[test]
    fn manifest_transport_type_follows_the_host() {
        let manifest = serde_json::json!({
            "transports": [
                {
                    "type": "sse",
                    "url": "https://example.invalid",
                    "platforms": [crate::platform::foreign_platform()],
                },
                {"type": "stdio", "command": "py"},
            ]
        });
        assert_eq!(
            transport_from_manifest_json(&manifest).as_deref(),
            Some("stdio"),
            "a windows-only endpoint is not what this host would launch"
        );
    }
}
