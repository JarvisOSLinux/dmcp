//! Execution backends: where a stdio server's launch line actually runs.
//!
//! A stdio transport names a program and its arguments, and dmcp spawns exactly
//! that on the host. An optional `execution` object **on the transport** (sibling
//! of `command`/`args`/`platforms`, so it can differ per host like the launch
//! line it modifies) says to run that same line somewhere else instead:
//!
//! ```json
//! "execution": {"type": "docker", "image": "python:3-slim", "mountInstallDir": "ro"}
//! "execution": {"wrapper": ["ssh", "build-host", "--"]}
//! ```
//!
//! Both spellings are already *expressible* without this module — a manifest can
//! put `docker` in `command` and the rest in `args`. Making them first-class buys
//! three things that spelling cannot: the form is validated instead of failing at
//! spawn time as "No such file or directory"; the container gets the install dir,
//! the working directory and the configured environment without the manifest
//! restating them; and, for docker, the configured **values** stay out of the
//! process table (see `plan_spawn`).
//!
//! Trust is unchanged by this feature. An execution block is part of the
//! manifest, so it is trusted exactly as far as that manifest is — SHA-256
//! verified against the registry entry at install, gated by the same trust tier
//! — and it grants a manifest no reach it did not already have through
//! `command`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::discovery::Scope;

/// How the host install directory is exposed inside the container.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountInstallDir {
    /// Visible, not writable. The default: a server usually needs its own code
    /// and data, and almost never needs to modify them.
    #[default]
    Ro,
    Rw,
    None,
}

impl MountInstallDir {
    /// The `:mode` suffix of the `-v` flag, or `None` when nothing is mounted.
    fn volume_mode(self) -> Option<&'static str> {
        match self {
            MountInstallDir::Ro => Some("ro"),
            MountInstallDir::Rw => Some("rw"),
            MountInstallDir::None => Option::None,
        }
    }
}

/// A validated execution backend.
///
/// The two forms are separate variants rather than one struct of optional
/// fields, so "docker without an image" and "an empty wrapper" are states the
/// rest of dmcp cannot observe: they are rejected while parsing, and
/// `Wrapper`'s program is a field of its own, so an empty argv is not
/// representable at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ExecutionRepr", into = "ExecutionRepr")]
pub enum Execution {
    Docker {
        image: String,
        mount_install_dir: MountInstallDir,
        extra_args: Vec<String>,
    },
    /// A generic argv prefix. dmcp knows nothing about what it does — it puts
    /// the launch line after it and steps back.
    Wrapper { program: String, args: Vec<String> },
}

/// The on-disk shape, deserialized before validation.
///
/// `deny_unknown_fields` is a security property, not tidiness. Absence of
/// `execution` means "spawn the launch line on the host, unwrapped". If a typo
/// (`"imgae"`) were ignored, `{"type":"docker","imgae":"python:3-slim"}` would
/// fail the image check and — under any lenient reading — degrade to that
/// unwrapped host spawn, which is the one outcome a manifest asking for a
/// container must never silently get. So every unreadable execution block is an
/// error, and an error here fails the whole manifest parse: `dmcp list` warns by
/// name that the manifest does not parse, and `uninstall` reads the index rather
/// than the manifest, so such a server is still removable. This is deliberately
/// *stricter* than `platforms`, which degrades to "covers no host" — there,
/// leniency costs a refusal; here it would cost the isolation that was asked for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ExecutionRepr {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mount_install_dir: Option<MountInstallDir>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    extra_args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wrapper: Option<Vec<String>>,
}

impl TryFrom<ExecutionRepr> for Execution {
    type Error = String;

