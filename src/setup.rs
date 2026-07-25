//! Run setup scripts for MCP servers.
//!
//! Setup scripts can install dependencies, configure the environment, or (for remote servers)
//! prepare connection info. They run with MCP_CONFIG_* and MCP_INSTALL_DIR in the environment.
//!
//! A server that installs on more than one platform needs more than one setup
//! script: `setupScript` is the POSIX one, `setupScriptWindows` (`setup.ps1`)
//! the Windows one, and the host decides which runs.
//!
//! On Unix the interpreter is `sh` — except when the script's shebang asks for
//! bash. `/bin/sh` is dash on Debian and Ubuntu, and dash has no `pipefail`, no
//! arrays and no `[[ ]]`; a `#!/usr/bin/env bash` script fed to it dies on a
//! line that reads as correct. Honouring the shebang is what makes a setup
//! script behave the same way `./setup.sh` would.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::io::Read;
use std::path::Path;
use std::process::Command;

/// Errors from running setup.
#[derive(Debug)]
pub enum SetupError {
    NoSetupScript,
    ScriptNotFound(String),
    FetchFailed(String),
    /// The interpreter could not be started. It names itself and the script:
    /// a bare "No such file or directory" points at neither.
    SpawnFailed {
        program: String,
        script: std::path::PathBuf,
        source: io::Error,
    },
    SetupFailed(i32),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::NoSetupScript => write!(f, "No setup script defined"),
            SetupError::ScriptNotFound(path) => write!(f, "Setup script not found: {}", path),
            SetupError::FetchFailed(msg) => write!(f, "Failed to fetch setup script: {}", msg),
            SetupError::SpawnFailed {
                program,
                script,
                source,
            } => write!(
                f,
                "Failed to run {} with {}: {}",
                script.display(),
                program,
                source
            ),
            SetupError::SetupFailed(code) => write!(f, "Setup script exited with code {}", code),
        }
    }
}

impl std::error::Error for SetupError {}

/// Which of a manifest's two setup-script fields runs on `host`.
///
/// Windows prefers `setupScriptWindows`; falling back to the POSIX field there
/// is deliberate — a server that ships only `setup.sh` then fails loudly at
/// install instead of silently skipping setup and failing later at launch, when
/// nothing points back at the missing dependencies.
pub fn script_for_host<'a>(
    host: &str,
    windows: Option<&'a str>,
    posix: Option<&'a str>,
) -> Option<&'a str> {
    let windows = windows.filter(|s| !s.is_empty());
    let posix = posix.filter(|s| !s.is_empty());
    if host == "windows" {
        windows.or(posix)
    } else {
        posix
    }
}

/// Run the setup script for a server.
///
/// - `setup_script`: Path (relative to install_dir) or URL (http/https)
/// - `install_dir`: Working directory for the script
/// - `config`: Manifest config, injected as MCP_CONFIG_* env vars
pub fn run_setup(
    setup_script: &str,
    install_dir: &Path,
    config: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<(), SetupError> {
    let script_path = if setup_script.starts_with("http://") || setup_script.starts_with("https://")
    {
        fetch_script(setup_script)?
    } else {
        let path = install_dir.join(setup_script);
        if !path.exists() {
            return Err(SetupError::ScriptNotFound(
                path.to_string_lossy().to_string(),
            ));
        }
        path
    };

    let env = build_env(install_dir, config);
    let (programs, args) = script_command(
        crate::platform::host_platform(),
        &script_path,
        read_shebang(&script_path).as_deref(),
    );

    run_script(&programs, &args, &script_path, install_dir, env)
}

/// Run `script` with the first interpreter in `programs` that can be started,
/// falling through to the next one when an interpreter is simply not installed.
///
/// The fallback exists because honouring a `#!/usr/bin/env bash` shebang turned
/// "bash is nicer here" into "bash is required": most registry setup scripts
/// carry that shebang while using only POSIX constructs, and they installed
/// fine on hosts that ship `sh` and no bash. Falling back keeps the dash fix
/// for every host that has bash and keeps those hosts working. The fallback is
/// announced, so a script that genuinely needs bash fails with a traceable
/// cause instead of a mystifying dash syntax error.
fn run_script(
    programs: &[OsString],
    args: &[OsString],
    script: &Path,
    install_dir: &Path,
    env: HashMap<String, OsString>,
) -> Result<(), SetupError> {
    let mut first_failure: Option<(OsString, io::Error)> = None;
    for program in programs {
        if first_failure.is_some() {
            eprintln!(
                "[warn] {}: {} could not be started; falling back to {} — a script \
                 that needs {} will fail here",
                script.display(),
                programs[0].to_string_lossy(),
                program.to_string_lossy(),
                programs[0].to_string_lossy(),
            );
        }
        let result = Command::new(program)
            .args(args)
            .current_dir(install_dir)
            .envs(&env)
            .status();
        match result {
            Ok(status) => {
                if let Some(code) = status.code() {
                    if code != 0 {
                        return Err(SetupError::SetupFailed(code));
                    }
                }
                return Ok(());
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                first_failure.get_or_insert((program.clone(), e));
            }
            Err(e) => {
                return Err(SetupError::SpawnFailed {
                    program: program.to_string_lossy().to_string(),
                    script: script.to_path_buf(),
                    source: e,
                })
            }
        }
    }

    let (program, source) = first_failure.expect("script_command yields an interpreter");
    Err(SetupError::SpawnFailed {
        program: program.to_string_lossy().to_string(),
        script: script.to_path_buf(),
        source,
    })
}

/// Interpreters (preferred first) and arguments that execute `script` on `host`.
///
/// Windows has no POSIX shell to fall back on, so the script goes through
/// PowerShell. `-NoProfile` keeps an operator's profile out of an install, and
/// `-ExecutionPolicy Bypass` is required because a registry-delivered script is
/// unsigned — its integrity comes from the SHA-256 the registry recorded and
/// dmcp verified before writing it, not from Authenticode. A `.ps1` is run the
/// same way anywhere, since nothing else can interpret it — and nothing else
/// can stand in for it either, so that list has exactly one entry.
///
/// Everywhere else the script is a shell script, run by the shell it asks for,
/// with `sh` behind it as the shell that has always run these scripts.
fn script_command(
    host: &str,
    script: &Path,
    shebang: Option<&str>,
) -> (Vec<OsString>, Vec<OsString>) {
    let is_powershell = host == "windows"
        || script
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("ps1"));
    if is_powershell {
        (
            vec![OsString::from("powershell.exe")],
            vec![
                OsString::from("-NoProfile"),
                OsString::from("-ExecutionPolicy"),
                OsString::from("Bypass"),
                OsString::from("-File"),
                script.into(),
            ],
        )
    } else {
        (
            unix_shells(shebang).iter().map(OsString::from).collect(),
            vec![script.into()],
        )
    }
}

