//! Privilege elevation for system-scope operations.
//!
//! Linux: pkexec/polkit, or `sudo -A` when an askpass program is configured
//! and ready ([`SUDO_ASKPASS_ENV`]). macOS: sudo (TTY) or osascript (GUI).
//! Other platforms don't support system scope yet and print guidance instead
//! of attempting it.

use std::path::Path;
use std::process;

/// Returns true if the current process is running as root/administrator.
#[cfg(unix)]
pub fn is_elevated() -> bool {
    nix::unistd::Uid::current().is_root()
}

/// v1 stub: Windows elevation detection needs `TokenElevation` via the `windows`
/// crate; system scope on Windows is not wired up yet, so callers always fall
/// through to the "unsupported" re-exec path below.
#[cfg(windows)]
pub fn is_elevated() -> bool {
    false
}

/// Written to stderr just before the polkit prompt is raised. dispatch streams a
/// task's stderr into its live tail, so this line is how JARVIS (and the phone,
/// app and TUI) can tell "waiting for a password nobody has typed" from "running"
/// (Project-JARVIS#310). Keep the wording in step with the matcher in
/// Project-JARVIS `jarvis/runtime/password_wait.py`.
pub const AUTH_WAITING_NOTICE: &str =
    "dmcp: waiting for the administrator password on the computer running JARVIS (polkit prompt)";

/// [`AUTH_WAITING_NOTICE`] when sudo asks through the askpass program instead
/// of polkit: JARVIS shows the prompt in jarvis-app and on phones.
pub const AUTH_WAITING_NOTICE_SUDO: &str =
    "dmcp: waiting for the administrator password on the computer running JARVIS (sudo askpass)";

/// Names an askpass program for sudo (Project-JARVIS#310). When it is set and
/// the program answers `--check` with success, dmcp elevates with `sudo -A`
/// and that program supplies the password; otherwise it uses pkexec as
/// always. Asked at each elevation, so whoever runs the program (JARVIS) can
/// turn it on and off without restarting dmcp.
pub const SUDO_ASKPASS_ENV: &str = "DMCP_SUDO_ASKPASS";

/// Argv flag a sudo re-exec carries, telling the root half that `SUDO_UID`
/// names the user who asked: sudo resets the environment, and a `SUDO_UID`
/// alone may come from a human's `sudo bash` far up the tree. Stripped in
/// `main` before clap, like [`crate::call::ELEVATION_SENTINEL_FLAG`].
pub const SUDO_REEXEC_FLAG: &str = "--dmcp-internal-sudo-reexec";

/// Written to stderr by the re-exec'd root process once polkit has accepted the
/// password. It ends the wait that [`AUTH_WAITING_NOTICE`] began.
pub const AUTH_GRANTED_NOTICE: &str = "dmcp: administrator password accepted, running as root";

/// The uid of the user a re-exec runs for: `PKEXEC_UID` after pkexec, or
/// `SUDO_UID` after a sudo re-exec of our own (`by_sudo`, from
/// [`SUDO_REEXEC_FLAG`]). `None` for a dmcp deliberately started as root.
#[cfg(target_os = "linux")]
fn invoking_uid(pkexec_uid: Option<&str>, sudo_uid: Option<&str>, by_sudo: bool) -> Option<u32> {
    match pkexec_uid {
        Some(uid) => uid.parse().ok(),
        None if by_sudo => sudo_uid?.parse().ok(),
        None => None,
    }
}

#[cfg(target_os = "linux")]
fn reexec_uid(by_sudo: bool) -> Option<u32> {
    invoking_uid(
        std::env::var("PKEXEC_UID").ok().as_deref(),
        std::env::var("SUDO_UID").ok().as_deref(),
        by_sudo,
    )
}

/// True for the root half of a pkexec or sudo re-exec: euid 0 with the
/// caller's uid known. A dmcp deliberately started as root stays quiet.
#[cfg(target_os = "linux")]
pub fn is_elevation_reexec(euid: u32, invoking: Option<u32>) -> bool {
    euid == 0 && invoking.is_some()
}

