//! Signing in (Project-JARVIS#229): dmcp as the OAuth client, the same role
//! Claude Desktop or VS Code play for a hosted MCP server.
//!
//! v1 is the device flow (RFC 8628). It needs no redirect listener, so it
//! works over SSH, headless, and when a daemon drives dmcp: the user opens a
//! URL anywhere, types a short code, and approves. That the user had to type
//! the code is also why a sign-in may grant the account to a server — someone
//! was demonstrably there.
//!
//! Providers come from the registry (`registry.json` `providers`), so a user
//! is only ever sent to endpoints the registry reviewed. Endpoints must be
//! https; plain http is accepted only on a loopback host, which is what a test
//! server looks like and never what a real provider looks like.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::accounts::{
    self, load_accounts, now_unix, open_store, save_accounts, AccountRecord, AccountsError,
    Refresher, Secret, StoreKind,
};
use crate::discovery::{get_server, Scope};
use crate::paths::Paths;

/// A sign-in provider as the registry declares it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub oauth: OAuthEndpoints,
    pub identity: Identity,
    #[serde(default)]
    pub scopes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthEndpoints {
    #[serde(default)]
    pub client_id: Option<String>,
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Identity {
    pub url: String,
    pub field: String,
}

/// What the user must do to finish signing in.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceCode {
    pub provider: String,
    pub provider_name: String,
    pub verification_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    pub expires_in: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginOutcome {
    pub provider: String,
    pub account: String,
    pub scopes: Vec<String>,
    pub store: StoreKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub granted_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug)]
pub enum LoginError {
    NoSources,
    UnknownProvider(String),
    NoClientId(String),
    InsecureEndpoint(String),
    UnknownScope(String, String),
    ServerNotInstalled(String),
    NotDeclared(String, String),
    SystemScope(String),
    Denied,
    Expired,
    Http(String),
    Protocol(String),
    UnknownAccount(String, String),
    NoAccount(String),
    AmbiguousAccount(String, Vec<String>),
    Accounts(AccountsError),
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoginError::NoSources => write!(f, "no registry sources are configured"),
            LoginError::UnknownProvider(p) => {
                write!(
                    f,
                    "no configured registry declares a sign-in provider '{p}'"
                )
            }
            LoginError::NoClientId(p) => write!(
                f,
                "the '{p}' provider has no registered OAuth client id yet; set {} to a \
                 client id with device flow enabled",
                client_id_env(p)
            ),
            LoginError::InsecureEndpoint(u) => write!(
                f,
                "refusing to send a sign-in to {u}: provider endpoints must be https"
            ),
            LoginError::UnknownScope(p, s) => {
                write!(f, "'{s}' is not a scope the '{p}' provider offers")
            }
            LoginError::ServerNotInstalled(s) => write!(f, "server '{s}' is not installed"),
            LoginError::NotDeclared(s, p) => {
                write!(f, "server '{s}' does not declare a '{p}' credential")
            }
            LoginError::SystemScope(s) => write!(
                f,
                "server '{s}' is system-scope; signed-in accounts are only delivered to \
                 user-scope servers"
            ),
            LoginError::Denied => write!(f, "sign-in was declined"),
            LoginError::Expired => write!(f, "the sign-in code expired before it was used"),
            LoginError::Http(e) => write!(f, "request failed: {e}"),
            LoginError::Protocol(e) => write!(f, "the provider answered unexpectedly: {e}"),
            LoginError::UnknownAccount(p, a) => write!(f, "no '{p}' account '{a}' is signed in"),
            LoginError::NoAccount(p) => write!(f, "no '{p}' account is signed in"),
            LoginError::AmbiguousAccount(p, all) => write!(
                f,
                "several '{p}' accounts are signed in ({}); pick one with --account",
                all.join(", ")
            ),
            LoginError::Accounts(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LoginError {}

impl From<AccountsError> for LoginError {
    fn from(e: AccountsError) -> Self {
        LoginError::Accounts(e)
    }
}

/// Short machine-readable name for `--json` results.
impl LoginError {
    pub fn status(&self) -> &'static str {
        match self {
            LoginError::Denied => "denied",
            LoginError::Expired => "expired",
            _ => "error",
        }
    }
}

/// `DMCP_OAUTH_CLIENT_ID_<ID>`: a client id that wins over the registry's —
/// for testing with a personal OAuth app, or before one is registered.
pub fn client_id_env(provider: &str) -> String {
    format!(
        "DMCP_OAUTH_CLIENT_ID_{}",
        provider.to_ascii_uppercase().replace('-', "_")
    )
}

/// https anywhere; http only on a loopback host.
///
/// Parsed, not pattern-matched: userinfo (`http://127.0.0.1@evil.example`)
/// and look-alike hosts are exactly where a hand-rolled check goes wrong.
pub fn check_endpoint(url: &str) -> Result<(), LoginError> {
    let insecure = || LoginError::InsecureEndpoint(url.to_string());
    let parsed = url::Url::parse(url).map_err(|_| insecure())?;
    match parsed.scheme() {
        "https" if parsed.host().is_some() => Ok(()),
        "http" => match parsed.host() {
            Some(url::Host::Domain("localhost")) => Ok(()),
            Some(url::Host::Ipv4(ip)) if ip.is_loopback() => Ok(()),
            Some(url::Host::Ipv6(ip)) if ip.is_loopback() => Ok(()),
            _ => Err(insecure()),
        },
        _ => Err(insecure()),
    }
}

fn http_client() -> Result<reqwest::blocking::Client, LoginError> {
    reqwest::blocking::Client::builder()
        .user_agent("dmcp/1.0")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| LoginError::Http(e.to_string()))
}

