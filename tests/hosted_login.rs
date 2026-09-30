//! End to end: signing in to a hosted MCP server by its own OAuth (the MCP
//! authorization spec), then calling it with the token (Project-JARVIS#229).
//!
//! `tests/fixtures/fake_hosted_server.py` is both the MCP server and its
//! authorization server on a loopback port, and it really checks what a client
//! must do: dynamic registration of the exact redirect URI, PKCE S256, the
//! `resource` parameter, the bearer on every MCP request. The test plays the
//! browser: it follows the authorize URL dmcp prints, and delivers the redirect
//! back to dmcp's loopback listener. The store is forced to the owner-only
//! file, so nothing touches the real keyring, and nothing leaves the machine.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

const SERVER: &str = "com.test.hosted";

struct Hosted {
    child: Child,
    port: u16,
    log: PathBuf,
}

impl Drop for Hosted {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct TestEnv {
    root: PathBuf,
    hosted: Option<Hosted>,
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn browser() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

impl TestEnv {
    fn new(mode: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("dmcp-hosted-it-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("user")).unwrap();
        let log = root.join("hosted.log");
        let mut child = Command::new("python3")
            .arg(fixture("fake_hosted_server.py"))
            .arg(&log)
            .arg(mode)
            .stdout(Stdio::piped())
            .spawn()
            .expect("start fake hosted server");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port: u16 = line.trim().parse().expect("server prints its port");
        let env = TestEnv {
            root,
            hosted: Some(Hosted { child, port, log }),
        };
        env.install(&format!("{}/mcp", env.base()));
        env
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.hosted.as_ref().unwrap().port)
    }

