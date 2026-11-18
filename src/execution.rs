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
//! process table and off the docker client (see `SpawnPlan::prepare`).
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
    /// The `--mount` value that binds `dir` at the same path, or `None` when
    /// nothing is mounted.
    ///
    /// `--mount`, not `-v`: `-v` splits its value at every `:`, so an install
    /// dir holding one could not be written at all. `--mount` is one CSV record,
    /// so the path goes in quoted fields, where a `,` or `"` stays part of it.
    fn bind_mount(self, dir: &str) -> Option<String> {
        let readonly = match self {
            MountInstallDir::Ro => true,
            MountInstallDir::Rw => false,
            MountInstallDir::None => return Option::None,
        };
        let mut spec = format!(
            "type=bind,{},{}",
            csv_field(&format!("src={dir}")),
            csv_field(&format!("dst={dir}"))
        );
        if readonly {
            spec.push_str(",readonly");
        }
        Some(spec)
    }
}

/// One quoted field of the CSV record docker reads `--mount` as (Go's
/// `encoding/csv`): inside quotes a `,` is data and `""` is one `"`.
fn csv_field(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
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

impl Execution {
    /// One line saying where the launch line runs, for `info`, `list`,
    /// `browse` and the agent's `get_server_info`. Extra docker flags and the
    /// wrapper's argv are spelled out in full: `--privileged -v /:/host` or a
    /// `sudo -E` wrapper is exactly what must not look like a plain host launch.
    pub fn describe(&self) -> String {
        match self {
            Execution::Docker {
                image,
                mount_install_dir,
                extra_args,
            } => {
                let mount = match mount_install_dir {
                    MountInstallDir::Ro => "install dir read-only",
                    MountInstallDir::Rw => "install dir writable",
                    MountInstallDir::None => "install dir not mounted",
                };
                let mut line = format!("docker image {image}, {mount}");
                if !extra_args.is_empty() {
                    line.push_str(&format!(", extra docker flags: {}", extra_args.join(" ")));
                }
                line
            }
            Execution::Wrapper { program, args } => {
                let mut argv = vec![program.as_str()];
                argv.extend(args.iter().map(String::as_str));
                format!("wrapper: {}", argv.join(" "))
            }
        }
    }
}

/// [`Execution::describe`] for a transport read as raw JSON (a registry
/// entry), or `None` when it declares no backend. A block that does not parse
/// says so, since dmcp would refuse to install it.
pub fn describe_json(transport: &serde_json::Value) -> Option<String> {
    let raw = transport.get("execution")?;
    Some(match Execution::deserialize(raw) {
        Ok(execution) => execution.describe(),
        Err(e) => format!("unreadable execution block ({e})"),
    })
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
    /// Set on the spawned process itself.
    pub env: HashMap<String, OsString>,
    /// Docker only: the container's environment, sorted by key. It reaches
    /// docker through an `--env-file` that [`SpawnPlan::prepare`] writes, and
    /// is never set on the docker client.
    pub container_env: Vec<(String, OsString)>,
}

impl SpawnPlan {
    /// The argv to spawn, plus the env file it names when the container has an
    /// environment.
    ///
    /// The env file goes right after `run`, is written 0600 in dmcp's private
    /// runtime directory, and is removed when the returned [`EnvFile`] drops.
    /// docker reads it while parsing its own flags, before it starts the
    /// container, so a caller keeps it until the server has answered the MCP
    /// handshake (or the child has exited) and may drop it after.
    pub fn prepare(&self) -> Result<(Vec<String>, Option<EnvFile>), String> {
        if self.container_env.is_empty() {
            return Ok((self.args.clone(), None));
        }
        let contents = env_file_contents(&self.container_env)?;
        let file = EnvFile::write(&contents)
            .map_err(|e| format!("could not write the container's env file: {e}"))?;
        let mut args = self.args.clone();
        args.splice(
            1..1,
            [
                "--env-file".to_string(),
                file.path().to_string_lossy().into_owned(),
            ],
        );
        Ok((args, Some(file)))
    }
}

/// A container's env file, removed when dropped.
#[derive(Debug)]
pub struct EnvFile {
    path: PathBuf,
}

impl EnvFile {
    fn write(contents: &str) -> std::io::Result<Self> {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let dir = env_file_dir()?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        for _ in 0..16 {
            let path = dir.join(format!(
                "env-{}-{}-{nanos}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut f) => {
                    // The guard exists before the write, so a failed write
                    // still removes the file.
                    let file = EnvFile { path };
                    f.write_all(contents.as_bytes())?;
                    return Ok(file);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("no free env file name in {}", dir.display()),
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for EnvFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The broker's directory: 0700 and owned by this uid, or refused.
#[cfg(unix)]
fn env_file_dir() -> std::io::Result<PathBuf> {
    let dir = crate::broker::broker_dir();
    crate::broker::ensure_broker_dir(&dir).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(dir)
}

/// The per-user temp directory.
#[cfg(not(unix))]
fn env_file_dir() -> std::io::Result<PathBuf> {
    Ok(std::env::temp_dir())
}

/// The `--env-file` text for `vars`, or why one of them cannot be carried.
///
/// docker reads the file a line at a time (lines up to 64 KiB), splits each at
/// its first `=`, and rejects a name with whitespace. A setting that does not
/// fit is refused rather than dropped, since a server started without one of
/// its settings fails somewhere else, later, for no stated reason. The message
/// names the key, never the value.
fn env_file_contents(vars: &[(String, OsString)]) -> Result<String, String> {
    const MAX_LINE: usize = 64 * 1024 - 1;
    let mut out = String::new();
    for (key, value) in vars {
        if key.is_empty()
            || key.starts_with('#')
            || key.contains(|c: char| c == '=' || c == '\0' || c.is_whitespace())
        {
            return Err(format!(
                "setting {key:?} is not a name docker's --env-file can carry"
            ));
        }
        let value = value
            .to_str()
            .ok_or_else(|| format!("setting {key} is not valid UTF-8"))?;
        if value.contains(['\n', '\r', '\0']) {
            return Err(format!(
                "setting {key} contains a line break, which docker's --env-file cannot carry"
            ));
        }
        if key.len() + 1 + value.len() > MAX_LINE {
            return Err(format!(
                "setting {key} is longer than docker's --env-file reads (64 KiB a line)"
            ));
        }
        out.push_str(key);
        out.push('=');
        out.push_str(value);
        out.push('\n');
    }
    Ok(out)
}

/// Turn a launch line plus its backend into the process to spawn.
///
/// `current_dir` is the **host** install dir in every case: harmless under
/// docker, where `-w` governs the path inside the container, and necessary for a
/// wrapper, where a relative script (`./run-remote.sh`) has to resolve against
/// the server's own directory.
///
/// `env` is the manifest's config. Without a backend it is set on the server
/// process. Behind a wrapper it is set on the wrapper: whether the values
/// travel any further is the wrapper's business, and an `ssh` hop does **not**
/// carry them by default. Under docker it becomes `container_env` and is not
/// set on the docker client at all: a setting named `DOCKER_HOST`, `HOME` or
/// `PATH` is meant for the server, and on the client it would redirect docker
/// itself.
pub fn plan_spawn(
    execution: Option<&Execution>,
    command: &str,
    args: &[String],
    install_dir: &Path,
    env: HashMap<String, OsString>,
) -> SpawnPlan {
    let current_dir = install_dir.to_path_buf();
    match execution {
        None => SpawnPlan {
            program: command.to_string(),
            args: args.to_vec(),
            current_dir,
            env,
            container_env: Vec::new(),
        },
        Some(Execution::Wrapper {
            program,
            args: prefix,
        }) => {
            let mut argv = prefix.clone();
            argv.push(command.to_string());
            argv.extend_from_slice(args);
            SpawnPlan {
                program: program.clone(),
                args: argv,
                current_dir,
                env,
                container_env: Vec::new(),
            }
        }
        Some(Execution::Docker {
            image,
            mount_install_dir,
            extra_args,
        }) => {
            let dir = install_dir.to_string_lossy().into_owned();
            let mut argv = vec!["run".to_string(), "-i".to_string(), "--rm".to_string()];
            if let Some(spec) = mount_install_dir.bind_mount(&dir) {
                argv.push("--mount".to_string());
                argv.push(spec);
            }
            argv.push("-w".to_string());
            argv.push(dir);
            argv.extend_from_slice(extra_args);
            argv.push(image.clone());
            argv.push(command.to_string());
            argv.extend_from_slice(args);
            // Sorted: a HashMap's order is arbitrary, and an env file that
            // varies run to run is neither reproducible nor testable.
            let mut container_env: Vec<(String, OsString)> = env.into_iter().collect();
            container_env.sort();
            SpawnPlan {
                program: "docker".to_string(),
                args: argv,
                current_dir,
                env: HashMap::new(),
                container_env,
            }
        }
    }
}

/// The refusal when a system-scope server declares an execution backend, or
/// `None` when the combination does not arise.
///
/// Checked before any spawn and before any elevation. A system-scope server runs
/// by re-execing dmcp as root (pkexec, or `sudo -A`), and what that re-exec means once the
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
         as root (through pkexec, or sudo -A), and what that re-exec means inside \
         a container or behind a wrapper is undefined — the elevated process would be the \
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
                container_env: Vec::new(),
            }
        );
        let (args, env_file) = plan.prepare().unwrap();
        assert_eq!(args, plan.args);
        assert!(env_file.is_none());
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
                "--mount",
                r#"type=bind,"src=/srv/app","dst=/srv/app",readonly"#,
                "-w",
                "/srv/app",
                "--cpus",
                "1",
                "python:3-slim",
                "python3",
                "server.py",
            ]
        );
        assert_eq!(
            plan.container_env,
            vec![
                ("A_KEY".to_string(), OsString::from("first")),
                ("B_KEY".to_string(), OsString::from("second")),
            ],
            "sorted, so the env file is the same every run"
        );
        assert!(plan.env.is_empty());
        assert_eq!(
            plan.current_dir,
            PathBuf::from("/srv/app"),
            "the host cwd is kept; -w governs the path inside the container"
        );
    }

    /// Toufic's review of #63: config and the docker client shared one
    /// environment, so a setting named `DOCKER_HOST`, `HOME` or `PATH` meant for
    /// the server redirected docker itself.
    #[test]
    fn settings_docker_itself_reads_go_to_the_container_only() {
        let execution = parse(r#"{"type":"docker","image":"img"}"#).unwrap();
        let plan = plan_spawn(
            Some(&execution),
            "server",
            &[],
            Path::new("/srv/app"),
            env_of(&[
                ("DOCKER_HOST", "tcp://elsewhere:2375"),
                ("HOME", "/data"),
                ("PATH", "/opt/bin"),
            ]),
        );
        assert!(plan.env.is_empty(), "{:?}", plan.env);
        let (args, env_file) = plan.prepare().unwrap();
        let env_file = env_file.expect("an env file for the container");
        assert_eq!(
            std::fs::read_to_string(env_file.path()).unwrap(),
            "DOCKER_HOST=tcp://elsewhere:2375\nHOME=/data\nPATH=/opt/bin\n"
        );
        assert_eq!(
            &args[..3],
            ["run", "--env-file", &*env_file.path().to_string_lossy()]
        );
    }

    /// The env file holds what the docker client's environment used to: owner
    /// only, and only while the container starts.
    #[test]
    fn the_env_file_is_private_and_removed_on_drop() {
        let execution = parse(r#"{"type":"docker","image":"img"}"#).unwrap();
        let plan = plan_spawn(
            Some(&execution),
            "server",
            &[],
            Path::new("/srv/app"),
            env_of(&[("BRAVE_API_KEY", "bsk-secret-value")]),
        );
        let (args, env_file) = plan.prepare().unwrap();
        let env_file = env_file.unwrap();
        let path = env_file.path().to_path_buf();
        assert!(
            !args.iter().any(|a| a.contains("bsk-secret-value")),
            "no config value may appear in argv: {args:?}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{path:?}");
        }
        drop(env_file);
        assert!(!path.exists(), "{path:?}");
    }

    /// docker reads the file a line at a time and splits at the first `=`, so
    /// what it cannot carry is refused by name, never written half-right.
    #[test]
    fn a_setting_the_env_file_cannot_carry_is_refused_by_name() {
        let refused = |key: &str, value: &str| {
            let vars = vec![(key.to_string(), OsString::from(value))];
            env_file_contents(&vars).unwrap_err()
        };
        assert!(refused("SNEAKY=INJECTED", "x").contains("SNEAKY=INJECTED"));
        assert!(refused("TWO WORDS", "x").contains("TWO WORDS"));
        assert!(refused("", "x").contains("not a name"));
        assert!(refused("#COMMENT", "x").contains("#COMMENT"));
        let msg = refused("PEM", "-----BEGIN-----\nsecret-body\n");
        assert!(msg.contains("PEM") && msg.contains("line break"), "{msg}");
        assert!(
            !msg.contains("secret-body"),
            "the value is never echoed: {msg}"
        );
        assert!(refused("CR", "a\rb").contains("line break"));
        assert!(refused("LONG", &"x".repeat(64 * 1024)).contains("64 KiB"));

        let vars = vec![("OK".to_string(), OsString::from("a = b # c, \"d\""))];
        assert_eq!(env_file_contents(&vars).unwrap(), "OK=a = b # c, \"d\"\n");
    }

    #[test]
    fn mount_none_omits_the_mount_and_rw_drops_readonly() {
        let none = parse(r#"{"type":"docker","image":"i","mountInstallDir":"none"}"#).unwrap();
        let plan = plan_spawn(Some(&none), "s", &[], Path::new("/srv"), HashMap::new());
        assert!(!plan.args.iter().any(|a| a == "--mount"), "{:?}", plan.args);
        assert!(
            plan.args.windows(2).any(|w| w == ["-w", "/srv"]),
            "the working directory is set even with nothing mounted: {:?}",
            plan.args
        );

        let rw = parse(r#"{"type":"docker","image":"i","mountInstallDir":"rw"}"#).unwrap();
        let plan = plan_spawn(Some(&rw), "s", &[], Path::new("/srv"), HashMap::new());
        assert!(
            plan.args
                .windows(2)
                .any(|w| w == ["--mount", r#"type=bind,"src=/srv","dst=/srv""#]),
            "{:?}",
            plan.args
        );
    }

    /// `-v` split at every `:`, so such a path could not be mounted at all, and
    /// a `,` or `"` would end a field of `--mount`'s CSV record unless quoted.
    #[test]
    fn an_install_dir_with_colons_commas_or_quotes_mounts_intact() {
        let ro = parse(r#"{"type":"docker","image":"i"}"#).unwrap();
        let dir = Path::new(r#"/home/a:b/c,d/"e""#);
        let plan = plan_spawn(Some(&ro), "s", &[], dir, HashMap::new());
        let spec = &plan.args[plan.args.iter().position(|a| a == "--mount").unwrap() + 1];
        assert_eq!(
            spec,
            r#"type=bind,"src=/home/a:b/c,d/""e""","dst=/home/a:b/c,d/""e""",readonly"#
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
    fn describe_spells_out_what_widens_the_servers_reach() {
        let docker = parse(
            r#"{"type":"docker","image":"img:1","extraArgs":["--privileged","-v","/:/host"]}"#,
        )
        .unwrap();
        assert_eq!(
            docker.describe(),
            "docker image img:1, install dir read-only, extra docker flags: --privileged -v /:/host"
        );
        let rw = parse(r#"{"type":"docker","image":"i","mountInstallDir":"rw"}"#).unwrap();
        assert_eq!(rw.describe(), "docker image i, install dir writable");
        let none = parse(r#"{"type":"docker","image":"i","mountInstallDir":"none"}"#).unwrap();
        assert_eq!(none.describe(), "docker image i, install dir not mounted");
        let wrapper = parse(r#"{"wrapper":["sudo","-E"]}"#).unwrap();
        assert_eq!(wrapper.describe(), "wrapper: sudo -E");
    }

    #[test]
    fn describe_json_reads_a_raw_transport() {
        let plain = serde_json::json!({"type": "stdio", "command": "s"});
        assert_eq!(describe_json(&plain), None);
        let wrapped = serde_json::json!({"type": "stdio", "command": "s",
            "execution": {"wrapper": ["env"]}});
        assert_eq!(describe_json(&wrapped).as_deref(), Some("wrapper: env"));
        let typo = serde_json::json!({"type": "stdio", "command": "s",
            "execution": {"type": "docker", "imgae": "i"}});
        let shown = describe_json(&typo).unwrap();
        assert!(
            shown.starts_with("unreadable execution block") && shown.contains("imgae"),
            "{shown}"
        );
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