    fn try_from(repr: ExecutionRepr) -> Result<Self, Self::Error> {
        match (repr.kind.as_deref(), repr.wrapper) {
            (Some(_), Some(_)) => Err("execution: `type` and `wrapper` are mutually exclusive \
                                       — a launch line runs through one backend, not two"
                .to_string()),
            (Some("docker"), None) => {
                let image = repr
                    .image
                    .filter(|i| !i.trim().is_empty())
                    .ok_or("execution: `type: \"docker\"` requires a non-empty `image`")?;
                Ok(Execution::Docker {
                    image,
                    mount_install_dir: repr.mount_install_dir.unwrap_or_default(),
                    extra_args: repr.extra_args,
                })
            }
            (Some(other), None) => Err(format!(
                "execution: unsupported type {other:?} — dmcp implements \"docker\"; \
                 anything else goes through `wrapper`"
            )),
            (None, Some(wrapper)) => {
                // The docker-only keys are rejected rather than ignored for the
                // same reason unknown keys are: a manifest that set `image`
                // clearly meant a container, and running it through the wrapper
                // alone would quietly not be that.
                if repr.image.is_some() || repr.mount_install_dir.is_some() {
                    return Err("execution: `image` and `mountInstallDir` belong to \
                                `type: \"docker\"`, not to `wrapper`"
                        .to_string());
                }
                if !repr.extra_args.is_empty() {
                    return Err("execution: `extraArgs` belongs to `type: \"docker\"`; \
                                a wrapper's arguments go in `wrapper` itself"
                        .to_string());
                }
                let mut argv = wrapper.into_iter();
                let program = argv
                    .next()
                    .filter(|p| !p.trim().is_empty())
                    .ok_or("execution: `wrapper` must name a program to run")?;
                Ok(Execution::Wrapper {
                    program,
                    args: argv.collect(),
                })
            }
            (None, None) => {
                Err("execution: needs either `type: \"docker\"` or `wrapper`".to_string())
            }
        }
    }
}

impl From<Execution> for ExecutionRepr {
    fn from(e: Execution) -> Self {
        match e {
            Execution::Docker {
                image,
                mount_install_dir,
                extra_args,
            } => ExecutionRepr {
                kind: Some("docker".to_string()),
                image: Some(image),
                // Written back explicitly even when it was defaulted, so a
                // rewritten manifest states the mount mode it is running with.
                mount_install_dir: Some(mount_install_dir),
                extra_args,
                wrapper: None,
            },
            Execution::Wrapper { program, args } => {
                let mut wrapper = vec![program];
                wrapper.extend(args);
                ExecutionRepr {
                    kind: None,
                    image: None,
                    mount_install_dir: None,
                    extra_args: Vec::new(),
                    wrapper: Some(wrapper),
                }
            }
        }
    }
}

/// Exactly what gets spawned: the whole decision, before any process exists.
///
/// Every stdio spawn site builds its `Command` from one of these and nothing
/// else, so the one-shot call, `tools`, `run` and the session broker cannot
/// drift on how a server is launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnPlan {
    pub program: String,
    pub args: Vec<String>,
    pub current_dir: PathBuf,
    pub env: HashMap<String, OsString>,
}

/// Turn a launch line plus its backend into the process to spawn.
///
/// `current_dir` is the **host** install dir in every case: harmless under
/// docker, where `-w` governs the path inside the container, and necessary for a
/// wrapper, where a relative script (`./run-remote.sh`) has to resolve against
/// the server's own directory.
///
/// `env` — the manifest's config — is set on the spawned process in every case.
/// Under docker that process is the docker client, and the bare `-e KEY` flags
/// below forward the values from there into the container. Behind a wrapper it
/// is the wrapper itself: whether the values travel any further is the wrapper's
/// business, and an `ssh` hop does **not** carry them by default.
pub fn plan_spawn(
    execution: Option<&Execution>,
    command: &str,
    args: &[String],
    install_dir: &Path,
    env: HashMap<String, OsString>,
) -> SpawnPlan {
    let current_dir = install_dir.to_path_buf();
    let (program, argv) = match execution {
        None => (command.to_string(), args.to_vec()),
        Some(Execution::Wrapper {
            program,
            args: prefix,
        }) => {
            let mut argv = prefix.clone();
            argv.push(command.to_string());
            argv.extend_from_slice(args);
            (program.clone(), argv)
        }
        Some(Execution::Docker {
            image,
            mount_install_dir,
            extra_args,
        }) => {
            let dir = install_dir.to_string_lossy().into_owned();
            let mut argv = vec!["run".to_string(), "-i".to_string(), "--rm".to_string()];
            if let Some(mode) = mount_install_dir.volume_mode() {
                argv.push("-v".to_string());
                argv.push(format!("{dir}:{dir}:{mode}"));
            }
            argv.push("-w".to_string());
            argv.push(dir);
            // Config VALUES never reach argv. The bare `-e KEY` form tells docker
            // to forward KEY from its own environment — which `env` below is
            // about to set — so an API token stays out of /proc/<pid>/cmdline,
            // which every local account can read.
            for key in forwardable_keys(&env) {
                argv.push("-e".to_string());
                argv.push(key);
            }
            argv.extend_from_slice(extra_args);
            argv.push(image.clone());
            argv.push(command.to_string());
            argv.extend_from_slice(args);
            ("docker".to_string(), argv)
        }
    };
    SpawnPlan {
        program,
        args: argv,
        current_dir,
        env,
    }
}

