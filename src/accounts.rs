//! Signed-in accounts: where they are kept, which servers may use them, and
//! how a server gets one at spawn (Project-JARVIS#229).
//!
//! Three pieces, split by what they hold:
//!
//! - **The secret** (access token, refresh token, expiry) lives in the OS
//!   keyring — Secret Service, Keychain or Credential Manager — keyed
//!   `dmcp` / `<provider>/<account>`. Where no keyring answers (headless, a
//!   container) it falls back to an owner-only `credentials.json` beside the
//!   install tree, and says so. `DMCP_CREDENTIAL_STORE=file|keyring` forces one.
//! - **The index** (`accounts.json` beside `sources.list`) holds nothing
//!   secret: which accounts exist, their scopes, which store holds each, and
//!   the grants. Listing accounts never unlocks the keyring.
//! - **Grants** bind a server to an account. A server that *declares* a
//!   provider gets nothing until a grant exists, because installing is
//!   something the agent can do and handing it the user's GitHub token must not
//!   be. Grants are written only by `dmcp login --for` and `dmcp grant`, never
//!   through `dmcp serve`, and never into a server's `config`, which the model
//!   can reach.
//!
//! At spawn, [`credential_env`] turns a manifest's declarations into
//! environment variables, or into a [`CredentialRequired`] saying exactly what
//! is missing — before the server starts, instead of the server failing later
//! with its own 401.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::manifest_io::{create_install_dir, write_manifest_atomic, Readers};
use crate::models::Manifest;
use crate::paths::Paths;

const KEYRING_SERVICE: &str = "dmcp";
const STORE_ENV: &str = "DMCP_CREDENTIAL_STORE";

/// Refresh this long before the recorded expiry, so a token is not handed to
/// a server that will find it expired a moment later.
const EXPIRY_MARGIN_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreKind {
    Keyring,
    File,
}

impl std::fmt::Display for StoreKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            StoreKind::Keyring => "keyring",
            StoreKind::File => "file",
        })
    }
}

/// One signed-in account, as the index records it. Nothing here is secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRecord {
    pub provider: String,
    pub account: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub store: StoreKind,
    /// Kept so a refresh needs no registry fetch at spawn time.
    pub token_endpoint: String,
    #[serde(default)]
    pub client_id: Option<String>,
    pub signed_in_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountsFile {
    #[serde(default)]
    pub accounts: Vec<AccountRecord>,
    /// server id → provider id → account.
    #[serde(default)]
    pub grants: BTreeMap<String, BTreeMap<String, String>>,
}

impl AccountsFile {
    pub fn find(&self, provider: &str, account: &str) -> Option<&AccountRecord> {
        self.accounts
            .iter()
            .find(|a| a.provider == provider && a.account == account)
    }

    pub fn for_provider(&self, provider: &str) -> Vec<&AccountRecord> {
        self.accounts
            .iter()
            .filter(|a| a.provider == provider)
            .collect()
    }

    pub fn grant_for(&self, server: &str, provider: &str) -> Option<&str> {
        self.grants
            .get(server)
            .and_then(|g| g.get(provider))
            .map(String::as_str)
    }

    /// Servers granted `provider/account`.
    pub fn granted_servers(&self, provider: &str, account: &str) -> Vec<&str> {
        self.grants
            .iter()
            .filter(|(_, g)| g.get(provider).map(String::as_str) == Some(account))
            .map(|(s, _)| s.as_str())
            .collect()
    }

    pub fn upsert(&mut self, record: AccountRecord) {
        match self
            .accounts
            .iter_mut()
            .find(|a| a.provider == record.provider && a.account == record.account)
        {
            Some(existing) => *existing = record,
            None => self.accounts.push(record),
        }
    }

    pub fn grant(&mut self, server: &str, provider: &str, account: &str) {
        self.grants
            .entry(server.to_string())
            .or_default()
            .insert(provider.to_string(), account.to_string());
    }

