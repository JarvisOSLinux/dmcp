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

/// What the user must open to finish a hosted server's sign-in.
#[derive(Debug, Clone, Serialize)]
pub struct AuthorizeUrl {
    pub server: String,
    pub url: String,
    /// Seconds dmcp waits for the browser to come back.
    pub expires_in: u64,
}

/// What a sign-in needs from the user. Serialized with a `type` tag, which is
/// the shape `dmcp login --json` prints.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoginPrompt {
    /// Type a code on the provider's page (device flow).
    DeviceCode(DeviceCode),
    /// Approve in a browser, which returns to dmcp (a hosted server's OAuth).
    Authorize(AuthorizeUrl),
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
    NothingToSignIn(String),
    NameTheProvider(String, Vec<String>),
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
            LoginError::NothingToSignIn(s) => write!(
                f,
                "server '{s}' declares no account and no sign-in of its own"
            ),
            LoginError::NameTheProvider(s, all) => write!(
                f,
                "server '{s}' uses several accounts ({}); name the provider: dmcp login <provider> --for {s}",
                all.join(", ")
            ),
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
                client_secret: None,
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
    on_prompt: &mut dyn FnMut(&LoginPrompt),
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

    on_prompt(&LoginPrompt::DeviceCode(DeviceCode {
        provider: provider.id.clone(),
        provider_name: provider.name.clone(),
        verification_uri: auth.verification_uri.clone(),
        verification_uri_complete: auth.verification_uri_complete.clone(),
        user_code: auth.user_code.clone(),
        expires_in: auth.expires_in,
    }));

    let (secret, granted) = poll_for_token(&client, &provider, &client_id, &auth)?;
    let account = identify(&client, &provider, &secret.access_token)?;
    let scopes = match granted.as_deref().map(split_scopes) {
        Some(s) if !s.is_empty() => s,
        _ => wanted.into_iter().collect(),
    };

    let (store_kind, warning) = save_account(
        paths,
        AccountRecord {
            provider: provider.id.clone(),
            account: account.clone(),
            scopes: scopes.clone(),
            store: StoreKind::File,
            token_endpoint: provider.oauth.token_endpoint.clone(),
            client_id: Some(client_id),
            signed_in_at: now_unix(),
            hosted: false,
            resource: None,
        },
        &secret,
        for_server,
    )?;

    Ok(LoginOutcome {
        provider: provider.id,
        account,
        scopes,
        store: store_kind,
        granted_to: for_server.map(str::to_string),
        warning,
    })
}

