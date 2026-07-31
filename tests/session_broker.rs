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

/// A stdio transport that launches the fake server, optionally declared for a
/// specific platform.
fn fake_server_transport(platforms: Option<&[&str]>) -> serde_json::Value {
    let mut transport = serde_json::json!({
        "type": "stdio",
        "command": "python3",
        "args": [fixture_path().to_string_lossy()],
    });
    if let Some(p) = platforms {
        transport["platforms"] = serde_json::json!(p);
    }
    transport
}

/// A platform that is never this host, so a transport declared for it is one
/// dmcp must skip whichever OS the suite runs on.
fn foreign_platform() -> &'static str {
    if dmcp::host_platform() == "linux" {
        "windows"
    } else {
        "linux"
    }
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
    hang_on_init: bool,
    spawn_timeout_secs: Option<u64>,
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
            hang_on_init: false,
            spawn_timeout_secs: None,
        }
    }

    fn with_ttl(mut self, secs: u64) -> Self {
        self.ttl_secs = Some(secs);
        self
    }

    /// Make the fake server block forever on `initialize` (via the fixture's
    /// `FAKE_HANG_ON_INIT` env, inherited broker → child).
    fn with_hang_on_init(mut self) -> Self {
        self.hang_on_init = true;
        self
    }

    fn with_spawn_timeout(mut self, secs: u64) -> Self {
        self.spawn_timeout_secs = Some(secs);
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
        self.install_with_transports(
            id,
            scope,
            stateful,
            serde_json::json!([fake_server_transport(None)]),
        );
    }

    /// Same, with the manifest's `transports` array supplied verbatim — how a
    /// per-platform launch line is put in front of dmcp.
    fn install_with_transports(
        &self,
        id: &str,
        scope: &str,
        stateful: bool,
        transports: serde_json::Value,
    ) {
        let base = self.scope_root(scope);
        let dir = base.join(id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut manifest = serde_json::json!({
            "id": id,
            "name": id,
            "version": "0.1.0",
            "transports": transports,
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
        if self.hang_on_init {
            c.env("FAKE_HANG_ON_INIT", "1");
        }
        if let Some(secs) = self.spawn_timeout_secs {
            c.env("DMCP_SESSION_SPAWN_TIMEOUT_SECS", secs.to_string());
        }
        c
    }

    fn call_session(&self, id: &str, tool: &str, session: &str) -> Output {
        self.cmd()
            .args(["call", id, tool, "--session", session])
            .output()
            .expect("run dmcp call --session")
    }

    /// `call_session` with tool arguments, for a server whose behavior varies
    /// by input (the eliciting fake's round count).
    fn call_session_args(&self, id: &str, tool: &str, args: Option<&str>, session: &str) -> Output {
        let mut c = self.cmd();
        c.args(["call", id, tool, "--session", session]);
        if let Some(a) = args {
            c.args(["--args", a]);
        }
        c.output().expect("run dmcp call --session --args")
    }

    fn call_oneshot(&self, id: &str, tool: &str) -> Output {
        self.cmd()
            .args(["call", id, tool])
            .output()
            .expect("run dmcp call")
    }

    /// Like `call_session`, but fails loudly if the call does not return within
    /// `max` instead of hanging the whole test binary. Proves the broker bounds
    /// a server that never completes `initialize`.
    fn call_session_bounded(&self, id: &str, tool: &str, session: &str, max: Duration) -> Output {
        let mut child = self
            .cmd()
            .args(["call", id, tool, "--session", session])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn dmcp call --session");
        let deadline = Instant::now() + max;
        loop {
            if child.try_wait().expect("try_wait").is_some() {
                return child.wait_with_output().expect("collect output");
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "dmcp call --session did not return within {:?}; \
                     spawn/initialize timeout not enforced",
                    max
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Send a raw NDJSON request straight to the broker socket, bypassing the
    /// thin client's gate. Used to prove the broker re-enforces the gate itself.
    fn raw_request(&self, line: &str) -> serde_json::Value {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        let stream = UnixStream::connect(self.socket_path()).expect("connect broker socket");
        let mut w = stream.try_clone().expect("clone socket");
        w.write_all(line.as_bytes()).expect("write request");
        w.write_all(b"\n").expect("write newline");
        w.flush().ok();
        let mut r = BufReader::new(stream);
        let mut resp = String::new();
        r.read_line(&mut resp).expect("read response");
        serde_json::from_str(resp.trim()).expect("valid json response")
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

/// Finding #1: a JSON-RPC error from a *still-live* child (an unknown tool) is a
/// recoverable protocol error, not session loss. The session survives — stable
/// pid, monotonic counter — and the failure is reported as a tool/protocol
/// error, never "session lost" (which would tear a real browser down to
/// about:blank, the exact #36 regression).
#[test]
fn protocol_error_keeps_session_alive() {
    if !python3_available() {
        eprintln!("skipping protocol_error_keeps_session_alive: python3 not found");
        return;
    }
    let env = TestEnv::new();
    env.install("com.test.fake", "user", true);

    assert_eq!(
        stdout_trimmed(&env.call_session("com.test.fake", "counter", "S1")),
        "1"
    );
    let pid_before = stdout_trimmed(&env.call_session("com.test.fake", "pid", "S1"));

    let bad = env.call_session("com.test.fake", "does_not_exist", "S1");
    assert!(!bad.status.success(), "an unknown tool must fail");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&bad.stdout),
        String::from_utf8_lossy(&bad.stderr)
    );
    assert!(
        !combined.contains("session lost"),
        "a protocol error must not be reported as session loss, got: {}",
        combined
    );

    // The child survived with its in-process state intact: same pid, and the
    // counter continues from 1 (was not reset by a fresh spawn).
    let pid_after = stdout_trimmed(&env.call_session("com.test.fake", "pid", "S1"));
    assert_eq!(
        pid_before, pid_after,
        "the child must survive a protocol error (no fresh spawn / about:blank reset)"
    );
    assert_eq!(
        stdout_trimmed(&env.call_session("com.test.fake", "counter", "S1")),
        "2",
        "the in-process counter must persist across a protocol error"
    );

    env.session_close("S1");
}

/// Finding #2: a server that blocks before completing `initialize` must not
/// wedge the session forever. The broker bounds spawn+initialize, so the call
/// returns promptly with a clear start failure instead of hanging (and rmcp's
/// ChildWithCleanup reaps the stuck child when the timed-out serve future is
/// dropped).
#[test]
fn spawn_hang_times_out_without_wedging() {
    if !python3_available() {
        eprintln!("skipping spawn_hang_times_out_without_wedging: python3 not found");
        return;
    }
    let env = TestEnv::new().with_hang_on_init().with_spawn_timeout(2);
    env.install("com.test.hang", "user", true);

    let out = env.call_session_bounded("com.test.hang", "pid", "S1", Duration::from_secs(20));
    assert!(
        !out.status.success(),
        "a server that never initializes must not report success"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("failed to start") || stderr.contains("did not initialize"),
        "error must name a startup failure, got: {}",
        stderr
    );
}

/// Finding #3: the broker actually idle-exits (removes its socket) after its
/// idle window, proving the shutdown signal reaches the accept loop. A lost
/// wakeup would strand the broker with a live socket forever.
#[test]
fn broker_idle_exits_and_removes_socket() {
    if !python3_available() {
        eprintln!("skipping broker_idle_exits_and_removes_socket: python3 not found");
        return;
    }
    let env = TestEnv::new().with_ttl(2);
    env.install("com.test.fake", "user", true);

    assert!(
        env.call_session("com.test.fake", "pid", "S1")
            .status
            .success(),
        "the establishing call should succeed"
    );
    assert!(
        env.socket_path().exists(),
        "the broker socket should exist after a call"
    );

    // TTL (2s) sweeps the idle session; the broker then idle-exits
    // DMCP_BROKER_IDLE_EXIT_SECS (5s) after going empty.
    assert!(
        wait_until(Duration::from_secs(30), || !env.socket_path().exists()),
        "the broker must idle-exit and remove its socket"
    );
}

/// Finding #4: a broker directory that is not private to the current uid is
/// refused before any socket inside it is trusted, closing the /tmp cross-user
/// socket-hijack window. A test can't chown to a foreign uid, so this exercises
/// the mode arm of the same ownership+mode check (a world/group-accessible dir).
#[test]
fn world_accessible_broker_dir_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    let env = TestEnv::new();

    // Pre-create the broker dir group/other-accessible, as an attacker planting
    // a socket in a shared /tmp would leave it.
    let dir = env.root.join("run/dmcp");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();

    // `session list` would otherwise connect to whatever socket answers here;
    // it must refuse the unsafe dir first.
    let out = env.cmd().args(["session", "list"]).output().unwrap();
    assert!(
        !out.status.success(),
        "session list must refuse an unsafe broker dir"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing to use") && stderr.contains("0700"),
        "error must explain the unsafe-dir refusal, got: {}",
        stderr
    );
}

/// Finding #5: the broker re-enforces the stateful + user-scope gate itself, so
/// a raw socket request for a system-scope or stateless id is refused even
/// though it bypasses the thin client's gate — the socket is the trust boundary.
#[test]
fn broker_reenforces_gate_on_raw_socket_request() {
    if !python3_available() {
        eprintln!("skipping broker_reenforces_gate_on_raw_socket_request: python3 not found");
        return;
    }
    let env = TestEnv::new();
    env.install("com.test.fake", "user", true); // stateful user: starts the broker
    env.install("com.test.sysfake", "system", true); // system scope
    env.install("com.test.stateless", "user", false); // stateless user

    // Start the broker with a legitimate call.
    assert_eq!(
        stdout_trimmed(&env.call_session("com.test.fake", "counter", "S1")),
        "1"
    );

    // A system-scope id sent raw over the socket must be refused by the broker.
    let sys =
        env.raw_request(r#"{"op":"call","id":"com.test.sysfake","session":"S1","tool":"pid"}"#);
    assert_eq!(
        sys["ok"],
        serde_json::json!(false),
        "broker must refuse a system-scope session, got: {}",
        sys
    );
    assert!(
        sys["error"].as_str().unwrap_or("").contains("system-scope"),
        "broker gate error must explain the scope restriction, got: {}",
        sys
    );

    // A stateless id sent raw over the socket must be refused too.
    let stateless = env
        .raw_request(r#"{"op":"call","id":"com.test.stateless","session":"S1","tool":"counter"}"#);
    assert_eq!(
        stateless["ok"],
        serde_json::json!(false),
        "broker must refuse a stateless session, got: {}",
        stateless
    );
    assert!(
        stateless["error"]
            .as_str()
            .unwrap_or("")
            .contains("stateful"),
        "broker gate error must explain the stateful requirement, got: {}",
        stateless
    );

    env.session_close("S1");
}

// ---------------------------------------------------------------------------
// Per-transport platform selection (#42)
// ---------------------------------------------------------------------------

/// A manifest may carry one launch line per platform. Every spawn site has to
/// pick the one for this host: the one-shot call, the session broker, and
/// `tools`. Here the first transport is declared for another platform and names
/// a command that does not exist, so anything that still takes entry zero fails
/// with a spawn error instead of returning the fake server's output.
#[test]
fn every_spawn_site_launches_the_transport_declared_for_this_host() {
    if !python3_available() {
        eprintln!("skipping every_spawn_site_launches_the_transport_declared_for_this_host: python3 not found");
        return;
    }
    let env = TestEnv::new();
    let transports = serde_json::json!([
        {
            "type": "stdio",
            "command": "dmcp-no-such-command-for-another-platform",
            "args": [],
            "platforms": [foreign_platform()],
        },
        fake_server_transport(Some(&[dmcp::host_platform()])),
    ]);
    env.install_with_transports("com.test.fake", "user", true, transports);

    let one_shot = env.call_oneshot("com.test.fake", "counter");
    assert!(
        one_shot.status.success(),
        "one-shot call must launch this host's transport: {}",
        String::from_utf8_lossy(&one_shot.stderr)
    );
    assert_eq!(stdout_trimmed(&one_shot), "1");

    let session = env.call_session("com.test.fake", "counter", "PLAT");
    assert!(
        session.status.success(),
        "broker must launch this host's transport: {}",
        String::from_utf8_lossy(&session.stderr)
    );
    assert_eq!(stdout_trimmed(&session), "1");

    let tools = env
        .cmd()
        .args(["tools", "com.test.fake"])
        .output()
        .expect("run dmcp tools");
    assert!(
        tools.status.success(),
        "tools must launch this host's transport: {}",
        String::from_utf8_lossy(&tools.stderr)
    );
    assert!(String::from_utf8_lossy(&tools.stdout).contains("counter"));

    env.session_close("PLAT");
}

/// Nothing is declared for this host: the refusal names the platforms the
/// server was written for, rather than spawning a command meant for another OS
/// and reporting whatever it dies of.
#[test]
fn a_server_with_no_transport_for_this_host_is_refused_by_name() {
    let env = TestEnv::new();
    env.install_with_transports(
        "com.test.elsewhere",
        "user",
        true,
        serde_json::json!([fake_server_transport(Some(&[foreign_platform()]))]),
    );

    for args in [
        vec!["call", "com.test.elsewhere", "counter"],
        vec!["call", "com.test.elsewhere", "counter", "--session", "X"],
        vec!["tools", "com.test.elsewhere"],
        vec!["run", "com.test.elsewhere"],
    ] {
        let out = env.cmd().args(&args).output().expect("run dmcp");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "`dmcp {}` must fail when no transport covers this host",
            args.join(" ")
        );
        assert!(
            stderr.contains(foreign_platform()),
            "`dmcp {}` must name the platforms the server declares, got: {}",
            args.join(" "),
            stderr
        );
        assert!(
            !stderr.contains("No such file or directory"),
            "`dmcp {}` must refuse before spawning, got: {}",
            args.join(" "),
            stderr
        );
    }
}

// ---------------------------------------------------------------------------
// Elicitation (Project-JARVIS#210): a server asking a question mid-tool-call.
// ---------------------------------------------------------------------------

fn eliciting_fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_eliciting_server.py")
}

/// Install the eliciting fake under `id`, stateful and user-scoped — the only
/// shape `--session` accepts, and the only one that can hold a server alive
/// across the question-and-answer exchange.
fn install_eliciting(env: &TestEnv, id: &str) {
    env.install_with_transports(
        id,
        "user",
        true,
        serde_json::json!([{
            "type": "stdio",
            "command": "python3",
            "args": [eliciting_fixture_path().to_string_lossy()],
        }]),
    );
}

/// The end the whole feature turns on: a server that asks a question mid-call
/// must get an answer and finish, never hang.
///
/// The `dmcp call --session` CLI has nobody to ask, so it declines — and a
/// decline is a real protocol answer, not silence. The server unblocks, reports
/// what it heard, and the call completes. Before elicitation was handled at all,
/// rmcp's default client also declined, so this asserts the *observable*
/// contract rather than merely that nothing crashed: the answer reaches the
/// server, and the call terminates well inside the timeout.
#[test]
fn a_server_that_asks_a_question_gets_an_answer_and_finishes() {
    if !python3_available() {
        eprintln!(
            "skipping a_server_that_asks_a_question_gets_an_answer_and_finishes: python3 not found"
        );
        return;
    }
    let env = TestEnv::new();
    install_eliciting(&env, "com.test.elicit");

    let started = Instant::now();
    let out = env.call_session("com.test.elicit", "ask", "ELICIT");
    let elapsed = started.elapsed();

    assert!(
        out.status.success(),
        "eliciting call failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("decline"),
        "the server should have been told 'decline', got: {}",
        stdout
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "the call took {:?} — a parked prompt must not hang the session",
        elapsed
    );
}

/// A wizard asks more than once. Every round must be answered independently,
/// so the server walks through all of them and the call still terminates.
#[test]
fn every_round_of_a_multi_question_wizard_is_answered() {
    if !python3_available() {
        eprintln!("skipping every_round_of_a_multi_question_wizard_is_answered: python3 not found");
        return;
    }
    let env = TestEnv::new();
    install_eliciting(&env, "com.test.elicit.multi");

    let out = env.call_session_args(
        "com.test.elicit.multi",
        "ask",
        Some(r#"{"rounds": 3}"#),
        "WIZARD",
    );

    assert!(
        out.status.success(),
        "multi-round call failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.matches("decline").count(),
        3,
        "each of the three questions needs its own answer, got: {}",
        stdout
    );
}

/// The session survives an elicitation: the server is still the same live
/// process afterwards, so a parked-then-answered prompt does not cost the
/// in-process state the broker exists to preserve.
#[test]
fn a_session_outlives_the_question_it_was_asked() {
    if !python3_available() {
        eprintln!("skipping a_session_outlives_the_question_it_was_asked: python3 not found");
        return;
    }
    let env = TestEnv::new();
    install_eliciting(&env, "com.test.elicit.alive");

    let first = env.call_session("com.test.elicit.alive", "ask", "ALIVE");
    assert!(first.status.success());
    let second = env.call_session("com.test.elicit.alive", "ask", "ALIVE");
    assert!(
        second.status.success(),
        "the session should still be usable after a prompt: {}",
        String::from_utf8_lossy(&second.stderr)
    );
}
