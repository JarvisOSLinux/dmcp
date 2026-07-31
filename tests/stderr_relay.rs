//! Integration tests for the live stderr relay during a call (#49).
//!
//! These drive the real `dmcp` binary end to end against
//! `tests/fixtures/fake_logging_server.py`. The point is LIVENESS, not mere
//! presence: the fake writes a newline-less marker to stderr mid-`tools/call`
//! and then blocks until a sentinel file appears. The test creates the
//! sentinel only after the marker shows up on dmcp's stderr, so the call can
//! complete only if stderr flows through while the call is still in flight —
//! an end-buffered relay fails the bounded wait instead of hanging the suite.
//!
//! Every test skips gracefully when `python3` is not on PATH.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn dmcp_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dmcp")
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_logging_server.py")
}

/// Poll until `f()` holds or the deadline passes. Returns whether it held.
fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// An isolated install tree, removed on drop. All MCP_* paths are pinned
/// inside it so tests never touch the real environment.
struct TestEnv {
    root: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("dmcp-stderr-it-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(root.join("user/installed")).unwrap();
        std::fs::create_dir_all(root.join("system/installed")).unwrap();
        TestEnv { root }
    }

    /// Install the fake logging server under `id` in user scope.
    fn install(&self, id: &str) {
        let base = self.root.join("user/installed");
        let dir = base.join(id);
        std::fs::create_dir_all(&dir).unwrap();

        let manifest = serde_json::json!({
            "id": id,
            "name": id,
            "version": "0.1.0",
            "transports": [{
                "type": "stdio",
                "command": "python3",
                "args": [fixture_path().to_string_lossy()],
            }],
            "installDir": dir.to_string_lossy(),
        });
        let manifest_path = dir.join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let index = serde_json::json!({
            "servers": {
                id: {
                    "location": manifest_path.to_string_lossy(),
                    "keywords": [],
                }
            }
        });
        std::fs::write(
            base.join("index.json"),
            serde_json::to_string_pretty(&index).unwrap(),
        )
        .unwrap();
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(dmcp_bin());
        c.env("MCP_USER_INSTALL_DIR", self.root.join("user/installed"))
            .env("MCP_SYSTEM_INSTALL_DIR", self.root.join("system/installed"))
            .env("MCP_USER_SOURCES_PATH", self.root.join("user/sources.list"))
            .env(
                "MCP_SYSTEM_SOURCES_PATH",
                self.root.join("system/sources.list"),
            )
            .env("MCP_VECTOR_INDEX_DIR", self.root.join("vector"))
            .current_dir(&self.root);
        c
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Read a pipe byte-wise into a shared buffer from a thread, so the main
/// thread can watch it grow while the child is still running.
fn tail_into(pipe: impl Read + Send + 'static, buf: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let mut chunk = [0u8; 1024];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.lock().unwrap().extend_from_slice(&chunk[..n]),
            }
        }
    });
}

fn lossy(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&buf.lock().unwrap()).to_string()
}

/// The heart of #49: the server's stderr must reach dmcp's own stderr while
/// the tool call is still in flight. The server blocks until the sentinel this
/// test only creates after seeing the marker — with an end-buffered relay the
/// marker never shows, the sentinel is never written, and the bounded waits
/// below fail with a message instead of a hang.
#[test]
fn stderr_reaches_the_caller_while_the_call_is_in_flight() {
    if !python3_available() {
        eprintln!(
            "skipping stderr_reaches_the_caller_while_the_call_is_in_flight: python3 not found"
        );
        return;
    }
    let env = TestEnv::new();
    env.install("com.test.logging");

    let sentinel = env.root.join("sentinel");
    let args = serde_json::json!({ "sentinel": sentinel.to_string_lossy() }).to_string();
    let mut child = env
        .cmd()
        .args(["call", "com.test.logging", "blocking_log", "--args", &args])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dmcp call");

    let seen_err = Arc::new(Mutex::new(Vec::new()));
    let seen_out = Arc::new(Mutex::new(Vec::new()));
    tail_into(child.stderr.take().expect("stderr"), seen_err.clone());
    tail_into(child.stdout.take().expect("stdout"), seen_out.clone());

    // The marker has no trailing newline, so seeing it here proves raw-chunk
    // relay, not line-by-line flushing.
    let marker_seen = wait_until(Duration::from_secs(20), || {
        lossy(&seen_err).contains("MARKER_LIVE: Proceed? [Y/n] ")
    });
    if !marker_seen {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "server stderr did not reach dmcp's stderr while the call was in flight \
             (relay is end-buffered?); stderr so far: {:?}",
            lossy(&seen_err)
        );
    }
    assert!(
        !lossy(&seen_out).contains("MARKER_LIVE"),
        "the relay must never touch stdout (the JSON-RPC wire / result stream)"
    );

    std::fs::write(&sentinel, b"go").expect("create sentinel");

    let exited = wait_until(Duration::from_secs(20), || {
        child.try_wait().expect("try_wait").is_some()
    });
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
        panic!("dmcp call did not complete after the sentinel appeared");
    }
    let status = child.wait().expect("wait");
    assert!(
        status.success(),
        "the unblocked call must succeed; stderr: {}",
        lossy(&seen_err)
    );
    assert!(
        wait_until(Duration::from_secs(5), || lossy(&seen_out)
            .contains("unblocked")),
        "the tool result must arrive on stdout untouched, got: {:?}",
        lossy(&seen_out)
    );
}