    /// Drop a grant; true when there was one.
    pub fn revoke(&mut self, server: &str, provider: &str) -> bool {
        let Some(g) = self.grants.get_mut(server) else {
            return false;
        };
        let removed = g.remove(provider).is_some();
        if g.is_empty() {
            self.grants.remove(server);
        }
        removed
    }

    /// Forget an account and every grant that pointed at it.
    pub fn remove_account(&mut self, provider: &str, account: &str) -> Option<AccountRecord> {
        let at = self
            .accounts
            .iter()
            .position(|a| a.provider == provider && a.account == account)?;
        let record = self.accounts.remove(at);
        for g in self.grants.values_mut() {
            if g.get(provider).map(String::as_str) == Some(account) {
                g.remove(provider);
            }
        }
        self.grants.retain(|_, g| !g.is_empty());
        Some(record)
    }
}

/// What the store holds for one account.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Secret {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds; absent for tokens that do not expire (GitHub OAuth apps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

// Hand-written so no `{:?}` anywhere can print a token.
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Secret {
    pub fn needs_refresh(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|at| now + EXPIRY_MARGIN_SECS >= at)
    }
}

#[derive(Debug)]
pub enum AccountsError {
    Io(PathBuf, std::io::Error),
    Parse(PathBuf, serde_json::Error),
    Store(String),
}

impl std::fmt::Display for AccountsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccountsError::Io(p, e) => write!(f, "{}: {}", p.display(), e),
            AccountsError::Parse(p, e) => write!(f, "{} is not valid: {}", p.display(), e),
            AccountsError::Store(e) => write!(f, "credential store: {}", e),
        }
    }
}

impl std::error::Error for AccountsError {}

/// `accounts.json`, beside the user's `sources.list`.
pub fn accounts_path(paths: &Paths) -> PathBuf {
    config_dir(paths).join("accounts.json")
}

/// The file store's `credentials.json`, beside the user's `installed/` tree.
pub fn file_store_path(paths: &Paths) -> PathBuf {
    paths
        .user_install_dir()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| paths.user_install_dir().to_path_buf())
        .join("credentials.json")
}

fn config_dir(paths: &Paths) -> PathBuf {
    paths
        .user_sources
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn load_accounts(paths: &Paths) -> Result<AccountsFile, AccountsError> {
    let path = accounts_path(paths);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| AccountsError::Parse(path, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AccountsFile::default()),
        Err(e) => Err(AccountsError::Io(path, e)),
    }
}

pub fn save_accounts(paths: &Paths, accounts: &AccountsFile) -> Result<(), AccountsError> {
    let path = accounts_path(paths);
    write_owner_only(&path, accounts)
}

fn write_owner_only<T: Serialize>(path: &std::path::Path, value: &T) -> Result<(), AccountsError> {
    if let Some(dir) = path.parent() {
        create_install_dir(dir, Readers::OwnerOnly)
            .map_err(|e| AccountsError::Io(dir.to_path_buf(), e))?;
    }
    let bytes = serde_json::to_vec_pretty(value).expect("plain data serializes");
    write_manifest_atomic(path, &bytes, Readers::OwnerOnly)
        .map_err(|e| AccountsError::Io(path.to_path_buf(), e))
}

/// Where secrets are kept.
pub trait SecretStore {
    fn get(&self, provider: &str, account: &str) -> Result<Option<Secret>, AccountsError>;
    fn set(&self, provider: &str, account: &str, secret: &Secret) -> Result<(), AccountsError>;
    fn delete(&self, provider: &str, account: &str) -> Result<(), AccountsError>;
}

fn store_key(provider: &str, account: &str) -> String {
    format!("{}/{}", provider, account)
}

/// The OS keyring.
pub struct KeyringStore {
    store: Arc<keyring_core::CredentialStore>,
}

impl KeyringStore {
    pub fn open() -> Result<Self, AccountsError> {
        Ok(KeyringStore {
            store: platform_keyring().map_err(|e| AccountsError::Store(e.to_string()))?,
        })
    }

