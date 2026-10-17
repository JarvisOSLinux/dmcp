//! `dmcp config <id> set <KEY> --stdin` takes the value from standard input.
//!
//! JARVIS writes server secrets (API keys) through `dmcp config set`. As a
//! command-line argument the value is readable by any local user from the
//! process list while dmcp runs, and dmcp echoed it back on stdout. With
//! --stdin it is in neither place.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);
const ID: &str = "com.test.configured";

struct TestEnv {
    root: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("dmcp-config-stdin-it-{}-{}", std::process::id(), n));
        let base = root.join("user/installed");
        let dir = base.join(ID);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("manifest.json");
        std::fs::write(
            &manifest,
            serde_json::json!({
                "version": "1.0.0",
                "transports": [{"type": "stdio", "command": "true", "args": []}],
                "configurableProperties": [{"key": "API_KEY"}, {"key": "K"}, {"key": "MODE"}],
                "config": {},
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            base.join("index.json"),
            serde_json::json!({"servers": {ID: {"keywords": [], "location": manifest}}})
                .to_string(),
        )
        .unwrap();
        TestEnv { root }
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dmcp"))
            .args(args)
            .env("MCP_USER_INSTALL_DIR", self.root.join("user/installed"))
            .env("MCP_SYSTEM_INSTALL_DIR", self.root.join("system/installed"))
            .env("MCP_USER_SOURCES_PATH", self.root.join("user/sources.list"))
            .env(
                "MCP_SYSTEM_SOURCES_PATH",
                self.root.join("system/sources.list"),
            )
            .env("MCP_VECTOR_INDEX_DIR", self.root.join("vector"))
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run dmcp");
        let mut pipe = child.stdin.take().unwrap();
        if let Some(text) = stdin {
            pipe.write_all(text.as_bytes()).unwrap();
        }
        drop(pipe);
        child.wait_with_output().unwrap()
    }

    fn config(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(
            self.root
                .join("user/installed")
                .join(ID)
                .join("manifest.json"),
        )
        .unwrap();
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["config"].clone()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn stdin_value_is_stored_and_not_echoed() {
    let env = TestEnv::new();
    let out = env.run(
        &["config", ID, "set", "API_KEY", "--stdin"],
        Some("s3cret value\n"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), "Set API_KEY");
    assert!(!stdout.contains("s3cret"));
    assert_eq!(env.config()["API_KEY"], "s3cret value");
}

#[test]
fn only_one_trailing_newline_is_dropped() {
    let env = TestEnv::new();
    let out = env.run(&["config", ID, "set", "K", "--stdin"], Some("line\r\n"));
    assert!(out.status.success());
    assert_eq!(env.config()["K"], "line");
}

#[test]
fn an_argument_value_still_works() {
    let env = TestEnv::new();
    let out = env.run(&["config", ID, "set", "MODE", "read-only"], None);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "Set MODE = read-only"
    );
    assert_eq!(env.config()["MODE"], "read-only");
}

#[test]
fn a_value_is_required_one_way_or_the_other() {
    let env = TestEnv::new();
    assert!(!env.run(&["config", ID, "set", "K"], None).status.success());
    assert!(!env
        .run(&["config", ID, "set", "K", "v", "--stdin"], Some("w"))
        .status
        .success());
    assert_eq!(env.config(), serde_json::json!({}));
}

#[test]
fn a_setting_the_server_does_not_declare_is_refused() {
    let env = TestEnv::new();
    let out = env.run(
        &["config", ID, "set", "NODE_OPTIONS", "--stdin"],
        Some("--import=data:text/javascript,0\n"),
    );
    assert!(!out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "Error: NODE_OPTIONS is not a setting of this server; its settings are: API_KEY, K, MODE"
    );
    assert_eq!(env.config(), serde_json::json!({}));
}
