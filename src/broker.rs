//! Session-scoped persistent MCP server broker.
//!
//! A stateless MCP server can be spawned fresh for every `dmcp call`, because
//! the process holds no state worth keeping. A *stateful* server (a browser, a
//! REPL, a DB connection) keeps its state inside the process, so the one-shot
//! "spawn, call once, kill" lifecycle throws that state away between calls —
//! navigate lands on a page, then the next call gets a brand-new browser at
//! `about:blank`.
//!
//! The broker is a long-lived process that owns a pool of live rmcp client
//! sessions keyed by `(server_id, session_id)`. A `dmcp call --session <sid>`
//! becomes a thin client that forwards the call to the broker over a Unix
//! domain socket; the broker keeps the child process alive across calls until
//! it is explicitly closed or its idle TTL expires.
//!
//! Scope is restricted to **user** in v1: a system-scoped server runs elevated,
//! and keeping an elevated child alive behind a socket is a privilege-retention
//! hazard we deliberately do not take on yet (see `contract.md` §3, §6).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::discovery::{get_server, Scope};
use crate::paths::Paths;

// ---------------------------------------------------------------------------
// Wire protocol — newline-delimited JSON over the UDS.
// ---------------------------------------------------------------------------

/// A request from a thin client to the broker. One JSON object per line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum BrokerRequest {
    /// Invoke `tool` on the `(id, session)` server, spawning it on first use.
    Call {
        id: String,
        session: String,
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Value>,
    },
    /// Close every server for `session`, or just `id` within it. Idempotent.
    Close {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Force an immediate idle sweep.
    Gc,
    /// List live sessions.
    List,
    /// Liveness probe.
    Ping,
}

/// The broker's reply. `ok` is always present; the remaining fields are
/// populated per-op and omitted otherwise.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct BrokerResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `call`: the tool result text, identical to the one-shot path's stdout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// `call`: whether the tool reported a structured error (drives exit code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// `list`: the live sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionListItem>>,
    /// `close` / `gc`: how many sessions were closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<usize>,
}

impl BrokerResponse {
    fn ok() -> Self {
        Self {
            ok: true,
            ..Default::default()
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
            ..Default::default()
        }
    }
    fn call_ok(content: String, is_error: bool) -> Self {
        Self {
            ok: true,
            content: Some(content),
            is_error: Some(is_error),
            ..Default::default()
        }
    }
    fn closed(n: usize) -> Self {
        Self {
            ok: true,
            closed: Some(n),
            ..Default::default()
        }
    }
    fn list(sessions: Vec<SessionListItem>) -> Self {
        Self {
            ok: true,
            sessions: Some(sessions),
            ..Default::default()
        }
    }
}

/// One row of `dmcp session list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionListItem {
    pub session_id: String,
    pub server_id: String,
    pub scope: String,
    pub age_secs: u64,
    pub idle_secs: u64,
}

/// Serialize a message as a single newline-terminated line (NDJSON framing).
pub fn encode_line<T: Serialize>(value: &T) -> String {
    let mut s = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    s
}

/// Decode one NDJSON request line.
pub fn decode_request(line: &str) -> Result<BrokerRequest, serde_json::Error> {
    serde_json::from_str(line.trim())
}

/// Decode one NDJSON response line.
pub fn decode_response(line: &str) -> Result<BrokerResponse, serde_json::Error> {
    serde_json::from_str(line.trim())
}

// ---------------------------------------------------------------------------
// Gating — which servers may be called with `--session`.
// ---------------------------------------------------------------------------