    fn entry(&self, provider: &str, account: &str) -> Result<keyring_core::Entry, AccountsError> {
        self.store
            .build(KEYRING_SERVICE, &store_key(provider, account), None)
            .map_err(|e| AccountsError::Store(e.to_string()))
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_keyring() -> keyring_core::Result<Arc<keyring_core::CredentialStore>> {
    Ok(zbus_secret_service_keyring_store::Store::new()?)
}

#[cfg(target_os = "macos")]
fn platform_keyring() -> keyring_core::Result<Arc<keyring_core::CredentialStore>> {
    Ok(apple_native_keyring_store::keychain::Store::new()?)
}

#[cfg(windows)]
fn platform_keyring() -> keyring_core::Result<Arc<keyring_core::CredentialStore>> {
    Ok(windows_native_keyring_store::Store::new()?)
}

impl SecretStore for KeyringStore {
    fn get(&self, provider: &str, account: &str) -> Result<Option<Secret>, AccountsError> {
        match self.entry(provider, account)?.get_password() {
            Ok(raw) => serde_json::from_str(&raw)
                .map(Some)
                .map_err(|e| AccountsError::Store(format!("stored secret is not valid: {e}"))),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(AccountsError::Store(e.to_string())),
        }
    }

    fn set(&self, provider: &str, account: &str, secret: &Secret) -> Result<(), AccountsError> {
        let raw = serde_json::to_string(secret).expect("plain data serializes");
        self.entry(provider, account)?
            .set_password(&raw)
            .map_err(|e| AccountsError::Store(e.to_string()))
    }

    fn delete(&self, provider: &str, account: &str) -> Result<(), AccountsError> {
        match self.entry(provider, account)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(AccountsError::Store(e.to_string())),
        }
    }
}

/// An owner-only JSON file, for machines with no keyring.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(paths: &Paths) -> Self {
        FileStore {
            path: file_store_path(paths),
        }
    }

    fn load(&self) -> Result<BTreeMap<String, Secret>, AccountsError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| AccountsError::Parse(self.path.clone(), e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(AccountsError::Io(self.path.clone(), e)),
        }
    }
}

impl SecretStore for FileStore {
    fn get(&self, provider: &str, account: &str) -> Result<Option<Secret>, AccountsError> {
        Ok(self.load()?.remove(&store_key(provider, account)))
    }

    fn set(&self, provider: &str, account: &str, secret: &Secret) -> Result<(), AccountsError> {
        let mut all = self.load()?;
        all.insert(store_key(provider, account), secret.clone());
        write_owner_only(&self.path, &all)
    }

    fn delete(&self, provider: &str, account: &str) -> Result<(), AccountsError> {
        let mut all = self.load()?;
        if all.remove(&store_key(provider, account)).is_some() {
            write_owner_only(&self.path, &all)?;
        }
        Ok(())
    }
}

pub fn open_store(kind: StoreKind, paths: &Paths) -> Result<Box<dyn SecretStore>, AccountsError> {
    Ok(match kind {
        StoreKind::Keyring => Box::new(KeyringStore::open()?),
        StoreKind::File => Box::new(FileStore::new(paths)),
    })
}

/// The store a new sign-in goes to, and a warning when that is the file.
///
/// A forced keyring that cannot be opened is an error, not a quiet fallback:
/// whoever set it asked for the token not to land on disk.
/// A store for a new sign-in: which kind, the store, and any warning to show.
pub type NewLoginStore = (StoreKind, Box<dyn SecretStore>, Option<String>);

