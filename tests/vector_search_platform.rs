//! Integration test for the platform state on the vector-search surface.
//!
//! `dmcp browse --vector … --json` is the discovery surface the agent actually
//! reaches: dispatch's `browse_servers` / `browse_servers_batch` MCP tools shell
//! out to exactly this command. The `--json` payload therefore has to carry the
//! host verdict, the same way the keyword browse does — otherwise the agent keeps
//! proposing servers that cannot run on this machine.
//!
//! The index is written directly instead of synced, so nothing here needs a
//! registry to be reachable.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn dmcp_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dmcp")
}

/// A platform that is never this host, so the refusal path is exercised the
/// same way whichever OS the suite runs on.
fn foreign_platform() -> &'static str {
    if dmcp::host_platform() == "linux" {
        "windows"
    } else {
        "linux"
    }
}

/// An isolated index tree, removed on drop.
struct TestEnv {
    root: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("dmcp-vector-it-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(root.join("vector")).unwrap();
        TestEnv { root }
    }

    /// Write a two-entry index: one server declared for `platforms` (when given)
    /// and one tool-level entry for the same server.
    fn write_index(&self, platforms: Option<&[&str]>) {
        let mut server = serde_json::json!({
            "server_id": "com.test.thing",
            "server_name": "Thing",
            "server_description": "Does a thing",
            "vector": [1.0, 0.0],
            "source": "registry",
        });
        let mut tool = serde_json::json!({
            "server_id": "com.test.thing",
            "server_name": "Thing",
            "tool_name": "do_thing",
            "tool_description": "Do it",
            "vector": [0.9, 0.1],
            "source": "registry",
        });
        if let Some(p) = platforms {
            server["platforms"] = serde_json::json!(p);
            tool["platforms"] = serde_json::json!(p);
        }
        let index = serde_json::json!({"entries": [server, tool]});
        std::fs::write(
            self.root.join("vector/index.json"),
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

    fn browse_vector(&self, extra: &[&str]) -> Output {
        let mut args = vec!["browse", "--vector", "[1.0, 0.0]", "--top-k", "5"];
        args.extend_from_slice(extra);
        self.cmd().args(args).output().expect("run dmcp browse")
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn json_stdout(out: &Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "browse failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("browse --json emits JSON")
}

/// The payload dispatch hands the agent marks a server this host cannot run —
/// on the tool-level hit too, which is the one a capability search returns.
#[test]
fn vector_search_json_marks_a_foreign_platform_entry() {
    let env = TestEnv::new();
    env.write_index(Some(&[foreign_platform()]));

    let results = json_stdout(&env.browse_vector(&["--json"]));
    let results = results.as_array().expect("an array of results");
    assert_eq!(results.len(), 2, "server-level and tool-level hits");
    for r in results {
        assert_eq!(
            r["unsupported_on_host"],
            serde_json::json!(true),
            "result {r} must be marked unsupported"
        );
        assert_eq!(r["platforms"], serde_json::json!([foreign_platform()]));
    }
}

/// Absent everywhere is today's behavior: the verdict is still stated, so the
/// agent never has to tell "supported" apart from "not reported".
#[test]
fn vector_search_json_always_states_the_verdict() {
    let env = TestEnv::new();
    env.write_index(None);

    let results = json_stdout(&env.browse_vector(&["--json"]));
    for r in results.as_array().expect("an array of results") {
        assert_eq!(r["unsupported_on_host"], serde_json::json!(false));
        assert!(r.get("platforms").is_none(), "no list to report: {r}");
    }
}

/// The batch surface (`browse_servers_batch`) carries it too.
#[test]
fn batch_vector_search_json_marks_a_foreign_platform_entry() {
    let env = TestEnv::new();
    env.write_index(Some(&[foreign_platform()]));

    let out = env
        .cmd()
        .args([
            "browse",
            "--vectors",
            "[[1.0, 0.0]]",
            "--top-k",
            "5",
            "--json",
        ])
        .output()
        .expect("run dmcp browse --vectors");
    let batch = json_stdout(&out);
    let first = batch[0].as_array().expect("one result set per query");
    assert!(!first.is_empty());
    for r in first {
        assert_eq!(r["unsupported_on_host"], serde_json::json!(true));
    }
}

/// The human surface says it too, so an operator reading the table sees the
/// same thing the agent does.
#[test]
fn vector_search_table_shows_the_platform_line() {
    let env = TestEnv::new();
    env.write_index(Some(&[foreign_platform()]));

    let out = env.browse_vector(&[]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Platforms:")
            && stdout.contains(foreign_platform())
            && stdout.contains("UNSUPPORTED"),
        "the table names the platforms and the verdict: {stdout}"
    );
}
