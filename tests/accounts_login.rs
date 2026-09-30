//! End to end: signing in, granting, and a server receiving the account
//! (Project-JARVIS#229).
//!
//! A fake OAuth provider (`tests/fixtures/fake_oauth_server.py`) stands in for
//! GitHub on a loopback port and logs every request it gets; a fake MCP server
//! (`fake_env_server.py`) reports the environment it was started with. The
//! registry is a local file, the store is forced to the owner-only file (no
//! test touches the real keyring), and nothing here reaches the network.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

const SERVER: &str = "com.test.env";
const TOKEN: &str = "gho_fake_device_token";

struct Provider {
    child: Child,
    port: u16,
    log: PathBuf,
}

impl Drop for Provider {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct TestEnv {
    root: PathBuf,
    provider: Option<Provider>,
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

impl TestEnv {
    fn new(mode: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("dmcp-accounts-it-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("user")).unwrap();

        let log = root.join("oauth.log");
        let mut child = Command::new("python3")
            .arg(fixture("fake_oauth_server.py"))
            .arg(&log)
            .arg(mode)
            .stdout(Stdio::piped())
            .spawn()
            .expect("start fake OAuth provider");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port: u16 = line.trim().parse().expect("provider prints its port");

        let env = TestEnv {
            root,
            provider: Some(Provider { child, port, log }),
        };
        let base = format!("http://127.0.0.1:{port}");
        env.write_registry([base.as_str(); 3]);
        env.install();
        env
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.provider.as_ref().unwrap().port)
    }

    /// Bases for the device, token and identity endpoints.
    fn write_registry(&self, [device, token, identity]: [&str; 3]) {
        let registry = serde_json::json!({
            "version": "1.0",
            "providers": {
                "demo": {
                    "id": "demo",
                    "name": "Demo",
                    "oauth": {
                        "deviceAuthorizationEndpoint": format!("{device}/device"),
                        "tokenEndpoint": format!("{token}/token"),
                    },
                    "identity": {"url": format!("{identity}/user"), "field": "login"},
                    "scopes": {"repo": "Everything", "gist": "Gists"}
                }
            },
            "servers": {}
        });
        std::fs::write(self.root.join("registry.json"), registry.to_string()).unwrap();
        std::fs::write(
            self.root.join("user/sources.list"),
            format!("file://{}\n", self.root.join("registry.json").display()),
        )
        .unwrap();
    }