/// Announce on stderr that the password was accepted. No-op unless this
/// process is the root half of a re-exec.
#[cfg(target_os = "linux")]
pub fn announce_auth_granted(by_sudo: bool) {
    let euid = nix::unistd::geteuid().as_raw();
    if is_elevation_reexec(euid, reexec_uid(by_sudo)) {
        eprintln!("{}", AUTH_GRANTED_NOTICE);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn announce_auth_granted(_by_sudo: bool) {}

/// Returns true if the path is under the system install directory.
pub fn is_system_scope(path: &Path, system_install_dir: &Path) -> bool {
    path.starts_with(system_install_dir)
}

fn current_exe_and_args() -> (std::path::PathBuf, Vec<String>) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Error: cannot get executable path: {}", e);
            process::exit(1);
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    (exe, args)
}

/// Whether this re-exec is a *delegated* elevation — one `dmcp serve`'s call
/// path spawned to reach root (marked by [`crate::call::DELEGATED_ENV`]). Only
/// then does the re-exec ask the now-root process to emit the elevation
/// sentinel; a human `dmcp call`/`install --system` must never see the marker.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_delegated_elevation() -> bool {
    std::env::var_os(crate::call::DELEGATED_ENV).is_some()
}

/// Restore HOME to the *invoking* user's home directory when this process is a
/// pkexec re-exec (issue #52). pkexec resets HOME to root's, but dmcp must read
/// the invoking user's `~/.config/mcp/sources.list`, not `/root/.config` — the
/// old `pkexec env HOME=… dmcp …` carried it across, but that made pkexec match
/// the action by `/usr/bin/env` rather than the annotated `/usr/bin/dmcp`, so
/// the custom action never fired. pkexec sets `PKEXEC_UID` to the caller; we map
/// it back to a home. Guarded on euid 0 **and** a present `PKEXEC_UID`, so a
/// dmcp deliberately started as root (a system-unit `dmcp serve`) is untouched.
/// Any failure to resolve leaves HOME as-is — never forced to `/root`.
///
/// MUST run as the first thing in `main`, before dotenvy, `Paths::resolve`, and
/// any thread or async runtime: `set_var` is process-global and only sound
/// while the process is single-threaded.
#[cfg(target_os = "linux")]
pub fn restore_invoking_user_home(by_sudo: bool) {
    let euid = nix::unistd::geteuid().as_raw();
    if let Some(home) = restored_home(euid, reexec_uid(by_sudo), home_for_uid) {
        std::env::set_var("HOME", home);
    }
}

/// The home directory for `uid` from the password database, or `None` if it has
/// no entry. Split from [`restore_invoking_user_home`] so the policy below is
/// testable without a real user database.
#[cfg(target_os = "linux")]
fn home_for_uid(uid: u32) -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.dir.to_string_lossy().into_owned())
}

/// Pure policy for [`restore_invoking_user_home`]: `Some(home)` to set HOME, or
/// `None` to leave it as-is. `None` unless euid is 0 (elevated) and the
/// invoking uid is known (see [`invoking_uid`]) and the lookup resolves it —
/// every other case falls back to leaving HOME untouched rather than guessing.
#[cfg(target_os = "linux")]
fn restored_home(
    euid: u32,
    invoking: Option<u32>,
    lookup: impl Fn(u32) -> Option<String>,
) -> Option<String> {
    if euid != 0 {
        return None;
    }
    lookup(invoking?)
}

/// Non-Linux hosts do not go through pkexec (`re_exec_elevated` uses sudo/
/// osascript, which preserve HOME), so there is nothing to restore.
#[cfg(not(target_os = "linux"))]
pub fn restore_invoking_user_home(_by_sudo: bool) {}

/// The askpass program to elevate with, when one is configured and says it can
/// answer now. Anything else (unset, missing, failing, not answering) is
/// `None`, which keeps pkexec.
#[cfg(target_os = "linux")]
fn ready_sudo_askpass() -> Option<std::ffi::OsString> {
    let askpass = std::env::var_os(SUDO_ASKPASS_ENV).filter(|p| !p.is_empty())?;
    let ready = process::Command::new(&askpass)
        .arg("--check")
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    ready.then_some(askpass)
}

