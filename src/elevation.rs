//! Privilege elevation for system-scope operations.
//!
//! Linux: pkexec/polkit. macOS: sudo (TTY) or osascript (GUI). Other platforms
//! don't support system scope yet and print guidance instead of attempting it.

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
pub fn restore_invoking_user_home() {
    let euid = nix::unistd::geteuid().as_raw();
    let pkexec_uid = std::env::var("PKEXEC_UID").ok();
    if let Some(home) = restored_home(euid, pkexec_uid.as_deref(), home_for_uid) {
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
/// `None` to leave it as-is. `None` unless euid is 0 (elevated) and `PKEXEC_UID`
/// both is present and parses to a uid the lookup can resolve — every other
/// case falls back to leaving HOME untouched rather than guessing.
#[cfg(target_os = "linux")]
fn restored_home(
    euid: u32,
    pkexec_uid: Option<&str>,
    lookup: impl Fn(u32) -> Option<String>,
) -> Option<String> {
    if euid != 0 {
        return None;
    }
    let uid: u32 = pkexec_uid?.parse().ok()?;
    lookup(uid)
}

/// Non-Linux hosts do not go through pkexec (`re_exec_with_pkexec` uses sudo/
/// osascript, which preserve HOME), so there is nothing to restore.
#[cfg(not(target_os = "linux"))]
pub fn restore_invoking_user_home() {}

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
pub fn re_exec_with_pkexec() -> ! {
    let (exe, args) = current_exe_and_args();

    let mut cmd = process::Command::new("pkexec");
    cmd.arg(&exe).args(&args);
    if is_delegated_elevation() {
        cmd.arg(crate::call::ELEVATION_SENTINEL_FLAG);
    }

    let status = cmd.status();

    match status {
        Ok(s) => process::exit(s.code().unwrap_or(1)),
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
pub fn re_exec_with_pkexec() -> ! {
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
pub fn re_exec_with_pkexec() -> ! {
    eprintln!("Error: system-scope operations require elevation, which isn't supported on this platform yet.");
    eprintln!("Use user scope instead, or run from an already-elevated terminal.");
    process::exit(1);
}

/// Write a file that requires elevated privileges. On Linux, shells out to
/// `pkexec cp` when not already elevated. On other platforms this assumes the
/// process was already elevated via [`re_exec_with_pkexec`] and writes directly.
pub fn write_file_elevated(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        if is_elevated() {
            return std::fs::write(path, contents);
        }
        let temp = std::env::temp_dir().join(format!("dmcp-write-{}.tmp", process::id()));
        std::fs::write(&temp, contents)?;
        let status = process::Command::new("pkexec")
            .arg("cp")
            .arg(&temp)
            .arg(path)
            .status()?;
        let _ = std::fs::remove_file(&temp);
        if !status.success() {
            return Err(std::io::Error::other("pkexec cp failed"));
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
        let status = process::Command::new("pkexec")
            .arg("rm")
            .arg("-rf")
            .arg(path)
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other("pkexec rm -rf failed"));
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

    /// An unelevated process never rewrites HOME, even if a PKEXEC_UID is set:
    /// the guard is euid 0 first.
    #[test]
    fn unelevated_process_leaves_home_untouched() {
        assert_eq!(
            restored_home(1000, Some("1000"), |_| Some("/home/alice".into())),
            None
        );
    }

    /// A deliberately-root `dmcp serve` (started under systemd, not via pkexec)
    /// has no PKEXEC_UID, so its HOME is left exactly as configured.
    #[test]
    fn root_without_pkexec_uid_is_left_untouched() {
        assert_eq!(restored_home(0, None, |_| Some("/home/alice".into())), None);
    }

    /// The pkexec case: euid 0 with a resolvable PKEXEC_UID restores the
    /// invoking user's home, so config resolution reads their sources.list.
    #[test]
    fn root_via_pkexec_restores_the_invoking_users_home() {
        let home = restored_home(0, Some("1000"), |uid| {
            assert_eq!(uid, 1000);
            Some("/home/alice".into())
        });
        assert_eq!(home, Some("/home/alice".to_string()));
    }

    /// A uid with no password-database entry falls back to leaving HOME as-is —
    /// never forced to /root.
    #[test]
    fn an_unresolvable_uid_leaves_home_untouched() {
        assert_eq!(restored_home(0, Some("4242"), |_| None), None);
    }

    /// A non-numeric PKEXEC_UID cannot be trusted; leave HOME untouched rather
    /// than guess.
    #[test]
    fn a_non_numeric_pkexec_uid_leaves_home_untouched() {
        assert_eq!(
            restored_home(0, Some("not-a-uid"), |_| Some("/home/x".into())),
            None
        );
    }
}