/// A `--session` call is valid only for a **stateful**, **user-scope** server.
///
/// System scope is rejected by design: keeping an elevated child alive behind a
/// socket would retain privilege past the one authorized call (contract §3/§6).
/// A stateless server is rejected because it has no cross-call state to preserve
/// — routing it through the broker would only add a socket hop and a leaked
/// idle process.
pub fn session_gate(scope: Scope, stateful: Option<bool>) -> Result<(), String> {
    if scope != Scope::User {
        return Err(
            "system-scope servers do not support --session (v1 keeps sessions user-scope for \
             elevation safety)"
                .to_string(),
        );
    }
    if stateful != Some(true) {
        return Err(
            "--session requires a stateful server (manifest \"stateful\": true); this server \
             keeps no in-process state across calls"
                .to_string(),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Socket path resolution.
// ---------------------------------------------------------------------------

/// Directory holding the broker socket. Pure so it is unit-testable without
/// touching the environment.
fn resolve_broker_dir(xdg_runtime_dir: Option<&str>, tmp: &Path, uid: u32) -> PathBuf {
    match xdg_runtime_dir {
        Some(x) if !x.trim().is_empty() => PathBuf::from(x.trim()).join("dmcp"),
        _ => tmp.join(format!("dmcp-{}", uid)),
    }
}

/// The broker's directory: `$XDG_RUNTIME_DIR/dmcp`, or `<tmp>/dmcp-<uid>`.
pub fn broker_dir() -> PathBuf {
    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    resolve_broker_dir(xdg.as_deref(), &std::env::temp_dir(), current_uid())
}

/// The broker socket path inside [`broker_dir`].
pub fn broker_socket_path() -> PathBuf {
    broker_dir().join("broker.sock")
}

#[cfg(unix)]
fn current_uid() -> u32 {
    nix::unistd::Uid::current().as_raw()
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

// ---------------------------------------------------------------------------
// Errors + call result.
// ---------------------------------------------------------------------------

/// Errors surfaced to the CLI for the session path.
#[derive(Debug)]
pub enum SessionError {
    /// The server is not installed, not stateful, or not user-scope.
    Gate(String),
    /// Local I/O (spawning the broker, reading current_exe, etc.).
    Io(String),
    /// Socket setup problem (ownership, bind, permissions).
    Socket(String),
    /// The broker could not be reached or started.
    BrokerUnreachable(String),
    /// Malformed protocol traffic.
    Protocol(String),
    /// The broker returned `ok:false` (e.g. "session lost").
    Remote(String),
    /// Sessions are not supported on this platform.
    Unsupported,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Gate(m) => write!(f, "{}", m),
            SessionError::Io(m) => write!(f, "{}", m),
            SessionError::Socket(m) => write!(f, "{}", m),
            SessionError::BrokerUnreachable(m) => write!(f, "broker unreachable: {}", m),
            SessionError::Protocol(m) => write!(f, "broker protocol error: {}", m),
            SessionError::Remote(m) => write!(f, "{}", m),
            SessionError::Unsupported => {
                write!(f, "session-scoped calls are not supported on this platform")
            }
        }
    }
}

impl std::error::Error for SessionError {}

/// A successful `--session` call: identical shape to the one-shot path so a
/// caller cannot tell the two apart on success.
pub struct SessionCallOk {
    pub content: String,
    pub is_error: bool,
}

// ---------------------------------------------------------------------------
// Public thin-client entry points (used by main.rs).
// ---------------------------------------------------------------------------

/// Perform a `--session` tool call: gate, ensure the broker is running
/// (auto-start it if needed), forward the call, and return the result.
pub fn session_call(
    paths: &Paths,
    id: &str,
    tool: &str,
    args: Option<Value>,
    session: &str,
) -> Result<SessionCallOk, SessionError> {
    let (manifest, scope) = get_server(paths, id)
        .ok_or_else(|| SessionError::Gate(format!("Server not found: {}", id)))?;
    session_gate(scope, manifest.stateful).map_err(SessionError::Gate)?;

    #[cfg(unix)]
    {
        let dir = broker_dir();
        let sock = dir.join("broker.sock");
        ensure_broker_running(&dir, &sock)?;
        let resp = send_request(
            &sock,
            &BrokerRequest::Call {
                id: id.to_string(),
                session: session.to_string(),
                tool: tool.to_string(),
                args,
            },
        )?;
        if resp.ok {
            Ok(SessionCallOk {
                content: resp.content.unwrap_or_default(),
                is_error: resp.is_error.unwrap_or(false),
            })
        } else {
            Err(SessionError::Remote(
                resp.error
                    .unwrap_or_else(|| "unknown broker error".to_string()),
            ))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (session, tool, args);
        Err(SessionError::Unsupported)
    }
}

/// List live sessions. No broker running ⇒ no sessions (never auto-starts one).
pub fn session_list(_paths: &Paths) -> Result<Vec<SessionListItem>, SessionError> {
    #[cfg(unix)]
    {
        let sock = broker_socket_path();
        if !ping(&sock) {
            return Ok(Vec::new());
        }
        let resp = send_request(&sock, &BrokerRequest::List)?;
        Ok(resp.sessions.unwrap_or_default())
    }
    #[cfg(not(unix))]
    {
        Ok(Vec::new())
    }
}

/// Close a session (all of it, or one server within it). Idempotent: returns 0
/// and exits cleanly when nothing is open.
pub fn session_close(
    _paths: &Paths,
    session: &str,
    id: Option<&str>,
) -> Result<usize, SessionError> {
    #[cfg(unix)]
    {
        let sock = broker_socket_path();
        if !ping(&sock) {
            return Ok(0);
        }
        let resp = send_request(
            &sock,
            &BrokerRequest::Close {
                session: session.to_string(),
                id: id.map(String::from),
            },
        )?;
        Ok(resp.closed.unwrap_or(0))
    }
    #[cfg(not(unix))]
    {
        let _ = (session, id);
        Ok(0)
    }
}

/// Force an idle sweep now. No broker ⇒ nothing to do.
pub fn session_gc(_paths: &Paths) -> Result<usize, SessionError> {
    #[cfg(unix)]
    {
        let sock = broker_socket_path();
        if !ping(&sock) {
            return Ok(0);
        }
        let resp = send_request(&sock, &BrokerRequest::Gc)?;
        Ok(resp.closed.unwrap_or(0))
    }
    #[cfg(not(unix))]
    {
        Ok(0)
    }
}

// ---------------------------------------------------------------------------
// Unix thin client.
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn send_request(sock: &Path, req: &BrokerRequest) -> Result<BrokerResponse, SessionError> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let stream =
        UnixStream::connect(sock).map_err(|e| SessionError::BrokerUnreachable(e.to_string()))?;
    let mut writer = stream
        .try_clone()
        .map_err(|e| SessionError::Io(e.to_string()))?;
    writer
        .write_all(encode_line(req).as_bytes())
        .map_err(|e| SessionError::Io(e.to_string()))?;
    writer.flush().ok();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| SessionError::Io(e.to_string()))?;
    if line.trim().is_empty() {
        return Err(SessionError::Protocol(
            "empty response from broker".to_string(),
        ));
    }
    decode_response(&line).map_err(|e| SessionError::Protocol(e.to_string()))
}

#[cfg(unix)]
fn ping(sock: &Path) -> bool {
    send_request(sock, &BrokerRequest::Ping)
        .map(|r| r.ok)
        .unwrap_or(false)
}

/// Create the socket directory 0700 and verify it is owned by the current uid.
/// Refuse a directory owned by anyone else — the socket must not be hijackable.
#[cfg(unix)]
fn ensure_broker_dir(dir: &Path) -> Result<(), SessionError> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| SessionError::Socket(format!("create {}: {}", dir.display(), e)))?;
    }
    let meta = std::fs::metadata(dir)
        .map_err(|e| SessionError::Socket(format!("stat {}: {}", dir.display(), e)))?;
    if !meta.is_dir() {
        return Err(SessionError::Socket(format!(
            "{} exists but is not a directory",
            dir.display()
        )));
    }
    let uid = current_uid();
    if meta.uid() != uid {
        return Err(SessionError::Socket(format!(
            "refusing to use {}: owned by uid {}, not {}",
            dir.display(),
            meta.uid(),
            uid
        )));
    }
    // Tighten perms in case the directory pre-existed with a looser mode.
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

