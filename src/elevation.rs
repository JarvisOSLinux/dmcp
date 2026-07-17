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

/// Re-execute the current binary with pkexec for elevation.
/// Passes through all current args. Exits with the child's exit code.
/// Preserves HOME so the elevated process can read the invoking user's config (sources.list).
#[cfg(target_os = "linux")]
pub fn re_exec_with_pkexec() -> ! {
    let (exe, args) = current_exe_and_args();

    // Pass HOME so elevated process reads user's ~/.config/mcp/sources.list, not /root/.config
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());

    let status = process::Command::new("pkexec")
        .arg("env")
        .arg(format!("HOME={}", home))
        .arg(&exe)
        .args(&args)
        .status();

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

    let status = if std::io::stdin().is_terminal() {
        process::Command::new("sudo")
            .arg("-E")
            .arg(&exe)
            .args(&args)
            .status()
    } else {
        let mut parts = vec![shell_quote(&exe.to_string_lossy())];
        parts.extend(args.iter().map(|a| shell_quote(a)));
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
