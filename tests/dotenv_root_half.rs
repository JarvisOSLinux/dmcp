//! A `.env` in the working directory must not reach the root half of an
//! elevation re-exec. `sudo` keeps the caller's working directory, so a `.env`
//! planted there would set `LD_PRELOAD` or `PATH` for root and for every
//! server root spawns.
//!
//! Real elevation cannot be driven from a test suite. A user namespace
//! (`unshare --map-root-user`) gives dmcp euid 0 without privilege, and the
//! re-exec markers (`PKEXEC_UID`, or `SUDO_UID` with the internal sudo flag)
//! make it the root half.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;

const FROM_DOTENV: &str = "/tmp/dmcp-dotenv-it-from-dotenv";
const SUDO_REEXEC_FLAG: &str = "--dmcp-internal-sudo-reexec";

fn dir_with_dotenv(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dmcp-dotenv-it-{}-{}", std::process::id(), name));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".env"),
        format!("MCP_USER_INSTALL_DIR={FROM_DOTENV}\n"),
    )
    .unwrap();
    dir
}

/// `dmcp paths` run from `cwd`, through `wrapper` (empty for none), with only
/// the given re-exec markers set. Returns stdout.
fn paths(cwd: &Path, wrapper: &[&str], markers: &[(&str, &str)], args: &[&str]) -> String {
    let dmcp = env!("CARGO_BIN_EXE_dmcp");
    let mut cmd = match wrapper.split_first() {
        Some((program, rest)) => {
            let mut cmd = Command::new(program);
            cmd.args(rest).arg(dmcp);
            cmd
        }
        None => Command::new(dmcp),
    };
    cmd.args(args)
        .arg("paths")
        .current_dir(cwd)
        .env_remove("MCP_USER_INSTALL_DIR")
        .env_remove("PKEXEC_UID")
        .env_remove("SUDO_UID")
        .envs(markers.iter().copied());
    let out = cmd.output().expect("run dmcp");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_user_run_loads_dotenv_from_the_working_directory() {
    let dir = dir_with_dotenv("user");
    assert!(paths(&dir, &[], &[], &[]).contains(FROM_DOTENV));
}

#[test]
fn the_root_half_of_a_reexec_ignores_dotenv_in_the_working_directory() {
    const AS_ROOT: &[&str] = &["unshare", "--map-root-user", "--"];
    let userns = Command::new(AS_ROOT[0])
        .args(&AS_ROOT[1..])
        .arg("true")
        .status()
        .is_ok_and(|s| s.success());
    if !userns {
        eprintln!("skipped: unprivileged user namespaces are unavailable here");
        return;
    }
    let dir = dir_with_dotenv("root");

    // Control: root with no re-exec marker (a system-unit `dmcp serve`) still
    // reads .env, so the namespace itself is not what hides it below.
    assert!(paths(&dir, AS_ROOT, &[], &[]).contains(FROM_DOTENV));

    let pkexec = paths(&dir, AS_ROOT, &[("PKEXEC_UID", "1000")], &[]);
    assert!(!pkexec.contains(FROM_DOTENV), "pkexec root half: {pkexec}");

    let sudo = paths(&dir, AS_ROOT, &[("SUDO_UID", "1000")], &[SUDO_REEXEC_FLAG]);
    assert!(!sudo.contains(FROM_DOTENV), "sudo root half: {sudo}");
}