/// Ensure a broker is live: probe, and if absent auto-start `<self> broker`
/// detached, then poll for readiness up to 5s.
#[cfg(unix)]
fn ensure_broker_running(dir: &Path, sock: &Path) -> Result<(), SessionError> {
    use std::time::{Duration, Instant};

    if ping(sock) {
        return Ok(());
    }
    ensure_broker_dir(dir)?;
    spawn_broker_detached(dir)?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if ping(sock) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(SessionError::BrokerUnreachable(
        "broker did not become ready within 5s".to_string(),
    ))
}

/// Spawn `<current_exe> broker` in its own process group, detached from this
/// process, with stdio redirected to a log file under the socket dir (or null).
#[cfg(unix)]
fn spawn_broker_detached(dir: &Path) -> Result<(), SessionError> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().map_err(|e| SessionError::Io(e.to_string()))?;

    let (out, err) = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("broker.log"))
    {
        Ok(f) => {
            let f2 = f.try_clone().ok();
            (
                Stdio::from(f),
                f2.map(Stdio::from).unwrap_or_else(Stdio::null),
            )
        }
        Err(_) => (Stdio::null(), Stdio::null()),
    };

    let mut cmd = Command::new(exe);
    cmd.arg("broker")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        // New process group so a Ctrl-C or process-group kill of the caller
        // (dispatch kills a --session task's group) does NOT reap the broker —
        // the whole point is that the broker outlives any single call.
        .process_group(0);
    cmd.spawn()
        .map_err(|e| SessionError::Io(format!("failed to spawn broker: {}", e)))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unix broker server.