/// The first configured registry that declares `id`.
pub fn find_provider(paths: &Paths, id: &str) -> Result<Provider, LoginError> {
    let sources = crate::sources::list_sources(paths, true, true);
    if sources.is_empty() {
        return Err(LoginError::NoSources);
    }
    let client = http_client()?;
    for (url, _) in sources {
        let Ok(registry) = crate::update::fetch_registry_value(&client, &url) else {
            continue;
        };
        if let Some(value) = registry.get("providers").and_then(|p| p.get(id)) {
            let provider: Provider = serde_json::from_value(value.clone())
                .map_err(|e| LoginError::Protocol(format!("provider '{id}' in {url}: {e}")))?;
            check_endpoint(&provider.oauth.device_authorization_endpoint)?;
            check_endpoint(&provider.oauth.token_endpoint)?;
            check_endpoint(&provider.identity.url)?;
            return Ok(provider);
        }
    }
    Err(LoginError::UnknownProvider(id.to_string()))
}

fn client_id(provider: &Provider) -> Result<String, LoginError> {
    std::env::var(client_id_env(&provider.id))
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| provider.oauth.client_id.clone())
        .ok_or_else(|| LoginError::NoClientId(provider.id.clone()))
}

/// Check that `server` can receive a `provider` account, before anyone is sent
/// to sign in for it. Returns the scopes it declares.
pub fn server_scopes(
    paths: &Paths,
    server: &str,
    provider: &str,
) -> Result<Vec<String>, LoginError> {
    let (manifest, scope) =
        get_server(paths, server).ok_or_else(|| LoginError::ServerNotInstalled(server.into()))?;
    if scope == Scope::System {
        return Err(LoginError::SystemScope(server.into()));
    }
    manifest
        .credentials
        .iter()
        .find(|d| d.provider == provider)
        .map(|d| d.scopes.clone())
        .ok_or_else(|| LoginError::NotDeclared(server.into(), provider.into()))
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    // Google spells it verification_url.
    #[serde(alias = "verification_url")]
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

impl TokenResponse {
    fn into_secret(self, now: u64) -> Option<(Secret, Option<String>)> {
        let access_token = self.access_token.filter(|t| !t.is_empty())?;
        Some((
            Secret {
                access_token,
                refresh_token: self.refresh_token.filter(|t| !t.is_empty()),
                expires_at: self.expires_in.map(|s| now + s),
            },
            self.scope,
        ))
    }
}

/// POST a form and read the JSON answer whatever the status: providers report
/// `authorization_pending` as a 400 (RFC 8628) or as a 200 (GitHub).
fn post_form(
    client: &reqwest::blocking::Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<serde_json::Value, LoginError> {
    let resp = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(form)
        .send()
        .map_err(|e| LoginError::Http(e.to_string()))?;
    let status = resp.status();
    let body = resp.text().map_err(|e| LoginError::Http(e.to_string()))?;
    serde_json::from_str(&body)
        .map_err(|_| LoginError::Protocol(format!("{url} answered {status} with no JSON body")))
}

fn split_scopes(raw: &str) -> Vec<String> {
    raw.split([',', ' '])
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Sign in to `provider_id` and, with `for_server`, grant it the account.
///
/// Asks for the scopes `for_server` declares, any `extra_scopes`, and the
/// scopes of servers already granted this provider — so signing in again for
/// one server never quietly strips another of access it had.
pub fn login(
    paths: &Paths,
    provider_id: &str,
    extra_scopes: &[String],
    for_server: Option<&str>,
    on_code: &mut dyn FnMut(&DeviceCode),
) -> Result<LoginOutcome, LoginError> {
    let mut wanted: BTreeSet<String> = extra_scopes.iter().cloned().collect();
    if let Some(server) = for_server {
        wanted.extend(server_scopes(paths, server, provider_id)?);
    }

    let provider = find_provider(paths, provider_id)?;
    for scope in &wanted {
        if !provider.scopes.contains_key(scope) {
            return Err(LoginError::UnknownScope(provider.id.clone(), scope.clone()));
        }
    }
    wanted.extend(accounts::granted_scopes(paths, provider_id));
    let client_id = client_id(&provider)?;
    let scope_param = wanted.iter().cloned().collect::<Vec<_>>().join(" ");

    let client = http_client()?;
    let auth: DeviceAuthorization = serde_json::from_value(post_form(
        &client,
        &provider.oauth.device_authorization_endpoint,
        &[("client_id", &client_id), ("scope", &scope_param)],
    )?)
    .map_err(|e| LoginError::Protocol(format!("device authorization: {e}")))?;
    check_endpoint(&auth.verification_uri)?;

    on_code(&DeviceCode {
        provider: provider.id.clone(),
        provider_name: provider.name.clone(),
        verification_uri: auth.verification_uri.clone(),
        verification_uri_complete: auth.verification_uri_complete.clone(),
        user_code: auth.user_code.clone(),
        expires_in: auth.expires_in,
    });

    let (secret, granted) = poll_for_token(&client, &provider, &client_id, &auth)?;
    let account = identify(&client, &provider, &secret.access_token)?;
    let scopes = match granted.as_deref().map(split_scopes) {
        Some(s) if !s.is_empty() => s,
        _ => wanted.into_iter().collect(),
    };

    let (store_kind, store, warning) = accounts::store_for_new_login(paths)?;
    store.set(&provider.id, &account, &secret)?;

    let mut index = load_accounts(paths)?;
    // A re-sign-in that lands in a different store must not leave the old copy.
    if let Some(previous) = index.find(&provider.id, &account) {
        if previous.store != store_kind {
            if let Ok(old) = open_store(previous.store, paths) {
                let _ = old.delete(&provider.id, &account);
            }
        }
    }
    index.upsert(AccountRecord {
        provider: provider.id.clone(),
        account: account.clone(),
        scopes: scopes.clone(),
        store: store_kind,
        token_endpoint: provider.oauth.token_endpoint.clone(),
        client_id: Some(client_id),
        signed_in_at: now_unix(),
    });
    if let Some(server) = for_server {
        index.grant(server, &provider.id, &account);
    }
    save_accounts(paths, &index)?;

    Ok(LoginOutcome {
        provider: provider.id,
        account,
        scopes,
        store: store_kind,
        granted_to: for_server.map(str::to_string),
        warning,
    })
}

fn poll_for_token(
    client: &reqwest::blocking::Client,
    provider: &Provider,
    client_id: &str,
    auth: &DeviceAuthorization,
) -> Result<(Secret, Option<String>), LoginError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(auth.expires_in);
    let mut interval = auth.interval.unwrap_or(5).max(1);
    loop {
        std::thread::sleep(Duration::from_secs(interval));
        if std::time::Instant::now() > deadline {
            return Err(LoginError::Expired);
        }
        let answer: TokenResponse = serde_json::from_value(post_form(
            client,
            &provider.oauth.token_endpoint,
            &[
                ("client_id", client_id),
                ("device_code", &auth.device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        )?)
        .map_err(|e| LoginError::Protocol(format!("token: {e}")))?;

        match answer.error.as_deref() {
            None => {
                return answer.into_secret(now_unix()).ok_or_else(|| {
                    LoginError::Protocol("token response carried no access_token".into())
                })
            }
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("access_denied") => return Err(LoginError::Denied),
            Some("expired_token") => return Err(LoginError::Expired),
            Some(other) => {
                return Err(LoginError::Protocol(match answer.error_description {
                    Some(d) => format!("{other}: {d}"),
                    None => other.to_string(),
                }))
            }
        }
    }
}

/// Name the account the token belongs to, from the provider's identity URL.
fn identify(
    client: &reqwest::blocking::Client,
    provider: &Provider,
    access_token: &str,
) -> Result<String, LoginError> {
    let resp = client
        .get(&provider.identity.url)
        .bearer_auth(access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .map_err(|e| LoginError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(LoginError::Protocol(format!(
            "identity endpoint answered {}",
            resp.status()
        )));
    }
    let body: serde_json::Value = resp.json().map_err(|e| LoginError::Http(e.to_string()))?;
    match body.get(&provider.identity.field) {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(serde_json::Value::Number(n)) => Ok(n.to_string()),
        _ => Err(LoginError::Protocol(format!(
            "identity has no '{}' field",
            provider.identity.field
        ))),
    }
}

/// Refreshes over HTTP against the endpoint recorded at sign-in.
pub struct HttpRefresher;

impl Refresher for HttpRefresher {
    fn refresh(&self, record: &AccountRecord, refresh_token: &str) -> Result<Secret, String> {
        check_endpoint(&record.token_endpoint).map_err(|e| e.to_string())?;
        let client = http_client().map_err(|e| e.to_string())?;
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        if let Some(id) = record.client_id.as_deref() {
            form.push(("client_id", id));
        }
        let answer: TokenResponse = serde_json::from_value(
            post_form(&client, &record.token_endpoint, &form).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if let Some(error) = answer.error {
            return Err(error);
        }
        answer
            .into_secret(now_unix())
            .map(|(secret, _)| secret)
            .ok_or_else(|| "refresh answered with no access_token".to_string())
    }
}

/// The one account of `provider`, or the named one.
fn pick_account(
    paths: &Paths,
    provider: &str,
    account: Option<&str>,
) -> Result<String, LoginError> {
    let index = load_accounts(paths)?;
    if let Some(a) = account {
        return index
            .find(provider, a)
            .map(|r| r.account.clone())
            .ok_or_else(|| LoginError::UnknownAccount(provider.into(), a.into()));
    }
    let all: Vec<String> = index
        .for_provider(provider)
        .iter()
        .map(|r| r.account.clone())
        .collect();
    match all.as_slice() {
        [] => Err(LoginError::NoAccount(provider.into())),
        [one] => Ok(one.clone()),
        _ => Err(LoginError::AmbiguousAccount(provider.into(), all)),
    }
}

/// Forget an account: its secret, its record, and every grant to it.
pub fn logout(
    paths: &Paths,
    provider: &str,
    account: Option<&str>,
) -> Result<AccountRecord, LoginError> {
    let account = pick_account(paths, provider, account)?;
    let mut index = load_accounts(paths)?;
    let record = index
        .remove_account(provider, &account)
        .ok_or_else(|| LoginError::UnknownAccount(provider.into(), account.clone()))?;
    // The index goes first: a secret nothing points at is inert, while a record
    // pointing at a deleted secret would keep being offered to servers.
    save_accounts(paths, &index)?;
    open_store(record.store, paths)?.delete(provider, &account)?;
    Ok(record)
}

/// Give `server` the `provider` account. Returns the account granted.
pub fn grant(
    paths: &Paths,
    server: &str,
    provider: &str,
    account: Option<&str>,
) -> Result<String, LoginError> {
    let declared = server_scopes(paths, server, provider)?;
    let account = pick_account(paths, provider, account)?;
    let mut index = load_accounts(paths)?;
    if let Some(record) = index.find(provider, &account) {
        if let Some(missing) = declared.iter().find(|s| !record.scopes.contains(s)) {
            return Err(LoginError::Protocol(format!(
                "account '{account}' was not granted scope '{missing}' that '{server}' needs; \
                 run: dmcp login {provider} --for {server}"
            )));
        }
    }
    index.grant(server, provider, &account);
    save_accounts(paths, &index)?;
    Ok(account)
}

/// Take `provider` access away from `server`. True when it had a grant.
pub fn revoke(paths: &Paths, server: &str, provider: &str) -> Result<bool, LoginError> {
    let mut index = load_accounts(paths)?;
    let removed = index.revoke(server, provider);
    if removed {
        save_accounts(paths, &index)?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_must_be_https_except_on_loopback() {
        assert!(check_endpoint("https://github.com/login/device/code").is_ok());
        assert!(check_endpoint("http://127.0.0.1:8080/token").is_ok());
        assert!(check_endpoint("http://localhost/token").is_ok());
        assert!(check_endpoint("http://[::1]:9/token").is_ok());
        assert!(check_endpoint("http://[::1]/token").is_ok());
        assert!(check_endpoint("http://[::1].evil.example/token").is_err());
        assert!(check_endpoint("http://127.0.0.1@evil.example/token").is_err());
        assert!(check_endpoint("http://github.com/login/device/code").is_err());
        assert!(check_endpoint("http://127.0.0.1.evil.example/token").is_err());
        assert!(check_endpoint("http://localhost.evil.example/token").is_err());
        assert!(check_endpoint("ftp://127.0.0.1/token").is_err());
    }

    #[test]
    fn client_id_env_name_is_a_valid_variable() {
        assert_eq!(client_id_env("github"), "DMCP_OAUTH_CLIENT_ID_GITHUB");
        assert_eq!(
            client_id_env("google-work"),
            "DMCP_OAUTH_CLIENT_ID_GOOGLE_WORK"
        );
    }

    #[test]
    fn scopes_split_on_commas_and_spaces() {
        assert_eq!(split_scopes("repo,gist"), vec!["repo", "gist"]);
        assert_eq!(split_scopes("a b  c"), vec!["a", "b", "c"]);
        assert!(split_scopes("").is_empty());
    }
}