/// Relay-and-buffer, not relay-instead-of-buffer: when the server dies
/// mid-call, the retained stderr still lands in the error text a caller
/// actually reads, alongside having been streamed live.
#[test]
fn a_failed_call_still_carries_the_servers_stderr_in_its_error() {
    if !python3_available() {
        eprintln!(
            "skipping a_failed_call_still_carries_the_servers_stderr_in_its_error: python3 not found"
        );
        return;
    }
    let env = TestEnv::new();
    env.install("com.test.logging");

    let out: Output = env
        .cmd()
        .args(["call", "com.test.logging", "explode"])
        .output()
        .expect("run dmcp call");

    assert!(
        !out.status.success(),
        "a dead server must not report success"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("server stderr:"),
        "the error text must attribute the retained stderr, got: {}",
        stderr
    );
    assert!(
        stderr.contains("FAKE_TRACEBACK: something terrible happened"),
        "the error text must carry what the server said, got: {}",
        stderr
    );
}

/// The retained failure detail is a bounded TAIL: a server that floods
/// hundreds of KiB to stderr and then dies must yield an error text carrying
/// the end of the flood (where the failure reason lives) behind an announced
/// truncation — never the whole flood, because `dmcp serve` delivers that
/// text verbatim to the LLM. The live relay stays uncapped: the entire flood,
/// first marker included, still streams through to dmcp's own stderr.
#[test]
fn a_flooding_server_yields_a_bounded_tail_in_the_error() {
    if !python3_available() {
        eprintln!(
            "skipping a_flooding_server_yields_a_bounded_tail_in_the_error: python3 not found"
        );
        return;
    }
    let env = TestEnv::new();
    env.install("com.test.logging");

    let out: Output = env
        .cmd()
        .args(["call", "com.test.logging", "flood_and_explode"])
        .output()
        .expect("run dmcp call");

    assert!(
        !out.status.success(),
        "a dead server must not report success"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);

    // The truncation marker splits the live stream from the error text: the
    // server never writes it, so its one occurrence is the retained detail.
    let marker = "[stderr truncated to last 64 KiB]";
    let (live, detail) = stderr.split_once(marker).unwrap_or_else(|| {
        panic!(
            "the error text must announce the truncation with {:?}; stderr len {}, tail: {:?}",
            marker,
            stderr.len(),
            &stderr[stderr.len().saturating_sub(512)..]
        )
    });

    assert!(
        live.contains("FLOOD_FIRST_MARKER") && live.contains("FLOOD_LAST_MARKER"),
        "the live relay must stream the WHOLE flood through, uncapped"
    );
    assert!(
        live.contains("server stderr:"),
        "the error text must attribute the retained stderr, got: {}",
        &live[live.len().saturating_sub(512)..]
    );

    assert!(
        detail.contains("FLOOD_LAST_MARKER"),
        "the retained detail must carry the tail of the flood"
    );
    assert!(
        !detail.contains("FLOOD_FIRST_MARKER"),
        "the retained detail must have dropped the head of the flood"
    );
    assert!(
        detail.len() <= 70 * 1024,
        "the retained detail must stay near the 64 KiB cap, got {} bytes",
        detail.len()
    );
}