/// The command that runs `program` as root: `sudo -A` with the askpass program
/// when one is ready, else pkexec. The bool is true for sudo.
#[cfg(target_os = "linux")]
fn elevating(program: &std::ffi::OsStr) -> (process::Command, bool) {
    match ready_sudo_askpass() {
        Some(askpass) => {
            let mut cmd = process::Command::new("sudo");
            cmd.env("SUDO_ASKPASS", askpass)
                .arg("-A")
                .arg("--")
                .arg(program);
            (cmd, true)
        }
        None => {
            let mut cmd = process::Command::new("pkexec");
            cmd.arg(program);
            (cmd, false)
        }
    }
}

/// Re-execute the current binary with pkexec for elevation.
/// Passes through all current args. Exits with the child's exit code.
///
/// pkexec maps a program to its polkit action by the executable it is asked to
/// exec, so it MUST exec dmcp directly. The old `pkexec env HOME=… dmcp …`
/// matched the action for `/usr/bin/env`, never the `/usr/bin/dmcp` the policy
/// annotates — so `org.jarvisos.dmcp.run-system-server` (its `allow_gui` and
/// `auth_admin_keep`) never fired and elevation silently fell back to the
/// generic exec action (issue #52). HOME is restored inside dmcp instead, from
/// `PKEXEC_UID` (see [`restore_invoking_user_home`]).
#[cfg(target_os = "linux")]
pub fn re_exec_elevated() -> ! {
    let (exe, args) = current_exe_and_args();

    let (mut cmd, by_sudo) = elevating(exe.as_os_str());
    eprintln!(
        "{}",
        if by_sudo {
            AUTH_WAITING_NOTICE_SUDO
        } else {
            AUTH_WAITING_NOTICE
        }
    );
    cmd.args(&args);
    if is_delegated_elevation() {
        cmd.arg(crate::call::ELEVATION_SENTINEL_FLAG);
    }
    if by_sudo {
        cmd.arg(SUDO_REEXEC_FLAG);
    }

    let status = cmd.status();

    match status {
        Ok(s) => process::exit(s.code().unwrap_or(1)),
        Err(e) if by_sudo => {
            eprintln!("Error: sudo failed: {}", e);
            process::exit(1);
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                eprintln!("Error: pkexec not found. Install polkit for system-scope operations:");
                eprintln!("  pacman -S polkit   (Arch Linux)");
                eprintln!("  apt install policykit-1  (Debian/Ubuntu)");
                eprintln!("Alternatively, run with sudo: sudo dmcp ...");
            } else {
                eprintln!("Error: pkexec failed: {}", e);
                eprintln!("Make sure polkit is installed. You can also try: sudo dmcp ...");
            }
            process::exit(1);
        }
    }
}

/// Re-execute the current binary elevated on macOS: `sudo` when attached to a
/// TTY (preserves stdio, so interactive/streaming commands like `dmcp run`
/// still work), otherwise `osascript` for a GUI authentication prompt.
#[cfg(target_os = "macos")]
pub fn re_exec_elevated() -> ! {
    use std::io::IsTerminal;

    let (exe, args) = current_exe_and_args();
    let home = std::env::var("HOME").unwrap_or_default();
    let delegated = is_delegated_elevation();

    let status = if std::io::stdin().is_terminal() {
        let mut cmd = process::Command::new("sudo");
        cmd.arg("-E").arg(&exe).args(&args);
        if delegated {
            cmd.arg(crate::call::ELEVATION_SENTINEL_FLAG);
        }
        cmd.status()
    } else {
        let mut parts = vec![shell_quote(&exe.to_string_lossy())];
        parts.extend(args.iter().map(|a| shell_quote(a)));
        if delegated {
            parts.push(shell_quote(crate::call::ELEVATION_SENTINEL_FLAG));
        }
        if !home.is_empty() {
            parts.insert(0, format!("HOME={}", shell_quote(&home)));
        }
        let shell_cmd = parts.join(" ");
        let script = format!(
            "do shell script \"{}\" with administrator privileges",
            applescript_escape(&shell_cmd)
        );
        process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .status()
    };

    match status {
        Ok(s) => process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("Error: elevation failed: {}", e);
            eprintln!("Make sure sudo/osascript is available, or try: sudo dmcp ...");
            process::exit(1);
        }
    }
}

