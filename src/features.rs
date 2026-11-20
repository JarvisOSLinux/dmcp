//! `requiredFeatures`: the dmcp features a manifest cannot be run without.
//!
//! dmcp skips manifest fields it doesn't know, so a manifest written for a
//! newer dmcp loads in an older one with the new part silently dropped. For a
//! description that is harmless. For a field that changes where or how the
//! server runs, it means the older dmcp launches a different server than the
//! one written: an `execution` backend dropped this way runs the launch line on
//! the host instead of in the container.
//!
//! A manifest that depends on such a field names it here. A dmcp that doesn't
//! know a listed feature refuses the manifest (at install, at connect, and
//! before every launch) instead of running it without the feature.
//!
//! The marker only protects a dmcp that reads it. dmcp 0.2.0 and older predate
//! it and ignore it like any other unknown field, so it cannot keep an
//! `execution` manifest from them: the registry holds those back instead. What
//! it does is make the next feature like this one safe on every dmcp from here
//! on.

use std::fmt;

/// Features this dmcp implements, by the name a manifest lists them under.
/// Add a name here in the same change that teaches dmcp the field it stands
/// for, never before: listing a name dmcp only half-handles is the silent drop
/// this module exists to prevent.
pub const KNOWN: &[&str] = &["execution"];

/// Why a manifest's `requiredFeatures` cannot be met by this dmcp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unmet {
    /// Features this dmcp doesn't implement, in the order listed.
    Unknown(Vec<String>),
    /// Present but not a list of names. Refused rather than read as "requires
    /// nothing": the unreadable part may be the very name that mattered.
    Unreadable(serde_json::Value),
}

impl fmt::Display for Unmet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unmet::Unknown(names) => write!(
                f,
                "it needs dmcp features this dmcp ({}) does not have: {}. Update dmcp to use it",
                env!("CARGO_PKG_VERSION"),
                names.join(", ")
            ),
            Unmet::Unreadable(value) => write!(
                f,
                "its requiredFeatures is not a list of feature names: {}",
                value
            ),
        }
    }
}

/// Check a manifest's `requiredFeatures` value against this dmcp. Absent,
/// `null` and `[]` require nothing.
pub fn unmet(declared: Option<&serde_json::Value>) -> Option<Unmet> {
    let value = declared.filter(|v| !v.is_null())?;
    let Some(items) = value.as_array() else {
        return Some(Unmet::Unreadable(value.clone()));
    };
    let mut unknown = Vec::new();
    for item in items {
        let Some(name) = item.as_str() else {
            return Some(Unmet::Unreadable(value.clone()));
        };
        if !KNOWN.contains(&name) {
            unknown.push(name.to_string());
        }
    }
    (!unknown.is_empty()).then_some(Unmet::Unknown(unknown))
}