pub fn store_for_new_login(paths: &Paths) -> Result<NewLoginStore, AccountsError> {
    match std::env::var(STORE_ENV).ok().as_deref() {
        Some("file") => Ok((StoreKind::File, Box::new(FileStore::new(paths)), None)),
        Some("keyring") => Ok((StoreKind::Keyring, Box::new(KeyringStore::open()?), None)),
        _ => match KeyringStore::open() {
            Ok(store) => Ok((StoreKind::Keyring, Box::new(store), None)),
            Err(e) => {
                let warning = format!(
                    "no keyring available ({e}); the token is stored in {} (owner-only). \
                     Set {STORE_ENV}=keyring to refuse this instead.",
                    file_store_path(paths).display()
                );
                Ok((
                    StoreKind::File,
                    Box::new(FileStore::new(paths)),
                    Some(warning),
                ))
            }
        },
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// Why a server cannot be given the account it declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Nobody has signed in to this provider.
    NoAccount,
    /// Someone has, but this server was never granted the account.
    NotGranted,
    /// The granted account lacks a scope the server declares.
    InsufficientScope,
    /// The token expired and could not be refreshed.
    Expired,
    /// The store holding the account could not be read (keyring locked or
    /// absent in this session).
    StoreUnavailable,
}

/// A server declared an account it cannot be given yet. Carries everything a
/// caller needs to offer the fix, and nothing secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialRequired {
    pub server: String,
    pub provider: String,
    pub scopes: Vec<String>,
    pub reason: Reason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Prefix of the one-line, machine-readable form on stderr. A caller that
/// drives dmcp as a subprocess (JARVIS, dispatch) matches this rather than the
/// prose.
pub const CREDENTIAL_REQUIRED_PREFIX: &str = "credential_required: ";

impl CredentialRequired {
    pub fn machine_line(&self) -> String {
        format!(
            "{}{}",
            CREDENTIAL_REQUIRED_PREFIX,
            serde_json::to_string(self).expect("plain data serializes")
        )
    }

    pub fn fix(&self) -> String {
        match self.reason {
            Reason::NotGranted => match &self.account {
                Some(a) => format!(
                    "dmcp grant {} {} --account {}",
                    self.server, self.provider, a
                ),
                None => format!("dmcp grant {} {}", self.server, self.provider),
            },
            Reason::StoreUnavailable => {
                "unlock the keyring in this session, or sign in again".to_string()
            }
            _ => format!("dmcp login {} --for {}", self.provider, self.server),
        }
    }
}

impl std::fmt::Display for CredentialRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let why = match self.reason {
            Reason::NoAccount => format!("no {} account is signed in", self.provider),
            Reason::NotGranted => format!(
                "it has not been given a {} account{}",
                self.provider,
                self.account
                    .as_deref()
                    .map(|a| format!(" (signed in: {a})"))
                    .unwrap_or_default()
            ),
            Reason::InsufficientScope => format!(
                "the {} account lacks a scope it needs ({})",
                self.provider,
                self.scopes.join(", ")
            ),
            Reason::Expired => format!("the {} sign-in expired", self.provider),
            Reason::StoreUnavailable => {
                format!(
                    "the {} account's credential store could not be read",
                    self.provider
                )
            }
        };
        write!(
            f,
            "Sign-in needed: '{}' cannot start because {}",
            self.server, why
        )?;
        if let Some(detail) = &self.detail {
            write!(f, " ({})", detail)?;
        }
        write!(f, ". Fix: {}", self.fix())?;
        if let Some(tool) = &self.login_tool {
            write!(f, " — or use the server's own '{}' tool", tool)?;
        }
        Ok(())
    }
}

impl std::error::Error for CredentialRequired {}

/// Exchanges a refresh token. Swappable so resolution is testable without a
/// network.
pub trait Refresher {
    fn refresh(&self, record: &AccountRecord, refresh_token: &str) -> Result<Secret, String>;
}

/// The environment a server's declared credentials add at spawn.
///
/// A key already set in the server's `config` is left alone, and a
/// declaration whose keys are all set by hand needs no account at all — a
/// personal access token keeps working exactly as before.
pub fn credential_env(
    paths: &Paths,
    server: &str,
    manifest: &Manifest,
) -> Result<HashMap<String, OsString>, Box<CredentialRequired>> {
    if manifest.credentials.is_empty() {
        return Ok(HashMap::new());
    }
    resolve(
        paths,
        server,
        manifest,
        &|kind| open_store(kind, paths),
        &crate::login::HttpRefresher,
        now_unix(),
    )
}