// ---------------------------------------------------------------------------

#[cfg(unix)]
pub use server::run_broker_foreground;

#[cfg(not(unix))]
pub fn run_broker_foreground(_paths: &Paths) -> Result<(), SessionError> {
    Err(SessionError::Unsupported)
}

#[cfg(unix)]
mod server {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use rmcp::model::CallToolRequestParams;
    use rmcp::service::RunningService;
    use rmcp::transport::TokioChildProcess;
    use rmcp::{RoleClient, ServiceExt};
    use tokio::sync::Mutex as AsyncMutex;

    use crate::models::Transport;

    type Key = (String, String);
    type Client = RunningService<RoleClient, ()>;
    type Entry = Arc<AsyncMutex<SessionSlot>>;

    /// One pooled session. The `client` is `None` before first spawn and after a
    /// loss/close; `removed` fences a slot that a sweep or close excised so a
    /// call racing the removal transparently retries with a fresh slot.
    struct SessionSlot {
        client: Option<Client>,
        created: Instant,
        last_used: Instant,
        removed: bool,
    }

    struct Broker {
        paths: Paths,
        ttl: Duration,
        sessions: Mutex<HashMap<Key, Entry>>,
        empty_since: Mutex<Option<Instant>>,
        shutdown: tokio::sync::Notify,
    }