    /// An installed user-scope server that declares a `demo` credential.
    fn install(&self) {
        let base = self.root.join("user/installed");
        let dir = base.join(SERVER);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("manifest.json");
        std::fs::write(
            &manifest,
            serde_json::json!({
                "version": "1.0.0",
                "scope": "user",
                "installDir": dir.to_string_lossy(),
                "transports": [{
                    "type": "stdio",
                    "command": "python3",
                    "args": [fixture("fake_env_server.py").to_string_lossy()],
                }],
                "configurableProperties": [
                    {"key": "DEMO_TOKEN", "sensitive": true, "required": true},
                    {"key": "DEMO_USER"}
                ],
                "credentials": [{
                    "provider": "demo",
                    "scopes": ["repo"],
                    "inject": {"DEMO_TOKEN": "access_token", "DEMO_USER": "account"}
                }]
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
            .env("DMCP_OAUTH_CLIENT_ID_DEMO", "test-client")
            .env_remove("DMCP_ELEVATION_DELEGATED")
            .current_dir(&self.root);
        c
    }

    fn dmcp(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().expect("run dmcp")
    }

    fn login_json(&self) -> (Output, Vec<serde_json::Value>) {
        let out = self.dmcp(&["login", "demo", "--for", SERVER, "--json"]);
        let events = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("not JSON: {l}")))
            .collect();
        (out, events)
    }

    fn whoami(&self) -> Output {
        self.dmcp(&["call", SERVER, "whoami"])
    }

    fn requests(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(&self.provider.as_ref().unwrap().log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn credentials_file(&self) -> PathBuf {
        self.root.join("user/credentials.json")
    }

    fn accounts_file(&self) -> PathBuf {
        self.root.join("user/accounts.json")
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        self.provider.take();
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
fn a_server_that_declares_an_account_is_refused_before_anyone_signs_in() {
    let env = TestEnv::new("ok");
    let out = env.whoami();
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    let line = required_line(&out);
    assert_eq!(line["reason"], "no_account");
    assert_eq!(line["provider"], "demo");
    assert_eq!(line["scopes"], serde_json::json!(["repo"]));
    assert!(stderr(&out).contains("dmcp login demo --for com.test.env"));
}

#[test]
fn listing_tools_does_not_need_the_account() {
    let env = TestEnv::new("ok");
    let out = env.dmcp(&["tools", SERVER]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("whoami"));
}

#[test]
fn signing_in_for_a_server_delivers_the_account_to_it() {
    let env = TestEnv::new("ok");
    let (out, events) = env.login_json();
    assert!(out.status.success(), "stderr: {}", stderr(&out));

    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0]["type"], "device_code");
    assert_eq!(events[0]["user_code"], "WDJB-MJHT");
    assert_eq!(
        events[0]["verification_uri"],
        format!("{}/activate", env.base())
    );
    assert_eq!(events[1]["type"], "result");
    assert_eq!(events[1]["status"], "signed_in");
    assert_eq!(events[1]["account"], "octocat");
    assert_eq!(events[1]["granted_to"], SERVER);
    assert!(
        !stdout(&out).contains(TOKEN) && !stderr(&out).contains(TOKEN),
        "login never prints the token"
    );

    // What was sent: the overridden client id, the server's declared scope.
    let requests = env.requests();
    let device = requests.iter().find(|r| r["path"] == "/device").unwrap();
    assert_eq!(device["form"]["client_id"], "test-client");
    assert_eq!(device["form"]["scope"], "repo");
    let polls = requests.iter().filter(|r| r["path"] == "/token").count();
    assert_eq!(polls, 2, "one pending poll, then the token");

    let out = env.whoami();
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let seen: serde_json::Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_eq!(seen["DEMO_TOKEN"], TOKEN);
    assert_eq!(seen["DEMO_USER"], "octocat");

    // Neither the index nor the listing holds the secret.
    let index = std::fs::read_to_string(env.accounts_file()).unwrap();
    assert!(!index.contains(TOKEN));
    let listing = env.dmcp(&["accounts", "--json"]);
    assert!(!stdout(&listing).contains(TOKEN));
    let listing: serde_json::Value = serde_json::from_slice(&listing.stdout).unwrap();
    assert_eq!(
        listing["accounts"][0]["granted_to"],
        serde_json::json!([SERVER])
    );
    assert_eq!(listing["accounts"][0]["store"], "file");
}

#[cfg(unix)]
#[test]
fn the_stored_token_and_index_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new("ok");
    assert!(env.login_json().0.status.success());
    for path in [env.credentials_file(), env.accounts_file()] {
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", path.display());
    }
}

#[test]
fn a_revoked_grant_stops_delivery_and_a_new_grant_restores_it() {
    let env = TestEnv::new("ok");
    assert!(env.login_json().0.status.success());

    let out = env.dmcp(&["grant", SERVER, "demo", "--revoke"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let out = env.whoami();
    assert_eq!(out.status.code(), Some(3));
    let line = required_line(&out);
    assert_eq!(line["reason"], "not_granted");
    assert_eq!(line["account"], "octocat");

    let out = env.dmcp(&["grant", SERVER, "demo"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert!(env.whoami().status.success());
}

#[test]
fn a_hand_set_value_wins_over_the_account() {
    let env = TestEnv::new("ok");
    assert!(env.login_json().0.status.success());
    let out = env.dmcp(&["config", SERVER, "set", "DEMO_TOKEN", "pat-by-hand"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let seen: serde_json::Value = serde_json::from_str(stdout(&env.whoami()).trim()).unwrap();
    assert_eq!(seen["DEMO_TOKEN"], "pat-by-hand");
    assert_eq!(
        seen["DEMO_USER"], "octocat",
        "the unset key is still filled"
    );
}

#[test]
fn signing_out_removes_the_token_and_every_grant() {
    let env = TestEnv::new("ok");
    assert!(env.login_json().0.status.success());
    let out = env.dmcp(&["logout", "demo"]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));

    let stored = std::fs::read_to_string(env.credentials_file()).unwrap();
    assert!(!stored.contains(TOKEN), "token left behind: {stored}");
    let listing: serde_json::Value =
        serde_json::from_slice(&env.dmcp(&["accounts", "--json"]).stdout).unwrap();
    assert_eq!(listing["accounts"], serde_json::json!([]));
    assert_eq!(required_line(&env.whoami())["reason"], "no_account");
}

#[test]
fn a_declined_sign_in_stores_nothing() {
    let env = TestEnv::new("deny");
    let (out, events) = env.login_json();
    assert!(!out.status.success());
    assert_eq!(events.last().unwrap()["status"], "denied");
    assert!(!env.credentials_file().exists());
    assert!(!env.accounts_file().exists());
}

/// An expired token is refreshed at spawn against the endpoint recorded at
/// sign-in, and the server receives the new one.
#[test]
fn an_expired_token_is_refreshed_before_the_server_starts() {
    let env = TestEnv::new("ok");
    assert!(env.login_json().0.status.success());
    let path = env.credentials_file();
    let mut stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    stored["demo/octocat"]["refresh_token"] = serde_json::json!("rt-1");
    stored["demo/octocat"]["expires_at"] = serde_json::json!(1);
    std::fs::write(&path, stored.to_string()).unwrap();

    let out = env.whoami();
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let seen: serde_json::Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_eq!(seen["DEMO_TOKEN"], "gho_fake_refreshed_token");

    let refresh = env
        .requests()
        .into_iter()
        .find(|r| r["form"]["grant_type"] == "refresh_token")
        .expect("a refresh request");
    assert_eq!(refresh["form"]["refresh_token"], "rt-1");
    assert_eq!(refresh["form"]["client_id"], "test-client");

    let stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        stored["demo/octocat"]["access_token"],
        "gho_fake_refreshed_token"
    );
    assert_eq!(stored["demo/octocat"]["refresh_token"], "rt-1");
}

/// Each endpoint on its own: one plain-http endpoint among loopback ones is
/// enough to refuse, before anything is sent anywhere.
#[test]
fn a_plain_http_provider_endpoint_is_refused_before_any_request() {
    let env = TestEnv::new("ok");
    let good = env.base();
    let bad = "http://auth.example.invalid";
    for (which, bases) in [
        ("device", [bad, &good, &good]),
        ("token", [&good, bad, &good]),
        ("identity", [&good, &good, bad]),
    ] {
        env.write_registry(bases);
        let (out, events) = env.login_json();
        assert!(!out.status.success(), "{which}");
        let message = events.last().unwrap()["message"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(message.contains("must be https"), "{which}: {message}");
        assert!(env.requests().is_empty(), "{which}: a request was sent");
    }
}

#[test]
fn signing_in_for_a_server_that_does_not_declare_the_provider_is_refused() {
    let env = TestEnv::new("ok");
    let out = env.dmcp(&["login", "demo", "--for", "com.test.missing", "--json"]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains("is not installed"));
    assert!(
        env.requests().is_empty(),
        "no one is sent to sign in for nothing"
    );
}