type StoreOpener<'a> = dyn Fn(StoreKind) -> Result<Box<dyn SecretStore>, AccountsError> + 'a;

pub fn resolve(
    paths: &Paths,
    server: &str,
    manifest: &Manifest,
    open: &StoreOpener<'_>,
    refresher: &dyn Refresher,
    now: u64,
) -> Result<HashMap<String, OsString>, Box<CredentialRequired>> {
    let mut env = HashMap::new();
    let login_tool = manifest.login.as_ref().map(|l| l.tool.clone());

    for decl in &manifest.credentials {
        let wanted: Vec<(&String, &String)> = decl
            .inject
            .iter()
            .filter(|(key, _)| !manifest.config.contains_key(*key))
            .collect();
        if wanted.is_empty() {
            continue;
        }

        let required = |reason: Reason, account: Option<&str>, detail: Option<String>| {
            Box::new(CredentialRequired {
                server: server.to_string(),
                provider: decl.provider.clone(),
                scopes: decl.scopes.clone(),
                reason,
                account: account.map(str::to_string),
                login_tool: login_tool.clone(),
                detail,
            })
        };

        let accounts = load_accounts(paths)
            .map_err(|e| required(Reason::StoreUnavailable, None, Some(e.to_string())))?;
        let Some(account) = accounts.grant_for(server, &decl.provider) else {
            let signed_in = accounts.for_provider(&decl.provider);
            return Err(match signed_in.as_slice() {
                [] => required(Reason::NoAccount, None, None),
                [only] => required(Reason::NotGranted, Some(&only.account), None),
                _ => required(Reason::NotGranted, None, None),
            });
        };
        let Some(record) = accounts.find(&decl.provider, account) else {
            return Err(required(Reason::NoAccount, None, None));
        };

        let missing: Vec<&String> = decl
            .scopes
            .iter()
            .filter(|s| !record.scopes.contains(s))
            .collect();
        if !missing.is_empty() {
            let listed = missing
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(required(
                Reason::InsufficientScope,
                Some(account),
                Some(format!("missing: {listed}")),
            ));
        }

        let store = open(record.store)
            .map_err(|e| required(Reason::StoreUnavailable, Some(account), Some(e.to_string())))?;
        let mut secret = match store.get(&record.provider, &record.account) {
            Ok(Some(s)) => s,
            Ok(None) => {
                return Err(required(
                    Reason::NoAccount,
                    Some(account),
                    Some(format!("its token is no longer in the {}", record.store)),
                ))
            }
            Err(e) => {
                return Err(required(
                    Reason::StoreUnavailable,
                    Some(account),
                    Some(e.to_string()),
                ))
            }
        };

        if secret.needs_refresh(now) {
            let Some(refresh_token) = secret.refresh_token.clone() else {
                return Err(required(Reason::Expired, Some(account), None));
            };
            let mut fresh = refresher
                .refresh(record, &refresh_token)
                .map_err(|e| required(Reason::Expired, Some(account), Some(e)))?;
            // Providers that do not rotate refresh tokens omit it on refresh.
            if fresh.refresh_token.is_none() {
                fresh.refresh_token = Some(refresh_token);
            }
            store
                .set(&record.provider, &record.account, &fresh)
                .map_err(|e| {
                    required(Reason::StoreUnavailable, Some(account), Some(e.to_string()))
                })?;
            secret = fresh;
        }

        for (key, field) in wanted {
            let value = match field.as_str() {
                "access_token" => Some(secret.access_token.clone()),
                "refresh_token" => secret.refresh_token.clone(),
                "client_id" => record.client_id.clone(),
                "account" => Some(record.account.clone()),
                _ => None,
            };
            if let Some(value) = value {
                env.insert(key.clone(), OsString::from(value));
            }
        }
    }
    Ok(env)
}