/// [`unmet`] over a raw manifest, for the install and connect paths that work
/// in JSON before anything is written.
pub fn unmet_in(manifest: &serde_json::Value) -> Option<Unmet> {
    unmet(manifest.get("requiredFeatures"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_null_and_empty_require_nothing() {
        assert_eq!(unmet(None), None);
        assert_eq!(unmet(Some(&json!(null))), None);
        assert_eq!(unmet(Some(&json!([]))), None);
        assert_eq!(unmet_in(&json!({"id": "x"})), None);
    }

    #[test]
    fn every_known_feature_is_met() {
        let all: Vec<_> = KNOWN.iter().map(|s| json!(s)).collect();
        assert_eq!(unmet(Some(&serde_json::Value::Array(all))), None);
    }

    #[test]
    fn unknown_names_are_reported_and_known_ones_left_out() {
        let got = unmet(Some(&json!(["execution", "teleport", "time-travel"])));
        assert_eq!(
            got,
            Some(Unmet::Unknown(vec![
                "teleport".into(),
                "time-travel".into()
            ]))
        );
        let msg = got.unwrap().to_string();
        assert!(msg.contains("teleport, time-travel"), "{msg}");
        assert!(msg.contains(env!("CARGO_PKG_VERSION")), "{msg}");
    }

    #[test]
    fn names_are_matched_exactly() {
        assert!(unmet(Some(&json!(["Execution"]))).is_some());
        assert!(unmet(Some(&json!([" execution"]))).is_some());
    }

    /// An installed manifest listing a feature this dmcp lacks: one that a
    /// newer dmcp installed before a downgrade, or one copied in by hand.
    mod installed {
        use super::*;
        use crate::paths::Paths;

        struct Tree(std::path::PathBuf);

        impl Drop for Tree {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn installed(name: &str, required: serde_json::Value) -> (Tree, Paths) {
            let root =
                std::env::temp_dir().join(format!("dmcp-features-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let paths = Paths {
                user_sources: root.join("u.list"),
                user_install_dir: root.join("user"),
                system_sources: root.join("s.list"),
                system_install_dir: root.join("system"),
                vector_index_dir: root.join("v"),
            };
            let dir = paths.user_install_dir().join("srv");
            std::fs::create_dir_all(&dir).unwrap();
            // A launch line that cannot start anywhere: if a path skipped the
            // gate it would fail on the spawn instead, with a different error.
            let manifest = json!({
                "id": "srv",
                "requiredFeatures": required,
                "transports": [{"type": "stdio", "command": "dmcp-test-no-such-program"}],
            });
            let manifest_path = dir.join("manifest.json");
            std::fs::write(&manifest_path, manifest.to_string()).unwrap();
            let index = json!({"servers": {"srv": {"location": manifest_path.to_string_lossy()}}});
            std::fs::write(
                paths.user_install_dir().join("index.json"),
                index.to_string(),
            )
            .unwrap();
            (Tree(root), paths)
        }

        #[test]
        fn it_still_loads_so_list_info_and_uninstall_can_see_it() {
            let (_t, paths) = installed("visible", json!(["teleport"]));
            let (manifest, _) = crate::discovery::get_server(&paths, "srv").expect("loads");
            assert_eq!(manifest.required_features, Some(json!(["teleport"])));
        }

        #[test]
        fn run_refuses_it_before_spawning() {
            let (_t, paths) = installed("run", json!(["teleport"]));
            let err = crate::run::run(&paths, "srv", false).unwrap_err();
            assert!(
                matches!(err, crate::run::RunError::UnmetFeatures(_)),
                "got {err:?}"
            );
            assert!(err.to_string().contains("teleport"), "{err}");
        }

        #[tokio::test]
        async fn call_and_tools_refuse_it_before_spawning() {
            let (_t, paths) = installed("call", json!(["teleport"]));
            let err = crate::call::list_tools(&paths, "srv").await.unwrap_err();
            assert!(
                matches!(err, crate::call::CallError::UnmetFeatures(_)),
                "got {err:?}"
            );
            let err = crate::call::call_tool(&paths, "srv", "t", None)
                .await
                .unwrap_err();
            assert!(
                matches!(err, crate::call::CallError::UnmetFeatures(_)),
                "got {err:?}"
            );
        }

        #[test]
        fn a_known_feature_reaches_the_spawn() {
            let (_t, paths) = installed("known", json!(["execution"]));
            let err = crate::run::run(&paths, "srv", false).unwrap_err();
            assert!(
                !matches!(err, crate::run::RunError::UnmetFeatures(_)),
                "got {err:?}"
            );
        }
    }

    #[test]
    fn anything_but_a_list_of_names_is_refused() {
        for bad in [
            json!("execution"),
            json!({"execution": true}),
            json!(["execution", 5]),
        ] {
            assert!(
                matches!(unmet(Some(&bad)), Some(Unmet::Unreadable(_))),
                "{bad} must be refused"
            );
        }
    }
}