    fn ttl_from_env() -> Duration {
        let secs = std::env::var("DMCP_SESSION_TTL_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(300);
        Duration::from_secs(secs)
    }

    /// Sweep cadence: ~30s by default, but scaled down for a short TTL so a low
    /// `DMCP_SESSION_TTL_SECS` (tests) is still enforced promptly.
    fn sweep_interval(ttl: Duration) -> Duration {
        let secs = (ttl.as_secs().max(2) / 2).clamp(1, 30);
        Duration::from_secs(secs)
    }

    fn idle_exit() -> Duration {
        let secs = std::env::var("DMCP_BROKER_IDLE_EXIT_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(600);
        Duration::from_secs(secs)
    }

    impl Broker {
        fn get_or_create(&self, key: &Key) -> Entry {
            let mut map = self.sessions.lock().unwrap();
            if let Some(e) = map.get(key) {
                return e.clone();
            }
            let e: Entry = Arc::new(AsyncMutex::new(SessionSlot {
                client: None,
                created: Instant::now(),
                last_used: Instant::now(),
                removed: false,
            }));
            map.insert(key.clone(), e.clone());
            *self.empty_since.lock().unwrap() = None;
            e
        }

        /// Remove a key from the map only if it still points at `entry` (guards
        /// against clobbering a fresh slot a concurrent call just installed).
        fn remove_entry(&self, key: &Key, entry: &Entry) {
            let mut map = self.sessions.lock().unwrap();
            if let Some(e) = map.get(key) {
                if Arc::ptr_eq(e, entry) {
                    map.remove(key);
                }
            }
        }

        fn update_empty(&self) {
            let empty = self.sessions.lock().unwrap().is_empty();
            let mut es = self.empty_since.lock().unwrap();
            if empty {
                if es.is_none() {
                    *es = Some(Instant::now());
                }
            } else {
                *es = None;
            }
        }

        fn should_exit(&self, idle_exit: Duration) -> bool {
            match *self.empty_since.lock().unwrap() {
                Some(t) => t.elapsed() >= idle_exit,
                None => false,
            }
        }

        /// Spawn + initialize a fresh rmcp client for `id`, reusing the one-shot
        /// path's manifest→transport / install-dir / env resolution.
        async fn spawn_client(&self, id: &str) -> Result<Client, String> {
            let (manifest, _scope) =
                get_server(&self.paths, id).ok_or_else(|| format!("Server not found: {}", id))?;
            let transports = manifest
                .transports
                .as_deref()
                .ok_or_else(|| "no transports defined".to_string())?;
            let primary = transports
                .first()
                .ok_or_else(|| "no transports defined".to_string())?;
            let (command, args) =
                match primary {
                    Transport::Stdio { command, args, .. } => (command.clone(), args.clone()),
                    _ => return Err(
                        "session broker supports stdio (local) servers only; remote servers are \
                         already long-lived"
                            .to_string(),
                    ),
                };
            let cmd = crate::call::build_stdio_command(
                &self.paths,
                &manifest,
                id,
                &command,
                args.as_deref(),
            )
            .map_err(|e| e.to_string())?;
            let transport = TokioChildProcess::new(cmd).map_err(|e| e.to_string())?;
            let client = ().serve(transport).await.map_err(|e| e.to_string())?;
            Ok(client)
        }

        async fn handle_call(
            &self,
            id: String,
            session: String,
            tool: String,
            args: Option<Value>,
        ) -> BrokerResponse {
            let key: Key = (id.clone(), session);
            loop {
                let entry = self.get_or_create(&key);
                let mut slot = entry.lock().await;
                if slot.removed {
                    // A sweep or close excised this slot between get_or_create
                    // and the lock; retry against a fresh one.
                    drop(slot);
                    continue;
                }
                if slot.client.is_none() {
                    match self.spawn_client(&id).await {
                        Ok(c) => {
                            slot.client = Some(c);
                            slot.created = Instant::now();
                        }
                        Err(e) => {
                            slot.removed = true;
                            drop(slot);
                            self.remove_entry(&key, &entry);
                            self.update_empty();
                            return BrokerResponse::err(format!(
                                "failed to start server '{}': {}",
                                id, e
                            ));
                        }
                    }
                }

                let args_obj = args
                    .clone()
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default();
                let result = {
                    let client = slot.client.as_ref().unwrap();
                    client
                        .call_tool(CallToolRequestParams {
                            meta: None,
                            name: tool.clone().into(),
                            arguments: if args_obj.is_empty() {
                                None
                            } else {
                                Some(args_obj)
                            },
                            task: None,
                        })
                        .await
                };

                return match result {
                    Ok(res) => {
                        slot.last_used = Instant::now();
                        BrokerResponse::call_ok(
                            crate::call::format_call_result(&res),
                            crate::call::call_is_error(&res),
                        )
                    }
                    Err(e) => {
                        // A transport/protocol failure on an established session
                        // means the child is gone. Evict and report the loss —
                        // never an empty success the caller would mistake for a
                        // real page.
                        if let Some(c) = slot.client.take() {
                            let _ = c.cancel().await;
                        }
                        slot.removed = true;
                        drop(slot);
                        self.remove_entry(&key, &entry);
                        self.update_empty();
                        BrokerResponse::err(format!("session lost: {}", e))
                    }
                };
            }
        }

        async fn handle_close(&self, session: String, id: Option<String>) -> BrokerResponse {
            let targets: Vec<(Key, Entry)> = {
                let map = self.sessions.lock().unwrap();
                map.iter()
                    .filter(|((server_id, sid), _)| {
                        let session_matches = *sid == session;
                        let id_matches = match &id {
                            Some(want) => want == server_id,
                            None => true,
                        };
                        session_matches && id_matches
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            };

            let mut closed = 0;
            for (key, entry) in targets {
                let mut slot = entry.lock().await;
                if let Some(c) = slot.client.take() {
                    let _ = c.cancel().await;
                    closed += 1;
                }
                slot.removed = true;
                drop(slot);
                self.remove_entry(&key, &entry);
            }
            self.update_empty();
            BrokerResponse::closed(closed)
        }

        async fn sweep(&self) -> usize {
            let entries: Vec<(Key, Entry)> = {
                let map = self.sessions.lock().unwrap();
                map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
            };

            let mut to_kill: Vec<Client> = Vec::new();
            {
                let mut map = self.sessions.lock().unwrap();
                for (key, entry) in &entries {
                    // try_lock skips a session with a call in flight — an active
                    // session is never "idle".
                    if let Ok(mut slot) = entry.try_lock() {
                        let expired = slot.client.is_some() && slot.last_used.elapsed() > self.ttl;
                        let orphan = slot.client.is_none() && !slot.removed;
                        if expired || orphan {
                            if let Some(c) = slot.client.take() {
                                to_kill.push(c);
                            }
                            slot.removed = true;
                            if let Some(e) = map.get(key) {
                                if Arc::ptr_eq(e, entry) {
                                    map.remove(key);
                                }
                            }
                        }
                    }
                }
            }

            let n = to_kill.len();
            for c in to_kill {
                let _ = c.cancel().await;
            }
            self.update_empty();
            n
        }

        async fn list(&self) -> Vec<SessionListItem> {
            let entries: Vec<(Key, Entry)> = {
                let map = self.sessions.lock().unwrap();
                map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
            };
            let mut out = Vec::new();
            for ((server_id, session_id), entry) in entries {
                match entry.try_lock() {
                    Ok(slot) => {
                        if slot.client.is_some() {
                            out.push(SessionListItem {
                                session_id,
                                server_id,
                                scope: "user".to_string(),
                                age_secs: slot.created.elapsed().as_secs(),
                                idle_secs: slot.last_used.elapsed().as_secs(),
                            });
                        }
                    }
                    // Busy ⇒ a call is running right now: active, idle 0.
                    Err(_) => out.push(SessionListItem {
                        session_id,
                        server_id,
                        scope: "user".to_string(),
                        age_secs: 0,
                        idle_secs: 0,
                    }),
                }
            }
            out.sort_by(|a, b| {
                a.session_id
                    .cmp(&b.session_id)
                    .then(a.server_id.cmp(&b.server_id))
            });
            out
        }

        async fn shutdown_all(&self) {
            let entries: Vec<Entry> = {
                let mut map = self.sessions.lock().unwrap();
                map.drain().map(|(_, v)| v).collect()
            };
            for entry in entries {
                let mut slot = entry.lock().await;
                if let Some(c) = slot.client.take() {
                    let _ = c.cancel().await;
                }
                slot.removed = true;
            }
        }

        async fn dispatch(&self, req: BrokerRequest) -> BrokerResponse {
            match req {
                BrokerRequest::Ping => BrokerResponse::ok(),
                BrokerRequest::Call {
                    id,
                    session,
                    tool,
                    args,
                } => self.handle_call(id, session, tool, args).await,
                BrokerRequest::Close { session, id } => self.handle_close(session, id).await,
                BrokerRequest::Gc => BrokerResponse::closed(self.sweep().await),
                BrokerRequest::List => BrokerResponse::list(self.list().await),
            }
        }
    }

    async fn handle_conn(broker: Arc<Broker>, stream: tokio::net::UnixStream) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (rd, mut wr) = stream.into_split();
        let mut reader = BufReader::new(rd);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let resp = match decode_request(trimmed) {
                Ok(req) => broker.dispatch(req).await,
                Err(e) => BrokerResponse::err(format!("bad request: {}", e)),
            };
            if wr.write_all(encode_line(&resp).as_bytes()).await.is_err() {
                break;
            }
            let _ = wr.flush().await;
        }
    }

    /// Run the broker in the foreground (the `dmcp broker` subcommand).
    pub fn run_broker_foreground(paths: &Paths) -> Result<(), SessionError> {
        let rt = tokio::runtime::Runtime::new().map_err(|e| SessionError::Io(e.to_string()))?;
        rt.block_on(broker_main(paths.clone()))
    }

    async fn broker_main(paths: Paths) -> Result<(), SessionError> {
        use std::os::unix::fs::PermissionsExt;
        use tokio::net::UnixListener;

        let dir = broker_dir();
        let sock = dir.join("broker.sock");
        ensure_broker_dir(&dir)?;

        // If a live broker already owns the socket, defer to it; if the socket is
        // stale (no one answers), reclaim it.
        if sock.exists() {
            if ping(&sock) {
                return Ok(());
            }
            let _ = std::fs::remove_file(&sock);
        }

        let listener = UnixListener::bind(&sock)
            .map_err(|e| SessionError::Socket(format!("bind {}: {}", sock.display(), e)))?;
        let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));

        let ttl = ttl_from_env();
        let broker = Arc::new(Broker {
            paths,
            ttl,
            sessions: Mutex::new(HashMap::new()),
            empty_since: Mutex::new(Some(Instant::now())),
            shutdown: tokio::sync::Notify::new(),
        });

        let sweeper = {
            let broker = Arc::clone(&broker);
            let interval = sweep_interval(ttl);
            let idle = idle_exit();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    broker.sweep().await;
                    if broker.should_exit(idle) {
                        broker.shutdown.notify_waiters();
                        break;
                    }
                }
            })
        };

        loop {
            tokio::select! {
                _ = broker.shutdown.notified() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => {
                            let broker = Arc::clone(&broker);
                            tokio::spawn(async move { handle_conn(broker, stream).await });
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        sweeper.abort();
        broker.shutdown_all().await;
        let _ = std::fs::remove_file(&sock);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests (platform-independent: protocol, gate, socket-path resolution).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_request_roundtrips() {
        let req = BrokerRequest::Call {
            id: "com.example.browser".into(),
            session: "abc123".into(),
            tool: "navigate".into(),
            args: Some(serde_json::json!({"url": "https://example.com"})),
        };
        let line = encode_line(&req);
        assert!(line.ends_with('\n'));
        assert_eq!(decode_request(&line).unwrap(), req);
    }

    #[test]
    fn op_tag_is_lowercase_and_flat() {
        let line = encode_line(&BrokerRequest::Ping);
        assert_eq!(line.trim(), r#"{"op":"ping"}"#);
        let close = BrokerRequest::Close {
            session: "s".into(),
            id: None,
        };
        // `id` omitted when absent (skip_serializing_if).
        assert_eq!(close_line_op(&close), "close");
        assert!(!encode_line(&close).contains("\"id\""));
    }

    fn close_line_op(req: &BrokerRequest) -> String {
        let v: serde_json::Value = serde_json::from_str(encode_line(req).trim()).unwrap();
        v["op"].as_str().unwrap().to_string()
    }

    #[test]
    fn close_with_id_roundtrips() {
        let req = BrokerRequest::Close {
            session: "s".into(),
            id: Some("com.example.x".into()),
        };
        assert_eq!(decode_request(&encode_line(&req)).unwrap(), req);
    }

    #[test]
    fn response_omits_empty_fields() {
        let r = BrokerResponse::ok();
        let line = encode_line(&r);
        assert!(line.contains("\"ok\":true"));
        assert!(!line.contains("error"));
        assert!(!line.contains("content"));
        assert!(!line.contains("sessions"));
    }

    #[test]
    fn call_ok_response_carries_content_and_flag() {
        let r = BrokerResponse::call_ok("hello".into(), false);
        let decoded = decode_response(&encode_line(&r)).unwrap();
        assert!(decoded.ok);
        assert_eq!(decoded.content.as_deref(), Some("hello"));
        assert_eq!(decoded.is_error, Some(false));
    }

    #[test]
    fn err_response_roundtrips() {
        let r = BrokerResponse::err("session lost: broken pipe");
        let decoded = decode_response(&encode_line(&r)).unwrap();
        assert!(!decoded.ok);
        assert_eq!(decoded.error.as_deref(), Some("session lost: broken pipe"));
    }

    #[test]
    fn gate_allows_stateful_user_scope() {
        assert!(session_gate(Scope::User, Some(true)).is_ok());
    }

    #[test]
    fn gate_rejects_system_scope_even_when_stateful() {
        let err = session_gate(Scope::System, Some(true)).unwrap_err();
        assert!(err.contains("system-scope"));
    }

    #[test]
    fn gate_rejects_stateless_server() {
        assert!(session_gate(Scope::User, None)
            .unwrap_err()
            .contains("stateful"));
        assert!(session_gate(Scope::User, Some(false))
            .unwrap_err()
            .contains("stateful"));
    }

    #[test]
    fn socket_dir_prefers_xdg_runtime_dir() {
        let d = resolve_broker_dir(Some("/run/user/1000"), Path::new("/tmp"), 1000);
        assert_eq!(d, PathBuf::from("/run/user/1000/dmcp"));
    }

    #[test]
    fn socket_dir_falls_back_to_tmp_with_uid() {
        let d = resolve_broker_dir(None, Path::new("/tmp"), 1000);
        assert_eq!(d, PathBuf::from("/tmp/dmcp-1000"));
        // Empty/whitespace XDG value is treated as unset.
        let d2 = resolve_broker_dir(Some("   "), Path::new("/tmp"), 7);
        assert_eq!(d2, PathBuf::from("/tmp/dmcp-7"));
    }
}