/// Scopes the servers already granted `provider` accounts need, so a fresh
/// sign-in asks for them too instead of silently dropping another server's
/// access.
pub fn granted_scopes(paths: &Paths, provider: &str) -> BTreeSet<String> {
    let Ok(accounts) = load_accounts(paths) else {
        return BTreeSet::new();
    };
    let mut scopes = BTreeSet::new();
    for (server, grants) in &accounts.grants {
        if !grants.contains_key(provider) {
            continue;
        }
        if let Some((manifest, _)) = crate::discovery::get_server(paths, server) {
            for decl in manifest
                .credentials
                .iter()
                .filter(|d| d.provider == provider)
            {
                scopes.extend(decl.scopes.iter().cloned());
            }
        }
    }
    scopes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A store opener over one shared in-memory map.
    struct Shared(std::rc::Rc<RefCell<BTreeMap<String, Secret>>>);

    impl SecretStore for Shared {
        fn get(&self, p: &str, a: &str) -> Result<Option<Secret>, AccountsError> {
            Ok(self.0.borrow().get(&store_key(p, a)).cloned())
        }
        fn set(&self, p: &str, a: &str, s: &Secret) -> Result<(), AccountsError> {
            self.0.borrow_mut().insert(store_key(p, a), s.clone());
            Ok(())
        }
        fn delete(&self, p: &str, a: &str) -> Result<(), AccountsError> {
            self.0.borrow_mut().remove(&store_key(p, a));
            Ok(())
        }
    }

    struct NoRefresh;
    impl Refresher for NoRefresh {
        fn refresh(&self, _: &AccountRecord, _: &str) -> Result<Secret, String> {
            panic!("refresh must not be attempted here")
        }
    }

    struct Rotating(RefCell<u32>);
    impl Refresher for Rotating {
        fn refresh(&self, _: &AccountRecord, refresh_token: &str) -> Result<Secret, String> {
            *self.0.borrow_mut() += 1;
            assert_eq!(refresh_token, "rt-1");
            Ok(Secret {
                access_token: "at-2".into(),
                refresh_token: None,
                expires_at: Some(10_000),
            })
        }
    }

    struct Failing;
    impl Refresher for Failing {
        fn refresh(&self, _: &AccountRecord, _: &str) -> Result<Secret, String> {
            Err("invalid_grant".into())
        }
    }

    struct Tree {
        root: PathBuf,
        paths: Paths,
    }

    impl Tree {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "dmcp-accounts-{}-{}-{}",
                tag,
                std::process::id(),
                now_unix()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let paths = Paths {
                user_sources: root.join("config/sources.list"),
                user_install_dir: root.join("data/installed"),
                system_sources: root.join("etc/sources.list"),
                system_install_dir: root.join("usr/installed"),
                vector_index_dir: root.join("data/vector_index"),
            };
            Tree { root, paths }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn manifest(config: &[(&str, &str)]) -> Manifest {
        let mut m: Manifest = serde_json::from_value(serde_json::json!({
            "version": "1.0.0",
            "credentials": [{
                "provider": "github",
                "scopes": ["repo"],
                "inject": {"GH_TOKEN": "access_token", "GH_USER": "account"}
            }],
            "login": {"tool": "sign_in"}
        }))
        .unwrap();
        for (k, v) in config {
            m.config.insert(k.to_string(), serde_json::json!(v));
        }
        m
    }

    fn record(scopes: &[&str]) -> AccountRecord {
        AccountRecord {
            provider: "github".into(),
            account: "octocat".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            store: StoreKind::File,
            token_endpoint: "https://example.invalid/token".into(),
            client_id: Some("cid".into()),
            signed_in_at: 1,
        }
    }

    fn signed_in(tree: &Tree, scopes: &[&str], grant: bool) {
        let mut accounts = AccountsFile::default();
        accounts.upsert(record(scopes));
        if grant {
            accounts.grant("srv", "github", "octocat");
        }
        save_accounts(&tree.paths, &accounts).unwrap();
    }

    fn secret(expires_at: Option<u64>) -> Secret {
        Secret {
            access_token: "at-1".into(),
            refresh_token: Some("rt-1".into()),
            expires_at,
        }
    }

    fn run(
        tree: &Tree,
        m: &Manifest,
        map: &std::rc::Rc<RefCell<BTreeMap<String, Secret>>>,
        refresher: &dyn Refresher,
    ) -> Result<HashMap<String, OsString>, Box<CredentialRequired>> {
        let map = map.clone();
        resolve(
            &tree.paths,
            "srv",
            m,
            &move |_| Ok(Box::new(Shared(map.clone())) as Box<dyn SecretStore>),
            refresher,
            1_000,
        )
    }

    fn map_with(s: Option<Secret>) -> std::rc::Rc<RefCell<BTreeMap<String, Secret>>> {
        let mut m = BTreeMap::new();
        if let Some(s) = s {
            m.insert(store_key("github", "octocat"), s);
        }
        std::rc::Rc::new(RefCell::new(m))
    }

    #[test]
    fn a_granted_account_is_injected_into_exactly_the_mapped_keys() {
        let tree = Tree::new("granted");
        signed_in(&tree, &["repo", "gist"], true);
        let env = run(
            &tree,
            &manifest(&[]),
            &map_with(Some(secret(None))),
            &NoRefresh,
        )
        .unwrap();
        assert_eq!(env.get("GH_TOKEN"), Some(&OsString::from("at-1")));
        assert_eq!(env.get("GH_USER"), Some(&OsString::from("octocat")));
        assert_eq!(env.len(), 2);
    }

    #[test]
    fn a_hand_set_config_value_wins_and_needs_no_account() {
        let tree = Tree::new("handset");
        let m = manifest(&[("GH_TOKEN", "pat"), ("GH_USER", "me")]);
        let env = run(&tree, &m, &map_with(None), &NoRefresh).unwrap();
        assert!(
            env.is_empty(),
            "nothing injected over hand-set values: {env:?}"
        );
    }

    #[test]
    fn a_partly_hand_set_declaration_fills_only_the_rest() {
        let tree = Tree::new("partial");
        signed_in(&tree, &["repo"], true);
        let m = manifest(&[("GH_TOKEN", "pat")]);
        let env = run(&tree, &m, &map_with(Some(secret(None))), &NoRefresh).unwrap();
        assert_eq!(env.get("GH_TOKEN"), None);
        assert_eq!(env.get("GH_USER"), Some(&OsString::from("octocat")));
    }

    #[test]
    fn no_account_at_all_is_reported_as_such() {
        let tree = Tree::new("none");
        let err = run(&tree, &manifest(&[]), &map_with(None), &NoRefresh).unwrap_err();
        assert_eq!(err.reason, Reason::NoAccount);
        assert_eq!(err.login_tool.as_deref(), Some("sign_in"));
        assert!(err.fix().starts_with("dmcp login github --for srv"));
    }

    /// The property the grant exists for: an installed server that declares
    /// github gets nothing just because someone signed in to github.
    #[test]
    fn a_signed_in_account_is_not_handed_to_an_ungranted_server() {
        let tree = Tree::new("ungranted");
        signed_in(&tree, &["repo"], false);
        let err = run(
            &tree,
            &manifest(&[]),
            &map_with(Some(secret(None))),
            &NoRefresh,
        )
        .unwrap_err();
        assert_eq!(err.reason, Reason::NotGranted);
        assert_eq!(err.account.as_deref(), Some("octocat"));
        assert_eq!(err.fix(), "dmcp grant srv github --account octocat");
    }

    #[test]
    fn a_missing_scope_is_named() {
        let tree = Tree::new("scope");
        signed_in(&tree, &["read:user"], true);
        let err = run(
            &tree,
            &manifest(&[]),
            &map_with(Some(secret(None))),
            &NoRefresh,
        )
        .unwrap_err();
        assert_eq!(err.reason, Reason::InsufficientScope);
        assert_eq!(err.detail.as_deref(), Some("missing: repo"));
    }

    #[test]
    fn a_token_gone_from_the_store_asks_for_a_new_sign_in() {
        let tree = Tree::new("gone");
        signed_in(&tree, &["repo"], true);
        let err = run(&tree, &manifest(&[]), &map_with(None), &NoRefresh).unwrap_err();
        assert_eq!(err.reason, Reason::NoAccount);
    }

    #[test]
    fn an_expiring_token_is_refreshed_and_the_new_one_stored() {
        let tree = Tree::new("refresh");
        signed_in(&tree, &["repo"], true);
        let map = map_with(Some(secret(Some(1_030))));
        let refresher = Rotating(RefCell::new(0));
        let env = run(&tree, &manifest(&[]), &map, &refresher).unwrap();
        assert_eq!(*refresher.0.borrow(), 1);
        assert_eq!(env.get("GH_TOKEN"), Some(&OsString::from("at-2")));
        let stored = map.borrow().get("github/octocat").cloned().unwrap();
        assert_eq!(stored.access_token, "at-2");
        assert_eq!(
            stored.refresh_token.as_deref(),
            Some("rt-1"),
            "a provider that does not rotate keeps the old refresh token"
        );
    }

    #[test]
    fn a_refresh_that_fails_reports_expired_not_a_crash() {
        let tree = Tree::new("refail");
        signed_in(&tree, &["repo"], true);
        let err = run(
            &tree,
            &manifest(&[]),
            &map_with(Some(secret(Some(10)))),
            &Failing,
        )
        .unwrap_err();
        assert_eq!(err.reason, Reason::Expired);
        assert_eq!(err.detail.as_deref(), Some("invalid_grant"));
    }

    #[test]
    fn the_machine_line_carries_no_secret_and_parses() {
        let tree = Tree::new("line");
        signed_in(&tree, &["read:user"], true);
        let err = run(
            &tree,
            &manifest(&[]),
            &map_with(Some(secret(None))),
            &NoRefresh,
        )
        .unwrap_err();
        let line = err.machine_line();
        assert!(!line.contains("at-1") && !line.contains("rt-1"));
        let json: serde_json::Value =
            serde_json::from_str(line.strip_prefix(CREDENTIAL_REQUIRED_PREFIX).unwrap()).unwrap();
        assert_eq!(json["reason"], "insufficient_scope");
        assert_eq!(json["provider"], "github");
    }

    #[test]
    fn secret_debug_never_prints_tokens() {
        let shown = format!("{:?}", secret(Some(5)));
        assert!(
            !shown.contains("at-1") && !shown.contains("rt-1"),
            "{shown}"
        );
    }

    #[test]
    fn a_malformed_declaration_leaves_the_manifest_loadable() {
        let m: Manifest = serde_json::from_value(serde_json::json!({
            "version": "1.0.0",
            "credentials": "github",
            "login": 7
        }))
        .unwrap();
        assert!(m.credentials.is_empty());
        assert!(m.login.is_none());
    }

    #[test]
    fn removing_an_account_drops_its_grants_only() {
        let mut a = AccountsFile::default();
        a.upsert(record(&["repo"]));
        let mut other = record(&["repo"]);
        other.account = "work".into();
        a.upsert(other);
        a.grant("one", "github", "octocat");
        a.grant("two", "github", "work");
        a.grant("two", "google", "me");
        a.remove_account("github", "octocat");
        assert!(!a.grants.contains_key("one"));
        assert_eq!(a.grant_for("two", "github"), Some("work"));
        assert_eq!(a.grant_for("two", "google"), Some("me"));
    }

    #[cfg(unix)]
    #[test]
    fn the_file_store_and_index_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tree = Tree::new("modes");
        let store = FileStore::new(&tree.paths);
        store.set("github", "octocat", &secret(None)).unwrap();
        signed_in(&tree, &["repo"], true);
        for path in [file_store_path(&tree.paths), accounts_path(&tree.paths)] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", path.display());
        }
        assert_eq!(store.get("github", "octocat").unwrap(), Some(secret(None)));
        store.delete("github", "octocat").unwrap();
        assert_eq!(store.get("github", "octocat").unwrap(), None);
    }
}
