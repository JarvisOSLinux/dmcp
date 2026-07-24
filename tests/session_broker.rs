//! Integration tests for the session-scoped persistent server broker (#36).
//!
//! These drive the real `dmcp` binary end to end: a `dmcp call --session`
//! auto-starts the broker, which spawns and keeps alive a fake stateful MCP
//! server (`tests/fixtures/fake_stateful_server.py`). The fake server's
//! `counter` (in-process integer) and `pid` (os.getpid) tools make "same
//! process across calls" and "no orphan left behind" directly observable.
//!
//! Every test skips gracefully when `python3` is not on PATH.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
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
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_stateful_server.py")
}

/// True if a signal can be sent to `pid` — i.e. the process is still alive.
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn kill9(pid: u32) {
    let _ = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// An isolated install tree + socket dir, removed on drop. All MCP_* paths and
/// XDG_RUNTIME_DIR are pinned inside it so tests never touch the real
/// environment and each gets its own broker.
struct TestEnv {
    root: PathBuf,
    ttl_secs: Option<u64>,
}

impl TestEnv {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("dmcp-broker-it-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(root.join("run")).unwrap();
        std::fs::create_dir_all(root.join("user/installed")).unwrap();
        std::fs::create_dir_all(root.join("system/installed")).unwrap();
        TestEnv {
            root,
            ttl_secs: None,
        }
    }

    fn with_ttl(mut self, secs: u64) -> Self {
        self.ttl_secs = Some(secs);
        self
    }

    fn scope_root(&self, scope: &str) -> PathBuf {
        match scope {
            "system" => self.root.join("system/installed"),
            _ => self.root.join("user/installed"),
        }
    }

    /// Install the fake server under `id` in the given scope. `stateful`
    /// controls whether the manifest declares `"stateful": true`.
    fn install(&self, id: &str, scope: &str, stateful: bool) {
        let base = self.scope_root(scope);
        let dir = base.join(id);
        std::fs::create_dir_all(&dir).unwrap();

        let fixture = fixture_path();
        let mut manifest = serde_json::json!({
            "id": id,
            "name": id,
            "version": "0.1.0",
            "transports": [{
                "type": "stdio",
                "command": "python3",
                "args": [fixture.to_string_lossy()],
            }],
            "installDir": dir.to_string_lossy(),
        });
        if stateful {
            manifest["stateful"] = serde_json::json!(true);
        }
        let manifest_path = dir.join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let index_path = base.join("index.json");
        let mut index: serde_json::Value = std::fs::read_to_string(&index_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({"servers": {}}));
        index["servers"][id] = serde_json::json!({
            "location": manifest_path.to_string_lossy(),
            "keywords": [],
        });
        std::fs::write(&index_path, serde_json::to_string_pretty(&index).unwrap()).unwrap();
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
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            // Bound how long a leftover broker lingers after a test.
            .env("DMCP_BROKER_IDLE_EXIT_SECS", "5")
            .env(
                "DMCP_SESSION_TTL_SECS",
                self.ttl_secs.unwrap_or(30).to_string(),
            )
            .current_dir(&self.root);
        c
    }

    fn call_session(&self, id: &str, tool: &str, session: &str) -> Output {
        self.cmd()
            .args(["call", id, tool, "--session", session])
            .output()
            .expect("run dmcp call --session")
    }

    fn call_oneshot(&self, id: &str, tool: &str) -> Output {
        self.cmd()
            .args(["call", id, tool])
            .output()
            .expect("run dmcp call")
    }

    fn session_close(&self, session: &str) {
        let _ = self.cmd().args(["session", "close", session]).output();
    }

    fn socket_path(&self) -> PathBuf {
        self.root.join("run/dmcp/broker.sock")
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stdout_trimmed(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ---------------------------------------------------------------------------

/// Criteria 1 & 2: the server process is spawned once and survives many
/// sequential calls — the counter increments monotonically and the pid is
/// stable, i.e. navigate-then-snapshot would see the navigated page.
#[test]
fn same_process_survives_many_sequential_calls() {
    if !python3_available() {
        eprintln!("skipping same_process_survives_many_sequential_calls: python3 not found");
        return;
    }
    let env = TestEnv::new();
    env.install("com.test.fake", "user", true);

    for expected in 1..=12 {
        let out = env.call_session("com.test.fake", "counter", "S1");
        assert!(
            out.status.success(),
            "call {} failed: {}",
            expected,
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            stdout_trimmed(&out),
            expected.to_string(),
            "counter must increment in the same live process"
        );
    }

    let pid_a = stdout_trimmed(&env.call_session("com.test.fake", "pid", "S1"));
    let pid_b = stdout_trimmed(&env.call_session("com.test.fake", "pid", "S1"));
    assert_eq!(
        pid_a, pid_b,
        "pid must be stable across calls in one session"
    );

    env.session_close("S1");
}

/// Criterion 8: a `--session` call's stdout/exit match the one-shot path on
/// success, so callers can't tell them apart. Both return the first counter
/// value ("1") from a freshly spawned server.
#[test]
fn session_output_shape_matches_oneshot() {
    if !python3_available() {
        eprintln!("skipping session_output_shape_matches_oneshot: python3 not found");
        return;
    }
    let env = TestEnv::new();
    env.install("com.test.fake", "user", true);

    let one_shot = env.call_oneshot("com.test.fake", "counter");
    let session = env.call_session("com.test.fake", "counter", "SHAPE");

    assert!(one_shot.status.success());
    assert!(session.status.success());
    assert_eq!(
        one_shot.stdout, session.stdout,
        "session and one-shot stdout must be byte-identical on success"
    );

    env.session_close("SHAPE");
}

/// Criterion 4 (explicit close): closing a session terminates its server child;
/// no orphan remains.
#[test]
fn explicit_close_kills_child_no_orphan() {
    if !python3_available() {
        eprintln!("skipping explicit_close_kills_child_no_orphan: python3 not found");
        return;
    }
    let env = TestEnv::new();
    env.install("com.test.fake", "user", true);

    let pid: u32 = stdout_trimmed(&env.call_session("com.test.fake", "pid", "S1"))
        .parse()
        .expect("pid tool returns an integer");
    assert!(pid_alive(pid), "server child should be alive after a call");

    env.session_close("S1");
    assert!(
        wait_until(Duration::from_secs(5), || !pid_alive(pid)),
        "server child {} must die after session close",
        pid
    );
}

/// Criterion 4 (TTL): a session idle beyond its TTL is swept and its child
/// terminated; no orphan remains.
#[test]
fn ttl_expiry_kills_child_no_orphan() {
    if !python3_available() {
        eprintln!("skipping ttl_expiry_kills_child_no_orphan: python3 not found");
        return;
    }
    let env = TestEnv::new().with_ttl(2);
    env.install("com.test.fake", "user", true);

    let pid: u32 = stdout_trimmed(&env.call_session("com.test.fake", "pid", "S1"))
        .parse()
        .expect("pid tool returns an integer");
    assert!(pid_alive(pid), "server child should be alive after a call");

    assert!(
        wait_until(Duration::from_secs(15), || !pid_alive(pid)),
        "server child {} must die after the idle TTL expires",
        pid
    );
}

/// Criterion 6: concurrent calls to one `(id, session)` serialize — ten
/// simultaneous `counter` calls return the values 1..=10 with no duplicates or
/// lost updates.
#[test]
fn concurrent_calls_to_one_session_serialize() {
    if !python3_available() {
        eprintln!("skipping concurrent_calls_to_one_session_serialize: python3 not found");
        return;
    }
    let env = TestEnv::new();
    env.install("com.test.fake", "user", true);

    // Establish the session first so all ten racers hit an already-live server
    // rather than racing the broker auto-start.
    assert_eq!(
        stdout_trimmed(&env.call_session("com.test.fake", "counter", "C")),
        "1"
    );

    let values: Vec<i64> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..10)
            .map(|_| {
                scope.spawn(|| stdout_trimmed(&env.call_session("com.test.fake", "counter", "C")))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap().parse::<i64>().expect("counter integer"))
            .collect()
    });

    let mut sorted = values.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        (2..=11).collect::<Vec<_>>(),
        "serialized concurrent calls must yield each counter value exactly once (got {:?})",
        values
    );

    env.session_close("C");
}

/// Criterion 7: when the session's server dies under the broker, the next call
/// exits non-zero with a clear "session lost" error — never an empty success.
#[test]
fn session_loss_reports_clear_error_and_nonzero_exit() {
    if !python3_available() {
        eprintln!("skipping session_loss_reports_clear_error_and_nonzero_exit: python3 not found");
        return;
    }
    let env = TestEnv::new();
    env.install("com.test.fake", "user", true);

    let pid: u32 = stdout_trimmed(&env.call_session("com.test.fake", "pid", "S2"))
        .parse()
        .expect("pid tool returns an integer");
    kill9(pid);
    assert!(
        wait_until(Duration::from_secs(5), || !pid_alive(pid)),
        "child should be dead before the follow-up call"
    );

    let out = env.call_session("com.test.fake", "counter", "S2");
    assert!(
        !out.status.success(),
        "a lost session must not report success"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("session lost"),
        "error must name the lost session, got: {}",
        stderr
    );
}

/// Criterion 9: a system-scope server rejects `--session` with a clear error
/// (and does not even start a broker).
#[test]
fn system_scope_session_is_rejected() {
    if !python3_available() {
        eprintln!("skipping system_scope_session_is_rejected: python3 not found");
        return;
    }
    let env = TestEnv::new();
    env.install("com.test.sysfake", "system", true);

    let out = env.call_session("com.test.sysfake", "pid", "S3");
    assert!(!out.status.success(), "system-scope --session must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("system-scope"),
        "error must explain the system-scope restriction, got: {}",
        stderr
    );
    assert!(
        !env.socket_path().exists(),
        "a rejected gate must not auto-start the broker"
    );
}

/// Gating: a stateless server rejects `--session` — the broker is only for
/// servers that actually hold cross-call state.
#[test]
fn stateless_server_session_is_rejected() {
    if !python3_available() {
        eprintln!("skipping stateless_server_session_is_rejected: python3 not found");
        return;
    }
    let env = TestEnv::new();
    env.install("com.test.stateless", "user", false);

    let out = env.call_session("com.test.stateless", "pid", "S4");
    assert!(!out.status.success(), "stateless --session must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("stateful"),
        "error must explain the stateful requirement, got: {}",
        stderr
    );
    assert!(
        !env.socket_path().exists(),
        "a rejected gate must not auto-start the broker"
    );
}

/// A stateless server without `--session` uses the untouched one-shot path:
/// every call is a fresh process, so the counter is always 1.
#[test]
fn oneshot_path_is_unaffected_by_session_support() {
    if !python3_available() {
        eprintln!("skipping oneshot_path_is_unaffected_by_session_support: python3 not found");
        return;
    }
    let env = TestEnv::new();
    env.install("com.test.stateless", "user", false);

    for _ in 0..3 {
        let out = env.call_oneshot("com.test.stateless", "counter");
        assert!(out.status.success());
        assert_eq!(
            stdout_trimmed(&out),
            "1",
            "one-shot calls must each spawn a fresh process"
        );
    }
    assert!(
        !env.socket_path().exists(),
        "the one-shot path must never start the broker"
    );
}