/// Store a fresh sign-in: the secret in the store a new login goes to, the
/// record and any grant in the index. `record.store` is overwritten with the
/// store actually used. Returns that store and any warning about it.
fn save_account(
    paths: &Paths,
    mut record: AccountRecord,
    secret: &Secret,
    grant_to: Option<&str>,
) -> Result<(StoreKind, Option<String>), LoginError> {
    let (store_kind, store, warning) = accounts::store_for_new_login(paths)?;
    store.set(&record.provider, &record.account, secret)?;

    let mut index = load_accounts(paths)?;
    // A re-sign-in that lands in a different store must not leave the old copy.
    if let Some(previous) = index.find(&record.provider, &record.account) {
        if previous.store != store_kind {
            if let Ok(old) = open_store(previous.store, paths) {
                let _ = old.delete(&record.provider, &record.account);
            }
        }
    }
    record.store = store_kind;
    if let Some(server) = grant_to {
        index.grant(server, &record.provider, &record.account);
    }
    index.upsert(record);
    save_accounts(paths, &index)?;
    Ok((store_kind, warning))
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

/// Sign in for `server`, choosing how from its manifest: a named provider is
/// the device flow; with none named, a hosted server's own OAuth, or the one
/// provider the server declares.
pub fn login_for(
    paths: &Paths,
    provider: Option<&str>,
    extra_scopes: &[String],
    for_server: Option<&str>,
    open_browser: bool,
    on_prompt: &mut dyn FnMut(&LoginPrompt),
) -> Result<LoginOutcome, LoginError> {
    let hosted = match for_server {
        Some(server) if provider.is_none() || provider == Some(server) => {
            let (manifest, _) = get_server(paths, server)
                .ok_or_else(|| LoginError::ServerNotInstalled(server.into()))?;
            let url = crate::transport::select(manifest.transports.as_deref())
                .ok()
                .and_then(|t| t.oauth_url())
                .map(str::to_string);
            match (url, provider) {
                (Some(url), _) => Some((server, url)),
                (None, Some(p)) => return login(paths, p, extra_scopes, for_server, on_prompt),
                (None, None) => {
                    return match manifest.credentials.as_slice() {
                        [] => Err(LoginError::NothingToSignIn(server.into())),
                        [only] => login(paths, &only.provider, extra_scopes, for_server, on_prompt),
                        all => Err(LoginError::NameTheProvider(
                            server.into(),
                            all.iter().map(|d| d.provider.clone()).collect(),
                        )),
                    }
                }
            }
        }
        _ => None,
    };
    match (hosted, provider) {
        (Some((server, url)), _) => login_hosted(paths, server, &url, open_browser, on_prompt),
        (None, Some(p)) => login(paths, p, extra_scopes, for_server, on_prompt),
        (None, None) => Err(LoginError::NothingToSignIn(
            "(none given: use --for <server> or name a provider)".into(),
        )),
    }
}

/// How long a hosted sign-in waits for the browser to come back.
fn hosted_timeout() -> u64 {
    std::env::var("DMCP_LOGIN_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&s: &u64| s > 0)
        .unwrap_or(300)
}

fn auth_error(e: rmcp::transport::auth::AuthError) -> LoginError {
    LoginError::Protocol(e.to_string())
}

/// Sign in to a hosted server by the MCP authorization spec.
///
/// The server names its authorization server (RFC 9728); dmcp registers itself
/// there (RFC 7591), which is why no one has to register an app first, and
/// sends the user to approve with PKCE. The browser comes back to a one-shot
/// listener on 127.0.0.1. rmcp does discovery, registration, the PKCE URL and
/// the code exchange; the token is then kept like any other account, and
/// refreshed by dmcp rather than rmcp, whose refresh cannot tell a stored
/// token's age.
pub fn login_hosted(
    paths: &Paths,
    server: &str,
    resource: &str,
    open_browser: bool,
    on_prompt: &mut dyn FnMut(&LoginPrompt),
) -> Result<LoginOutcome, LoginError> {
    check_endpoint(resource)?;
    let timeout = hosted_timeout();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| LoginError::Http(e.to_string()))?;
    let signed_in = runtime.block_on(async {
        use rmcp::transport::auth::AuthorizationManager;

        let mut manager = AuthorizationManager::new(resource)
            .await
            .map_err(auth_error)?;
        let metadata = manager.discover_metadata().await.map_err(auth_error)?;
        check_endpoint(&metadata.authorization_endpoint)?;
        check_endpoint(&metadata.token_endpoint)?;
        if let Some(registration) = &metadata.registration_endpoint {
            check_endpoint(registration)?;
        }
        let token_endpoint = metadata.token_endpoint.clone();
        manager.set_metadata(metadata);

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| LoginError::Http(e.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|e| LoginError::Http(e.to_string()))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");

        let client = manager
            .register_client("JARVIS (dmcp)", &redirect_uri)
            .await
            .map_err(auth_error)?;
        let scopes = manager.select_scopes(None, &[]);
        manager
            .configure_client(client.clone())
            .map_err(auth_error)?;
        let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();
        let url = manager
            .get_authorization_url(&scope_refs)
            .await
            .map_err(auth_error)?;
        check_endpoint(&url)?;
        let state = url::Url::parse(&url)
            .ok()
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "state")
                    .map(|(_, v)| v.into_owned())
            })
            .ok_or_else(|| LoginError::Protocol("authorization URL carries no state".into()))?;

        on_prompt(&LoginPrompt::Authorize(AuthorizeUrl {
            server: server.to_string(),
            url: url.clone(),
            expires_in: timeout,
        }));
        if open_browser {
            open_in_browser(&url);
        }

        let code = tokio::time::timeout(
            Duration::from_secs(timeout),
            wait_for_callback(&listener, &state),
        )
        .await
        .map_err(|_| LoginError::Expired)??;

        let token = manager
            .exchange_code_for_token(&code, &state)
            .await
            .map_err(auth_error)?;
        // Through serde rather than the oauth2 traits: the standard token
        // response serializes to exactly the RFC 6749 fields we read.
        let answer: TokenResponse = serde_json::to_value(&token)
            .and_then(serde_json::from_value)
            .map_err(|e| LoginError::Protocol(format!("token: {e}")))?;
        let (mut secret, granted) = answer
            .into_secret(now_unix())
            .ok_or_else(|| LoginError::Protocol("token response carried no access_token".into()))?;
        secret.client_secret = client.client_secret.clone();
        let scopes = match granted.as_deref().map(split_scopes) {
            Some(s) if !s.is_empty() => s,
            _ => scopes,
        };
        Ok::<_, LoginError>((secret, scopes, client.client_id, token_endpoint))
    })?;
    let (secret, scopes, client_id, token_endpoint) = signed_in;

    let (store_kind, warning) = save_account(
        paths,
        AccountRecord {
            provider: server.to_string(),
            account: accounts::HOSTED_ACCOUNT.to_string(),
            scopes: scopes.clone(),
            store: StoreKind::File,
            token_endpoint,
            client_id: Some(client_id),
            signed_in_at: now_unix(),
            hosted: true,
            resource: Some(resource.to_string()),
        },
        &secret,
        Some(server),
    )?;
    Ok(LoginOutcome {
        provider: server.to_string(),
        account: accounts::HOSTED_ACCOUNT.to_string(),
        scopes,
        store: store_kind,
        granted_to: Some(server.to_string()),
        warning,
    })
}