/// The config keys safe to name in a bare `-e KEY` flag, in a stable order.
///
/// A key containing `=` would turn the bare form into `-e KEY=VALUE` and put a
/// value on the command line after all; a key that is empty or carries a NUL is
/// not a usable environment name either. Such keys are dropped from the
/// forwarding list rather than spelled out in argv — they are still set on the
/// docker client's own environment, so nothing is hidden, only unforwarded.
/// Sorted because a `HashMap`'s order is arbitrary and an argv that varies run
/// to run is neither reproducible nor testable.
fn forwardable_keys(env: &HashMap<String, OsString>) -> Vec<String> {
    let mut keys: Vec<String> = env
        .keys()
        .filter(|k| !k.is_empty() && !k.contains('=') && !k.contains('\0'))
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// The refusal when a system-scope server declares an execution backend, or
/// `None` when the combination does not arise.
///
/// Checked before any spawn and before any elevation. A system-scope server runs
/// by re-execing dmcp through pkexec, and what that re-exec means once the
/// launch line is inside a container or on the far side of an ssh hop is
/// undefined: the elevated process would be the docker client, not the server,
/// so "the tool runs as root" would silently stop being true.
pub fn system_scope_refusal(
    scope: Scope,
    execution: Option<&Execution>,
    id: &str,
) -> Option<String> {
    if scope != Scope::System || execution.is_none() {
        return None;
    }
    Some(format!(
        "Server '{id}': execution backends are not supported for system-scope \
         servers yet. A system-scope server is launched by re-execing dmcp \
         through pkexec, and pkexec's re-exec semantics inside a container or \
         behind a wrapper are undefined — the elevated process would be the \
         backend, not the server. Install the server in user scope, or drop the \
         `execution` block from its transport."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<Execution, serde_json::Error> {
        serde_json::from_str(json)
    }

    fn err(json: &str) -> String {
        parse(json).unwrap_err().to_string()
    }

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, OsString> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(*v)))
            .collect()
    }

    #[test]
    fn the_docker_form_parses_with_its_defaults() {
        let e = parse(r#"{"type":"docker","image":"python:3-slim"}"#).unwrap();
        assert_eq!(
            e,
            Execution::Docker {
                image: "python:3-slim".to_string(),
                mount_install_dir: MountInstallDir::Ro,
                extra_args: Vec::new(),
            }
        );

        let full = parse(
            r#"{"type":"docker","image":"img","mountInstallDir":"rw",
                "extraArgs":["--cpus","1"]}"#,
        )
        .unwrap();
        assert_eq!(
            full,
            Execution::Docker {
                image: "img".to_string(),
                mount_install_dir: MountInstallDir::Rw,
                extra_args: vec!["--cpus".to_string(), "1".to_string()],
            }
        );
        assert_eq!(
            parse(r#"{"type":"docker","image":"i","mountInstallDir":"none"}"#).unwrap(),
            Execution::Docker {
                image: "i".to_string(),
                mount_install_dir: MountInstallDir::None,
                extra_args: Vec::new(),
            }
        );
    }

    #[test]
    fn the_wrapper_form_parses_as_a_program_and_its_arguments() {
        assert_eq!(
            parse(r#"{"wrapper":["ssh","host","--"]}"#).unwrap(),
            Execution::Wrapper {
                program: "ssh".to_string(),
                args: vec!["host".to_string(), "--".to_string()],
            }
        );
        assert_eq!(
            parse(r#"{"wrapper":["nice"]}"#).unwrap(),
            Execution::Wrapper {
                program: "nice".to_string(),
                args: Vec::new(),
            }
        );
    }

    /// Every one of these must be an error and not a silently unwrapped spawn.
    #[test]
    fn a_malformed_execution_block_is_rejected_rather_than_ignored() {
        let both = err(r#"{"type":"docker","image":"i","wrapper":["ssh"]}"#);
        assert!(both.contains("mutually exclusive"), "{both}");

        let empty = err(r#"{"wrapper":[]}"#);
        assert!(empty.contains("must name a program"), "{empty}");
        assert!(err(r#"{"wrapper":["  "]}"#).contains("must name a program"));

        let imageless = err(r#"{"type":"docker"}"#);
        assert!(imageless.contains("`image`"), "{imageless}");
        assert!(err(r#"{"type":"docker","image":"  "}"#).contains("`image`"));

        // The typo that must never degrade to an unwrapped host spawn.
        let typo = err(r#"{"type":"docker","imgae":"python:3-slim"}"#);
        assert!(
            typo.contains("imgae"),
            "the message names the bad key: {typo}"
        );

        let neither = err(r#"{}"#);
        assert!(neither.contains("either"), "{neither}");

        let unsupported = err(r#"{"type":"podman","image":"i"}"#);
        assert!(unsupported.contains("podman"), "{unsupported}");

        let mixed = err(r#"{"wrapper":["ssh"],"image":"i"}"#);
        assert!(mixed.contains("belong to"), "{mixed}");
        assert!(err(r#"{"wrapper":["ssh"],"extraArgs":["--cpus"]}"#).contains("extraArgs"));
    }

    #[test]
    fn an_execution_block_round_trips_through_serde() {
        for json in [
            r#"{"type":"docker","image":"img","mountInstallDir":"rw","extraArgs":["--cpus","1"]}"#,
            r#"{"wrapper":["ssh","host","--"]}"#,
        ] {
            let parsed = parse(json).unwrap();
            let written = serde_json::to_string(&parsed).unwrap();
            assert_eq!(parse(&written).unwrap(), parsed, "re-reading {written}");
        }

        // A defaulted mount mode is written back explicitly, and writing it
        // again changes nothing.
        let defaulted = parse(r#"{"type":"docker","image":"img"}"#).unwrap();
        let written = serde_json::to_string(&defaulted).unwrap();
        assert!(
            written.contains(r#""mountInstallDir":"ro""#),
            "the effective mount mode is stated: {written}"
        );
        assert_eq!(
            serde_json::to_string(&parse(&written).unwrap()).unwrap(),
            written
        );
    }

    /// The regression pin: with no execution block the plan is the launch line
    /// itself, exactly as every spawn site built it before this module existed.
    #[test]
    fn no_execution_block_plans_the_bare_launch_line() {
        let env = env_of(&[("TOKEN", "s3cr3t")]);
        let plan = plan_spawn(
            None,
            "python3",
            &["server.py".to_string()],
            Path::new("/srv/app"),
            env.clone(),
        );
        assert_eq!(
            plan,
            SpawnPlan {
                program: "python3".to_string(),
                args: vec!["server.py".to_string()],
                current_dir: PathBuf::from("/srv/app"),
                env,
            }
        );
    }

    #[test]
    fn the_docker_plan_is_the_exact_argv() {
        let execution =
            parse(r#"{"type":"docker","image":"python:3-slim","extraArgs":["--cpus","1"]}"#)
                .unwrap();
        let plan = plan_spawn(
            Some(&execution),
            "python3",
            &["server.py".to_string()],
            Path::new("/srv/app"),
            env_of(&[("B_KEY", "second"), ("A_KEY", "first")]),
        );

        assert_eq!(plan.program, "docker");
        assert_eq!(
            plan.args,
            vec![
                "run",
                "-i",
                "--rm",
                "-v",
                "/srv/app:/srv/app:ro",
                "-w",
                "/srv/app",
                "-e",
                "A_KEY",
                "-e",
                "B_KEY",
                "--cpus",
                "1",
                "python:3-slim",
                "python3",
                "server.py",
            ]
        );
        assert_eq!(
            plan.current_dir,
            PathBuf::from("/srv/app"),
            "the host cwd is kept; -w governs the path inside the container"
        );
    }

    /// The property the `-e KEY` bare form exists for: a token is in the
    /// environment of the process dmcp spawns, and nowhere in its command line.
    #[test]
    fn config_values_are_passed_by_environment_and_never_by_argv() {
        let execution = parse(r#"{"type":"docker","image":"img"}"#).unwrap();
        let plan = plan_spawn(
            Some(&execution),
            "server",
            &[],
            Path::new("/srv/app"),
            env_of(&[("BRAVE_API_KEY", "bsk-secret-value")]),
        );

        assert!(
            plan.args.iter().any(|a| a == "BRAVE_API_KEY"),
            "the key is forwarded: {:?}",
            plan.args
        );
        assert!(
            !plan.args.iter().any(|a| a.contains("bsk-secret-value")),
            "no config value may appear in argv: {:?}",
            plan.args
        );
        assert_eq!(
            plan.env.get("BRAVE_API_KEY").map(|v| v.to_string_lossy()),
            Some("bsk-secret-value".into())
        );
    }

    /// A key carrying `=` would smuggle a value into argv through the bare form.
    #[test]
    fn a_key_that_would_carry_a_value_into_argv_is_not_forwarded() {
        let execution = parse(r#"{"type":"docker","image":"img"}"#).unwrap();
        let plan = plan_spawn(
            Some(&execution),
            "server",
            &[],
            Path::new("/srv"),
            env_of(&[("SNEAKY=INJECTED", "x"), ("PLAIN", "y")]),
        );
        assert!(
            !plan.args.iter().any(|a| a.contains('=')),
            "{:?}",
            plan.args
        );
        assert!(plan.args.iter().any(|a| a == "PLAIN"));
    }

    #[test]
    fn mount_none_omits_the_volume_and_rw_asks_for_it() {
        let none = parse(r#"{"type":"docker","image":"i","mountInstallDir":"none"}"#).unwrap();
        let plan = plan_spawn(Some(&none), "s", &[], Path::new("/srv"), HashMap::new());
        assert!(!plan.args.iter().any(|a| a == "-v"), "{:?}", plan.args);
        assert!(
            plan.args.windows(2).any(|w| w == ["-w", "/srv"]),
            "the working directory is set even with nothing mounted: {:?}",
            plan.args
        );

        let rw = parse(r#"{"type":"docker","image":"i","mountInstallDir":"rw"}"#).unwrap();
        let plan = plan_spawn(Some(&rw), "s", &[], Path::new("/srv"), HashMap::new());
        assert!(
            plan.args.iter().any(|a| a == "/srv:/srv:rw"),
            "{:?}",
            plan.args
        );
    }

    #[test]
    fn the_wrapper_plan_prefixes_the_launch_line_and_keeps_the_env_outside() {
        let execution = parse(r#"{"wrapper":["ssh","build-host","--"]}"#).unwrap();
        let env = env_of(&[("TOKEN", "s3cr3t")]);
        let plan = plan_spawn(
            Some(&execution),
            "python3",
            &["server.py".to_string()],
            Path::new("/srv/app"),
            env.clone(),
        );

        assert_eq!(plan.program, "ssh");
        assert_eq!(plan.args, vec!["build-host", "--", "python3", "server.py"]);
        assert_eq!(
            plan.env, env,
            "the config env is set on the wrapper process; carrying it further is the wrapper's job"
        );
        assert!(
            !plan.args.iter().any(|a| a.contains("s3cr3t")),
            "a wrapper argv never carries config values either: {:?}",
            plan.args
        );
        assert_eq!(plan.current_dir, PathBuf::from("/srv/app"));
    }

    #[test]
    fn system_scope_and_an_execution_block_are_refused_together_only() {
        let execution = parse(r#"{"type":"docker","image":"i"}"#).unwrap();
        let msg = system_scope_refusal(Scope::System, Some(&execution), "com.test.s")
            .expect("system scope plus a backend must be refused");
        assert!(msg.contains("com.test.s"), "{msg}");
        assert!(msg.contains("not supported for system-scope"), "{msg}");
        assert!(msg.contains("pkexec"), "the refusal says why: {msg}");

        assert!(system_scope_refusal(Scope::User, Some(&execution), "x").is_none());
        assert!(system_scope_refusal(Scope::System, None, "x").is_none());
    }
}
