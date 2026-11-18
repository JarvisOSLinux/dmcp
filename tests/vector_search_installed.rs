//! Integration test for install status on the vector-search surface.
//!
//! `dmcp browse --vector … --json` is what dispatch's `browse_servers` tools
//! shell out to, and JARVIS labels each hit from it [INSTALLED] or [available].
//! Its planner installs whatever reads as available. So a result that omits
//! install status does not read as "unknown" -- it reads as "not installed",
//! and the agent re-clones a server it already has on every single query.
//! Keyword browse has always reported this; vector search did not.
//!
//! The vector index and the install indexes are written directly, so nothing
//! here needs a registry, a network, or a real server.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

const INSTALLED: &str = "com.test.installed";
const AVAILABLE: &str = "com.test.available";

struct TestEnv {
    root: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "dmcp-vector-installed-it-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(root.join("vector")).unwrap();
        let env = TestEnv { root };
        env.write_index();
        env
    }

    /// Two servers, both close to the query vector, so both are returned.
    fn write_index(&self) {
        let entry = |id: &str, v: [f32; 2]| {
            serde_json::json!({
                "server_id": id,
                "server_name": id,
                "server_description": "A server",
                "vector": v,
                "source": "registry",
            })
        };
        let index = serde_json::json!({
            "entries": [entry(INSTALLED, [1.0, 0.0]), entry(AVAILABLE, [0.9, 0.1])]
        });
        std::fs::write(
            self.root.join("vector/index.json"),
            serde_json::to_string_pretty(&index).unwrap(),
        )
        .unwrap();
    }

    /// Record `id` as installed at `scope` ("user" or "system"), the way an
    /// install leaves it: an entry in that scope's index.json pointing at a
    /// manifest that parses.
    fn install(&self, scope: &str, id: &str) {
        let base = self.root.join(scope).join("installed");
        let dir = base.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("manifest.json");
        std::fs::write(
            &manifest,
            serde_json::json!({
                "version": "1.0.0",
                "transports": [{"type": "stdio", "command": "true", "args": []}],
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            base.join("index.json"),
            serde_json::json!({
                "servers": { id: { "keywords": [], "location": manifest } }
            })
            .to_string(),
        )
        .unwrap();
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_dmcp"));
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

    fn browse(&self, args: &[&str]) -> serde_json::Value {
        let out: Output = self.cmd().args(args).output().expect("run dmcp browse");
        assert!(
            out.status.success(),
            "browse failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("browse --json emits JSON")
    }

    fn search(&self) -> Vec<serde_json::Value> {
        self.browse(&["browse", "--vector", "[1.0, 0.0]", "--top-k", "5", "--json"])
            .as_array()
            .expect("an array of results")
            .clone()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn status_of(results: &[serde_json::Value], id: &str) -> serde_json::Value {
    results
        .iter()
        .find(|r| r["server_id"] == id)
        .unwrap_or_else(|| panic!("{id} missing from {results:?}"))["installed"]
        .clone()
}

#[test]
fn vector_search_marks_an_installed_server_and_only_that_one() {
    let env = TestEnv::new();
    env.install("user", INSTALLED);

    let results = env.search();
    assert_eq!(status_of(&results, INSTALLED), serde_json::json!(true));
    assert_eq!(status_of(&results, AVAILABLE), serde_json::json!(false));
}

/// A system-scope install counts. jarvis-shell-system lives in
/// /usr/share/mcp, and reporting it "available" would have the agent try to
/// install a system-scope server as the user.
#[test]
fn vector_search_sees_system_scope_installs() {
    let env = TestEnv::new();
    env.install("system", INSTALLED);

    let results = env.search();
    assert_eq!(status_of(&results, INSTALLED), serde_json::json!(true));
}

/// Stated even when false. An absent flag is exactly the bug: the consumer
/// reads it as not installed, so the field has to be present either way.
#[test]
fn vector_search_always_states_install_status() {
    let env = TestEnv::new();

    for r in env.search() {
        assert_eq!(r["installed"], serde_json::json!(false), "result {r}");
    }
}

/// The batch surface (`browse_servers_batch`) carries it too, per query.
#[test]
fn batch_vector_search_marks_installed_servers() {
    let env = TestEnv::new();
    env.install("user", INSTALLED);

    let batch = env.browse(&[
        "browse",
        "--vectors",
        "[[1.0, 0.0], [0.9, 0.1]]",
        "--top-k",
        "5",
        "--json",
    ]);
    let groups = batch.as_array().expect("one result set per query");
    assert_eq!(groups.len(), 2);
    for group in groups {
        let results = group.as_array().expect("a result array");
        assert_eq!(status_of(results, INSTALLED), serde_json::json!(true));
        assert_eq!(status_of(results, AVAILABLE), serde_json::json!(false));
    }
}