    /// An installed hosted server that signs its caller in with OAuth.
    fn install(&self, url: &str) {
        let base = self.root.join("user/installed");
        let dir = base.join(SERVER);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("manifest.json");
        std::fs::write(
            &manifest,
            serde_json::json!({
                "version": "1.0.0",
                "scope": "user",
                "transports": [{"type": "http", "url": url, "auth": "oauth"}]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            base.join("index.json"),
            serde_json::json!({"servers": {SERVER: {"keywords": [], "location": manifest}}})
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
            .env("DMCP_CREDENTIAL_STORE", "file")
            .env("DMCP_LOGIN_TIMEOUT_SECS", "20")
            .current_dir(&self.root);
        c
    }

    fn dmcp(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().expect("run dmcp")
    }

    /// Start `dmcp login --for SERVER --json --no-browser` and return it with
    /// the authorize URL it printed.
    fn start_login(&self) -> (Child, BufReader<ChildStdout>, String) {
        let mut child = self
            .cmd()
            .args(["login", "--for", SERVER, "--json", "--no-browser"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start dmcp login");
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        let prompt: serde_json::Value =
            serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line:?}"));
        assert_eq!(prompt["type"], "authorize", "{prompt}");
        assert_eq!(prompt["server"], SERVER);
        (child, out, prompt["url"].as_str().unwrap().to_string())
    }

    /// The consent page's redirect, as the browser would receive it.
    fn consent(&self, authorize_url: &str) -> String {
        let resp = browser().get(authorize_url).send().unwrap();
        assert_eq!(
            resp.status().as_u16(),
            302,
            "authorize: {}",
            resp.text().unwrap()
        );
        resp.headers()["location"].to_str().unwrap().to_string()
    }

    fn finish_login(&self, mut child: Child, mut out: BufReader<ChildStdout>) -> serde_json::Value {
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        let status = child.wait().unwrap();
        let result: serde_json::Value =
            serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line:?}"));
        assert_eq!(result["type"], "result");
        if result["status"] == "signed_in" {
            assert!(status.success());
        }
        result
    }

    fn sign_in(&self) -> serde_json::Value {
        let (child, out, url) = self.start_login();
        let back = self.consent(&url);
        let page = browser().get(&back).send().unwrap();
        assert_eq!(page.status().as_u16(), 200);
        assert!(page.text().unwrap().contains("Signed in"));
        self.finish_login(child, out)
    }

    fn requests(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(&self.hosted.as_ref().unwrap().log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn credentials_file(&self) -> PathBuf {
        self.root.join("user/credentials.json")
    }

    fn stored(&self) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(self.credentials_file()).unwrap()).unwrap()
    }

    fn edit_stored(&self, edit: impl FnOnce(&mut serde_json::Value)) {
        let mut stored = self.stored();
        edit(&mut stored[format!("{SERVER}/default")]);
        std::fs::write(self.credentials_file(), stored.to_string()).unwrap();
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        self.hosted.take();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn required_line(out: &Output) -> serde_json::Value {
    let err = stderr(out);
    let line = err
        .lines()
        .find_map(|l| l.strip_prefix("credential_required: "))
        .unwrap_or_else(|| panic!("no credential_required line in: {err}"));
    serde_json::from_str(line).unwrap()
}

#[test]
fn a_hosted_server_is_refused_before_anyone_signs_in() {
    let env = TestEnv::new("ok");
    let out = env.dmcp(&["call", SERVER, "whoami"]);
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    let line = required_line(&out);
    assert_eq!(line["reason"], "no_account");
    assert_eq!(line["hosted"], true);
    assert!(stderr(&out).contains(&format!("dmcp login --for {SERVER}")));
    assert!(
        !env.requests().iter().any(|r| r["path"] == "/mcp"),
        "no request is sent to a server we have no token for"
    );
}

/// Listing tools goes ahead without a token (it is best effort), and the 401
/// that earns is "not signed in", not "rejected": nothing was sent to reject.
#[test]
fn listing_tools_before_sign_in_says_sign_in_not_rejected() {
    let env = TestEnv::new("ok");
    let out = env.dmcp(&["tools", SERVER]);
    assert!(!out.status.success());
    assert_eq!(
        required_line(&out)["reason"],
        "no_account",
        "{}",
        stderr(&out)
    );
}

#[test]
fn signing_in_follows_the_spec_and_the_token_reaches_the_server() {
    let env = TestEnv::new("ok");
    let result = env.sign_in();
    assert_eq!(result["status"], "signed_in", "{result}");
    assert_eq!(result["provider"], SERVER);
    assert_eq!(result["granted_to"], SERVER);

    let requests = env.requests();
    let register = requests.iter().find(|r| r["path"] == "/register").unwrap();
    let redirect = register["json"]["redirect_uris"][0].as_str().unwrap();
    assert!(
        redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/callback"),
        "a loopback redirect is registered: {redirect}"
    );
    let authorize = requests.iter().find(|r| r["path"] == "/authorize").unwrap();
    assert_eq!(authorize["query"]["code_challenge_method"], "S256");
    assert_eq!(
        authorize["query"]["resource"],
        format!("{}/mcp", env.base())
    );
    let exchange = requests
        .iter()
        .find(|r| r["form"]["grant_type"] == "authorization_code")
        .unwrap();
    assert!(exchange["form"]["code_verifier"].as_str().is_some());

    let out = env.dmcp(&["call", SERVER, "whoami"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(
        stdout(&out).contains("called with at-1"),
        "{}",
        stdout(&out)
    );

    let index = std::fs::read_to_string(env.root.join("user/accounts.json")).unwrap();
    assert!(
        !index.contains("at-1") && !index.contains("rt-1"),
        "{index}"
    );
    let listing: serde_json::Value =
        serde_json::from_slice(&env.dmcp(&["accounts", "--json"]).stdout).unwrap();
    assert_eq!(
        listing["accounts"][0]["granted_to"],
        serde_json::json!([SERVER])
    );
}

#[test]
fn listing_tools_works_once_signed_in() {
    let env = TestEnv::new("ok");
    assert_eq!(env.sign_in()["status"], "signed_in");
    let out = env.dmcp(&["tools", SERVER]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("whoami"));
}

#[test]
fn a_stray_or_forged_callback_neither_finishes_nor_aborts_the_sign_in() {
    let env = TestEnv::new("ok");
    let (child, out, url) = env.start_login();
    let back = env.consent(&url);
    let listener = back.split("/callback").next().unwrap().to_string();
    // Wrong state, a favicon fetch, a missing code: all ignored.
    for stray in [
        format!("{listener}/callback?code=forged&state=not-ours"),
        format!("{listener}/favicon.ico"),
        format!("{listener}/callback"),
    ] {
        let resp = browser().get(&stray).send().unwrap();
        assert_eq!(resp.status().as_u16(), 404, "{stray}");
    }
    browser().get(&back).send().unwrap();
    assert_eq!(env.finish_login(child, out)["status"], "signed_in");
}

#[test]
fn a_declined_consent_stores_nothing() {
    let env = TestEnv::new("deny");
    let (child, out, url) = env.start_login();
    let back = env.consent(&url);
    let page = browser().get(&back).send().unwrap();
    assert!(page.text().unwrap().contains("not completed"));
    let result = env.finish_login(child, out);
    assert_eq!(result["status"], "denied", "{result}");
    assert!(!env.credentials_file().exists());
}

/// Refresh is dmcp's, not rmcp's: it must send the resource again so the new
/// token is bound to the same server.
#[test]
fn an_expired_token_is_refreshed_for_the_same_resource() {
    let env = TestEnv::new("ok");
    assert_eq!(env.sign_in()["status"], "signed_in");
    env.edit_stored(|s| s["expires_at"] = serde_json::json!(1));

    let out = env.dmcp(&["call", SERVER, "whoami"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("called with at-2"));
    let refresh = env
        .requests()
        .into_iter()
        .find(|r| r["form"]["grant_type"] == "refresh_token")
        .expect("a refresh");
    assert_eq!(refresh["form"]["resource"], format!("{}/mcp", env.base()));
    assert_eq!(refresh["form"]["refresh_token"], "rt-1");
}

/// A registration that issues a client secret: it is kept with the token (in
/// the secret store, never the index) and presented on refresh.
#[test]
fn a_confidential_registration_keeps_its_secret_out_of_the_index() {
    let env = TestEnv::new("confidential");
    assert_eq!(env.sign_in()["status"], "signed_in");
    assert_eq!(
        env.stored()[format!("{SERVER}/default")]["client_secret"],
        "cs-1"
    );
    let index = std::fs::read_to_string(env.root.join("user/accounts.json")).unwrap();
    assert!(!index.contains("cs-1"), "{index}");

    env.edit_stored(|s| s["expires_at"] = serde_json::json!(1));
    let out = env.dmcp(&["call", SERVER, "whoami"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("called with at-2"));
    assert_eq!(
        env.stored()[format!("{SERVER}/default")]["client_secret"],
        "cs-1",
        "a refresh keeps the registration's secret"
    );
}

#[test]
fn a_token_the_server_rejects_asks_for_a_new_sign_in() {
    let env = TestEnv::new("ok");
    assert_eq!(env.sign_in()["status"], "signed_in");
    env.edit_stored(|s| s["access_token"] = serde_json::json!("revoked"));
    let out = env.dmcp(&["call", SERVER, "whoami"]);
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    assert_eq!(required_line(&out)["reason"], "rejected");
}

#[test]
fn signing_out_removes_the_hosted_token() {
    let env = TestEnv::new("ok");
    assert_eq!(env.sign_in()["status"], "signed_in");
    assert!(env.dmcp(&["logout", SERVER]).status.success());
    assert!(!std::fs::read_to_string(env.credentials_file())
        .unwrap()
        .contains("at-1"));
    assert_eq!(
        required_line(&env.dmcp(&["call", SERVER, "whoami"]))["reason"],
        "no_account"
    );
}

#[test]
fn a_plain_http_hosted_server_is_refused_before_any_request() {
    let env = TestEnv::new("ok");
    env.install("http://mcp.example.invalid/mcp");
    let out = env.dmcp(&["login", "--for", SERVER, "--json", "--no-browser"]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains("must be https"), "{}", stdout(&out));
    assert!(env.requests().is_empty());
}