const CALLBACK_DONE: &str = "<!doctype html><title>Signed in</title><p>Signed in. You can close this tab and return to JARVIS.</p>";
const CALLBACK_FAILED: &str = "<!doctype html><title>Sign-in not completed</title><p>The sign-in was not completed. You can close this tab.</p>";

/// Serve the loopback redirect until the browser comes back with this
/// sign-in's `state`. Anything else — a favicon request, a stray or forged
/// callback — is answered and ignored, so it can neither finish nor abort
/// the sign-in.
async fn wait_for_callback(
    listener: &tokio::net::TcpListener,
    state: &str,
) -> Result<String, LoginError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| LoginError::Http(e.to_string()))?;
        let mut buf = vec![0u8; 8192];
        let mut len = 0;
        while len < buf.len() {
            match stream.read(&mut buf[len..]).await {
                Ok(0) | Err(_) => break,
                Ok(n) => len += n,
            }
            if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&buf[..len]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("GET "))
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or("");
        let outcome = callback_outcome(target, state);
        let (status, body) = match &outcome {
            Some(Ok(_)) => ("200 OK", CALLBACK_DONE),
            Some(Err(_)) => ("200 OK", CALLBACK_FAILED),
            None => ("404 Not Found", ""),
        };
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(reply.as_bytes()).await;
        let _ = stream.shutdown().await;
        if let Some(outcome) = outcome {
            return outcome;
        }
    }
}

/// What one request to the loopback listener means: `None` to keep waiting,
/// or the code / the reason the sign-in ended.
fn callback_outcome(target: &str, state: &str) -> Option<Result<String, LoginError>> {
    let url = url::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    if url.path() != "/callback" {
        return None;
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    if param("state").as_deref() != Some(state) {
        return None;
    }
    if let Some(error) = param("error") {
        return Some(Err(if error == "access_denied" {
            LoginError::Denied
        } else {
            LoginError::Protocol(match param("error_description") {
                Some(d) => format!("{error}: {d}"),
                None => error,
            })
        }));
    }
    param("code").map(Ok)
}

/// Best effort: the URL is printed either way, so a missing opener only
/// costs a click.
fn open_in_browser(url: &str) {
    use std::process::{Command, Stdio};
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(windows)]
    let mut command = {
        // Not `cmd /c start`: cmd would read the URL's `&` as a command separator.
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = Command::new("xdg-open");
    let _ = command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Refreshes over HTTP against the endpoint recorded at sign-in.
pub struct HttpRefresher;

impl Refresher for HttpRefresher {
    fn refresh(
        &self,
        record: &AccountRecord,
        refresh_token: &str,
        client_secret: Option<&str>,
    ) -> Result<Secret, String> {
        check_endpoint(&record.token_endpoint).map_err(|e| e.to_string())?;
        let client = http_client().map_err(|e| e.to_string())?;
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        if let Some(id) = record.client_id.as_deref() {
            form.push(("client_id", id));
        }
        if let Some(secret) = client_secret {
            form.push(("client_secret", secret));
        }
        if let Some(resource) = record.resource.as_deref() {
            form.push(("resource", resource));
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
    fn only_this_sign_ins_callback_counts() {
        let ok = callback_outcome("/callback?code=c1&state=s1", "s1");
        assert!(matches!(ok, Some(Ok(ref c)) if c == "c1"));
        assert!(callback_outcome("/callback?code=c1&state=other", "s1").is_none());
        assert!(callback_outcome("/callback?code=c1", "s1").is_none());
        assert!(callback_outcome("/favicon.ico", "s1").is_none());
        assert!(callback_outcome("/elsewhere?code=c1&state=s1", "s1").is_none());
        assert!(matches!(
            callback_outcome("/callback?error=access_denied&state=s1", "s1"),
            Some(Err(LoginError::Denied))
        ));
        assert!(matches!(
            callback_outcome("/callback?error=server_error&state=s1", "s1"),
            Some(Err(LoginError::Protocol(_)))
        ));
        // A denial for some other sign-in is not this one's answer.
        assert!(callback_outcome("/callback?error=access_denied&state=other", "s1").is_none());
    }

    #[test]
    fn scopes_split_on_commas_and_spaces() {
        assert_eq!(split_scopes("repo,gist"), vec!["repo", "gist"]);
        assert_eq!(split_scopes("a b  c"), vec!["a", "b", "c"]);
        assert!(split_scopes("").is_empty());
    }
}