#[cfg(target_os = "macos")]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(target_os = "macos")]
fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// System scope isn't wired up on this platform yet (v1 stub) - route to a
/// clear error instead of attempting a write that would just fail.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn re_exec_elevated() -> ! {
    eprintln!("Error: system-scope operations require elevation, which isn't supported on this platform yet.");
    eprintln!("Use user scope instead, or run from an already-elevated terminal.");
    process::exit(1);
}

/// Write a file that requires elevated privileges. On Linux, shells out to
/// `pkexec cp` when not already elevated. On other platforms this assumes the
/// process was already elevated via [`re_exec_elevated`] and writes directly.
pub fn write_file_elevated(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        if is_elevated() {
            return std::fs::write(path, contents);
        }
        let temp = std::env::temp_dir().join(format!("dmcp-write-{}.tmp", process::id()));
        std::fs::write(&temp, contents)?;
        let (mut cmd, by_sudo) = elevating(std::ffi::OsStr::new("cp"));
        let status = cmd.arg(&temp).arg(path).status()?;
        let _ = std::fs::remove_file(&temp);
        if !status.success() {
            let how = if by_sudo { "sudo" } else { "pkexec" };
            return Err(std::io::Error::other(format!("{how} cp failed")));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::fs::write(path, contents)
    }
}

