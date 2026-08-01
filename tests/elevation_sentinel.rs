//! End-to-end wiring of the delegated-elevation sentinel (issue #51).
//!
//! pkexec cannot be driven from a test suite, so these do not exercise a real
//! elevation. They pin the one piece that lives in `main`: the internal
//! `--dmcp-internal-elevation-authenticated` flag a delegated re-exec appends is
//! stripped before clap, makes the now-root process emit the sentinel line the
//! parent's relay watches for, and — crucially — never reaches clap as an
//! unknown argument. The relay's own stripping and the deadline logic are unit
//! tested in `src/call.rs`.

use std::process::Command;

fn dmcp_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dmcp")
}

/// The marker the delegated child emits once past authentication. Kept in sync
/// with `call::ELEVATION_SENTINEL` by eye — a drift would fail these tests.
const SENTINEL: &str = "\u{1}dmcp:elevation-authenticated\u{1}\n";
const FLAG: &str = "--dmcp-internal-elevation-authenticated";

/// With the flag present, the process is the now-root end of a delegated
/// elevation: it emits the sentinel to stderr, and the flag is consumed by
/// `main` rather than choking clap — the wrapped command still runs.
#[test]
fn the_flag_emits_the_sentinel_and_never_reaches_clap() {
    let out = Command::new(dmcp_bin())
        .args([FLAG, "paths"])
        .env("MCP_USER_INSTALL_DIR", "/tmp/dmcp-sentinel-it")
        .output()
        .expect("run dmcp");

    assert!(
        out.status.success(),
        "the wrapped command must still run; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(SENTINEL),
        "the delegated process must emit the sentinel; stderr bytes: {:?}",
        stderr
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("User install dir:"),
        "the `paths` command must have run, got: {}",
        stdout
    );
}

/// Without the flag, no sentinel is emitted — an ordinary invocation is
/// byte-for-byte unchanged, so a human `dmcp` never sees the internal marker.
#[test]
fn without_the_flag_no_sentinel_is_emitted() {
    let out = Command::new(dmcp_bin())
        .args(["paths"])
        .env("MCP_USER_INSTALL_DIR", "/tmp/dmcp-sentinel-it")
        .output()
        .expect("run dmcp");

    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains('\u{1}'),
        "an ordinary invocation must not emit the sentinel; stderr: {:?}",
        stderr
    );
}