/// The Unix shells for a script, preferred first: `sh` unless its shebang asks
/// for bash, in which case bash with `sh` behind it. Only bash is special-cased
/// — it is the one shell whose absence turns working scripts into syntax errors
/// on the distributions where `/bin/sh` is dash, and the one whose own absence
/// a POSIX script survives.
fn unix_shells(shebang: Option<&str>) -> &'static [&'static str] {
    match shebang.and_then(shebang_interpreter) {
        Some("bash") => &["bash", "sh"],
        _ => &["sh"],
    }
}

/// The interpreter a shebang line names: `#!/bin/bash -e`, `#!/usr/bin/bash`
/// and `#!/usr/bin/env bash` all yield `bash`.
fn shebang_interpreter(line: &str) -> Option<&str> {
    let mut words = line.strip_prefix("#!")?.split_whitespace();
    let program = basename(words.next()?);
    if program == "env" {
        // `env` may be handed options (`-S`) and VAR=value assignments before
        // the program name.
        words
            .find(|w| !w.starts_with('-') && !w.contains('='))
            .map(basename)
    } else {
        Some(program)
    }
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// A script's shebang line, if it has one. Only the first line is read: a
/// setup script can be arbitrarily large and none of the rest is our business.
fn read_shebang(path: &Path) -> Option<String> {
    let mut head = [0u8; 256];
    let read = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    let head = &head[..read];
    if !head.starts_with(b"#!") {
        return None;
    }
    let end = head.iter().position(|&b| b == b'\n').unwrap_or(head.len());
    Some(String::from_utf8_lossy(&head[..end]).trim_end().to_string())
}

fn build_env(
    install_dir: &Path,
    config: &std::collections::HashMap<String, serde_json::Value>,
) -> HashMap<String, OsString> {
    let mut env = HashMap::new();
    env.insert(
        "MCP_INSTALL_DIR".to_string(),
        OsString::from(install_dir.to_string_lossy().as_ref()),
    );
    for (key, value) in config {
        let env_key = format!("MCP_CONFIG_{}", key.to_uppercase().replace(['-', '.'], "_"));
        let env_val = match value {
            serde_json::Value::String(s) => s.clone(),
            _ => value.to_string(),
        };
        env.insert(env_key, OsString::from(env_val));
    }
    env
}

fn fetch_script(url: &str) -> Result<std::path::PathBuf, SetupError> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("dmcp/1.0")
        .build()
        .map_err(|e| SetupError::FetchFailed(e.to_string()))?;

    let resp = client
        .get(url)
        .send()
        .map_err(|e| SetupError::FetchFailed(e.to_string()))?;

    if !resp.status().is_success() {
        return Err(SetupError::FetchFailed(format!("HTTP {}", resp.status())));
    }

    let body = resp
        .bytes()
        .map_err(|e| SetupError::FetchFailed(e.to_string()))?;

    // Keep the extension: PowerShell refuses to `-File` anything but a .ps1.
    let ext = if url.ends_with(".ps1") { "ps1" } else { "sh" };
    let temp = std::env::temp_dir().join(format!("dmcp-setup-{}.{}", std::process::id(), ext));
    std::fs::write(&temp, &body).map_err(|e| SetupError::FetchFailed(e.to_string()))?;

    Ok(temp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A directory of scripts, removed on drop.
    struct ScriptDir {
        root: std::path::PathBuf,
    }

    impl ScriptDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let root =
                std::env::temp_dir().join(format!("dmcp-setup-test-{}-{}", std::process::id(), n));
            std::fs::create_dir_all(&root).unwrap();
            ScriptDir { root }
        }

        fn write(&self, name: &str, body: &str) -> std::path::PathBuf {
            let path = self.root.join(name);
            std::fs::write(&path, body).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
    }

    impl Drop for ScriptDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn program_for(host: &str, name: &str, shebang: Option<&str>) -> String {
        let (programs, _) = script_command(host, Path::new(name), shebang);
        programs[0].to_string_lossy().to_string()
    }

    #[test]
    fn windows_runs_the_powershell_script_through_powershell() {
        let (programs, args) = script_command("windows", Path::new("/srv/setup.ps1"), None);
        assert_eq!(programs.len(), 1, "a .ps1 has no substitute interpreter");
        let program = &programs[0];
        assert_eq!(program.to_string_lossy(), "powershell.exe");
        let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into()).collect();
        assert_eq!(
            args,
            vec![
                "-NoProfile".to_string(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-File".into(),
                "/srv/setup.ps1".into(),
            ]
        );
    }

    /// Nothing but PowerShell can interpret a .ps1, so the extension decides
    /// even where the host does not.
    #[test]
    fn a_powershell_script_is_never_handed_to_a_posix_shell() {
        assert_eq!(
            program_for("linux", "/srv/setup.ps1", None),
            "powershell.exe"
        );
        assert_eq!(
            program_for("darwin", "/srv/SETUP.PS1", None),
            "powershell.exe"
        );
    }

    #[test]
    fn posix_hosts_run_shell_scripts_with_sh_by_default() {
        for host in ["linux", "darwin", "freebsd"] {
            assert_eq!(
                program_for(host, "/srv/setup.sh", None),
                "sh",
                "host {host}"
            );
            assert_eq!(
                program_for(host, "/srv/setup.sh", Some("#!/bin/sh")),
                "sh",
                "host {host}"
            );
        }
    }

    /// The latent bug this closes: a bash script run by `sh` is run by dash on
    /// Debian and Ubuntu, where `pipefail`, arrays and `[[ ]]` are hard errors.
    #[test]
    fn a_bash_shebang_is_honored_instead_of_sh() {
        for shebang in [
            "#!/usr/bin/env bash",
            "#!/bin/bash",
            "#!/usr/bin/bash",
            "#!/bin/bash -e",
            "#!/usr/bin/env -S bash -e",
            "#!/usr/bin/env FOO=1 bash",
        ] {
            assert_eq!(
                program_for("linux", "/srv/setup.sh", Some(shebang)),
                "bash",
                "shebang {shebang}"
            );
        }
    }

    /// Honouring the shebang must not turn bash into a hard requirement: most
    /// registry setup scripts declare bash and use only POSIX constructs, and
    /// they have to keep installing on a host that ships `sh` and nothing else.
    #[test]
    fn a_bash_script_falls_back_to_sh_when_bash_is_missing() {
        let (programs, _) = script_command(
            "linux",
            Path::new("/srv/setup.sh"),
            Some("#!/usr/bin/env bash"),
        );
        let programs: Vec<String> = programs
            .iter()
            .map(|p| p.to_string_lossy().into())
            .collect();
        assert_eq!(programs, vec!["bash".to_string(), "sh".into()]);

        let (posix, _) = script_command("linux", Path::new("/srv/setup.sh"), Some("#!/bin/sh"));
        assert_eq!(posix.len(), 1, "sh has nothing to fall back to");
    }

    /// The fallback actually runs the script, rather than reporting the first
    /// interpreter's absence as a failed install.
    #[cfg(unix)]
    #[test]
    fn a_missing_interpreter_falls_through_to_the_next_one() {
        let dir = ScriptDir::new();
        let marker = dir.root.join("ran");
        let script = dir.write(
            "s.sh",
            &format!("#!/usr/bin/env bash\ntouch {}\n", marker.display()),
        );
        let programs = vec![
            OsString::from("dmcp-no-such-interpreter"),
            OsString::from("sh"),
        ];
        run_script(
            &programs,
            &[OsString::from(&script)],
            &script,
            &dir.root,
            HashMap::new(),
        )
        .expect("a missing bash must not fail a POSIX script");
        assert!(marker.exists(), "the fallback interpreter ran the script");
    }

    /// With nothing left to fall back to, the error names the interpreter and
    /// the script instead of a bare "No such file or directory".
    #[test]
    fn a_missing_interpreter_with_no_fallback_names_itself() {
        let dir = ScriptDir::new();
        let script = dir.write("s.sh", "#!/usr/bin/env bash\nexit 0\n");
        let programs = vec![OsString::from("dmcp-no-such-interpreter")];
        let err = run_script(
            &programs,
            &[OsString::from(&script)],
            &script,
            &dir.root,
            HashMap::new(),
        )
        .unwrap_err();
        assert!(matches!(err, SetupError::SpawnFailed { .. }));
        let msg = err.to_string();
        assert!(
            msg.contains("dmcp-no-such-interpreter") && msg.contains("s.sh"),
            "the diagnostic names the interpreter and the script: {msg}"
        );
    }

    #[test]
    fn shebangs_naming_other_interpreters_still_get_sh() {
        // Unchanged behavior: only the bash/sh distinction is being fixed.
        for shebang in [
            "#!/usr/bin/env python3",
            "#!/bin/zsh",
            "not a shebang",
            "#!",
        ] {
            assert_eq!(
                program_for("linux", "/srv/setup.sh", Some(shebang)),
                "sh",
                "shebang {shebang}"
            );
        }
    }

    #[test]
    fn shebang_is_read_from_the_scripts_first_line_only() {
        let dir = ScriptDir::new();
        let path = dir.write("s.sh", "#!/usr/bin/env bash\n#!/bin/sh\necho hi\n");
        assert_eq!(read_shebang(&path).as_deref(), Some("#!/usr/bin/env bash"));

        let plain = dir.write("plain.sh", "echo hi\n");
        assert_eq!(read_shebang(&plain), None);

        let bare = dir.write("bare.sh", "#!/bin/bash");
        assert_eq!(read_shebang(&bare).as_deref(), Some("#!/bin/bash"));
    }

    /// End to end on this host: a script that only bash can execute runs
    /// because it declares bash. Under the old always-`sh` invocation this
    /// fails wherever /bin/sh is dash.
    #[cfg(unix)]
    #[test]
    fn a_bash_only_script_runs_to_completion() {
        let dir = ScriptDir::new();
        let marker = dir.root.join("ran");
        let script = dir.write(
            "bash-only.sh",
            &format!(
                "#!/usr/bin/env bash\n\
                 set -o pipefail\n\
                 words=(alpha beta gamma)\n\
                 [[ ${{#words[@]}} -eq 3 ]] || exit 1\n\
                 printf '%s' \"${{words[1]}}\" > {}\n",
                marker.display()
            ),
        );

        run_setup(
            script.to_string_lossy().as_ref(),
            &dir.root,
            &Default::default(),
        )
        .expect("a script declaring bash must be run by bash");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "beta");
    }

    /// A plain POSIX script is untouched by the change.
    #[cfg(unix)]
    #[test]
    fn a_posix_script_still_runs_and_still_reports_failure() {
        let dir = ScriptDir::new();
        let ok = dir.write("ok.sh", "#!/bin/sh\nexit 0\n");
        run_setup(
            ok.to_string_lossy().as_ref(),
            &dir.root,
            &Default::default(),
        )
        .unwrap();

        let bad = dir.write("bad.sh", "#!/bin/sh\nexit 7\n");
        let err = run_setup(
            bad.to_string_lossy().as_ref(),
            &dir.root,
            &Default::default(),
        )
        .unwrap_err();
        assert!(matches!(err, SetupError::SetupFailed(7)));
    }

    #[test]
    fn the_host_picks_which_script_runs() {
        assert_eq!(
            script_for_host("windows", Some("setup.ps1"), Some("setup.sh")),
            Some("setup.ps1")
        );
        for host in ["linux", "darwin"] {
            assert_eq!(
                script_for_host(host, Some("setup.ps1"), Some("setup.sh")),
                Some("setup.sh"),
                "host {host}"
            );
        }
    }

    #[test]
    fn a_windows_host_falls_back_to_the_posix_script() {
        // Loud failure at install beats a silently skipped setup that only
        // surfaces later as a server that will not start.
        assert_eq!(
            script_for_host("windows", None, Some("setup.sh")),
            Some("setup.sh")
        );
        // A Windows-only script is not something a POSIX host tries to run.
        assert_eq!(script_for_host("linux", Some("setup.ps1"), None), None);
        assert_eq!(script_for_host("linux", None, None), None);
        assert_eq!(script_for_host("windows", Some(""), Some("")), None);
    }
}