/// Remove a directory tree that requires elevated privileges. Linux uses
/// `pkexec rm -rf` when not already elevated; other platforms assume the
/// process is already elevated (see [`write_file_elevated`]).
pub fn remove_dir_elevated(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        if is_elevated() {
            return std::fs::remove_dir_all(path);
        }
        let (mut cmd, by_sudo) = elevating(std::ffi::OsStr::new("rm"));
        let status = cmd.arg("-rf").arg(path).status()?;
        if !status.success() {
            let how = if by_sudo { "sudo" } else { "pkexec" };
            return Err(std::io::Error::other(format!("{how} rm -rf failed")));
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::fs::remove_dir_all(path)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn uid(pkexec: Option<&str>, sudo: Option<&str>, by_sudo: bool) -> Option<u32> {
        invoking_uid(pkexec, sudo, by_sudo)
    }

    /// An unelevated process never rewrites HOME, even if a PKEXEC_UID is set:
    /// the guard is euid 0 first.
    #[test]
    fn unelevated_process_leaves_home_untouched() {
        assert_eq!(
            restored_home(1000, uid(Some("1000"), None, false), |_| Some(
                "/home/alice".into()
            )),
            None
        );
    }

    /// A deliberately-root `dmcp serve` (started under systemd, not via pkexec)
    /// has no PKEXEC_UID, so its HOME is left exactly as configured.
    #[test]
    fn root_without_pkexec_uid_is_left_untouched() {
        assert_eq!(
            restored_home(0, uid(None, None, false), |_| Some("/home/alice".into())),
            None
        );
    }

    /// The pkexec case: euid 0 with a resolvable PKEXEC_UID restores the
    /// invoking user's home, so config resolution reads their sources.list.
    #[test]
    fn root_via_pkexec_restores_the_invoking_users_home() {
        let home = restored_home(0, uid(Some("1000"), None, false), |uid| {
            assert_eq!(uid, 1000);
            Some("/home/alice".into())
        });
        assert_eq!(home, Some("/home/alice".to_string()));
    }

    /// Our own sudo re-exec restores the asker's home from SUDO_UID, as pkexec
    /// does from PKEXEC_UID.
    #[test]
    fn root_via_our_sudo_reexec_restores_the_invoking_users_home() {
        let home = restored_home(0, uid(None, Some("1000"), true), |uid| {
            assert_eq!(uid, 1000);
            Some("/home/alice".into())
        });
        assert_eq!(home, Some("/home/alice".to_string()));
    }

    /// A SUDO_UID without our re-exec flag is someone's `sudo` further up the
    /// tree (a build script, `sudo bash`): root's HOME stays.
    #[test]
    fn a_sudo_uid_from_elsewhere_is_ignored() {
        assert_eq!(uid(None, Some("1000"), false), None);
        assert_eq!(
            restored_home(0, uid(None, Some("1000"), false), |_| Some(
                "/home/x".into()
            )),
            None
        );
    }

    /// A uid with no password-database entry falls back to leaving HOME as-is —
    /// never forced to /root.
    #[test]
    fn an_unresolvable_uid_leaves_home_untouched() {
        assert_eq!(
            restored_home(0, uid(Some("4242"), None, false), |_| None),
            None
        );
    }

    /// A non-numeric PKEXEC_UID cannot be trusted; leave HOME untouched rather
    /// than guess.
    #[test]
    fn a_non_numeric_pkexec_uid_leaves_home_untouched() {
        assert_eq!(uid(Some("not-a-uid"), None, false), None);
        assert_eq!(uid(Some("not-a-uid"), Some("1000"), true), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_the_root_half_of_a_reexec_announces() {
        assert!(is_elevation_reexec(0, uid(Some("1000"), None, false)));
        assert!(is_elevation_reexec(0, uid(None, Some("1000"), true)));
        assert!(!is_elevation_reexec(0, uid(None, Some("1000"), false)));
        assert!(!is_elevation_reexec(0, None));
        assert!(!is_elevation_reexec(1000, Some(1000)));
    }

    /// Unset, empty, missing or failing askpass programs keep pkexec.
    #[cfg(target_os = "linux")]
    #[test]
    fn pkexec_unless_the_askpass_program_says_it_is_ready() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("dmcp-askpass-test-{}", process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = |name: &str, code: i32| {
            let p = dir.join(name);
            std::fs::write(
                &p,
                format!("#!/bin/sh\n[ \"$1\" = --check ] && exit {code}\nexit 9\n"),
            )
            .unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let ready = script("ready", 0);
        let off = script("off", 1);

        std::env::remove_var(SUDO_ASKPASS_ENV);
        assert!(!elevating(std::ffi::OsStr::new("cp")).1);
        std::env::set_var(SUDO_ASKPASS_ENV, "");
        assert!(!elevating(std::ffi::OsStr::new("cp")).1);
        std::env::set_var(SUDO_ASKPASS_ENV, dir.join("missing"));
        assert!(!elevating(std::ffi::OsStr::new("cp")).1);
        std::env::set_var(SUDO_ASKPASS_ENV, &off);
        assert!(!elevating(std::ffi::OsStr::new("cp")).1);

        std::env::set_var(SUDO_ASKPASS_ENV, &ready);
        let (cmd, by_sudo) = elevating(std::ffi::OsStr::new("cp"));
        assert!(by_sudo);
        assert_eq!(cmd.get_program(), "sudo");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["-A", "--", "cp"]);
        let askpass = cmd
            .get_envs()
            .find(|(k, _)| *k == "SUDO_ASKPASS")
            .and_then(|(_, v)| v);
        assert_eq!(askpass, Some(ready.as_os_str()));
        std::env::remove_var(SUDO_ASKPASS_ENV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_notices_are_distinct_single_lines() {
        for n in [
            AUTH_WAITING_NOTICE,
            AUTH_WAITING_NOTICE_SUDO,
            AUTH_GRANTED_NOTICE,
        ] {
            assert!(!n.contains('\n'));
        }
        assert_ne!(AUTH_WAITING_NOTICE, AUTH_GRANTED_NOTICE);
        // Project-JARVIS's password_wait matcher looks for this phrase in both.
        for n in [AUTH_WAITING_NOTICE, AUTH_WAITING_NOTICE_SUDO] {
            assert!(n.contains("waiting for the administrator password"));
        }
    }
}
