//! Pi's OAuth sign-ins, stored in `mcp-auth.json` next to Pi's `mcp.json`. Tokens are refreshed the
//! way Pi refreshes them, under the same lock files, so a refresh token that the server rotates is
//! never used twice.

mod client;
mod lock;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub use client::PiAuthClient;
use lock::DirLock;

use crate::config::{self, ConfigError};

/// Pi refreshes a token this long before it expires.
const REFRESH_SKEW: Duration = Duration::from_secs(30);
/// Pi takes over a refresh lock older than this, so a refresh must finish well within it.
const REFRESH_LOCK_STALE: Duration = Duration::from_secs(20);
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(25);
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(12);
/// The longest Pi's own lock on `mcp-auth.json` can go without renewal.
const FILE_LOCK_STALE: Duration = Duration::from_secs(30);
const FILE_LOCK_WAIT: Duration = Duration::from_secs(10);

/// The Pi sign-in of one http server.
#[derive(Debug)]
pub struct PiAuth {
    name: String,
    file: PathBuf,
    refresh_lock: PathBuf,
    /// The state's key in `mcp-auth.json`, by server name and URL.
    key: String,
    /// The key older Pi versions wrote, by URL alone.
    legacy_key: String,
    http: reqwest::Client,
}

#[derive(Debug)]
enum RefreshError {
    Lock(std::io::Error),
    Config(ConfigError),
    NoTokenEndpoint,
    Request(reqwest::Error),
    OAuth(String),
    Write(std::io::Error),
}

impl fmt::Display for RefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lock(error) => write!(f, "cannot lock: {error}"),
            Self::Config(error) => error.fmt(f),
            Self::NoTokenEndpoint => write!(f, "the sign-in has no token endpoint"),
            Self::Request(error) => write!(f, "token request failed: {error}"),
            Self::OAuth(error) => write!(f, "{error}; sign in again with `pi mcp login`"),
            Self::Write(error) => write!(f, "cannot save the new tokens: {error}"),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct State {
    discovery: Option<Discovery>,
    client_information: Option<ClientInformation>,
    tokens: Option<Tokens>,
    tokens_expire_at: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Discovery {
    authorization_server_url: Option<String>,
    authorization_server_metadata: Option<Metadata>,
    resource_metadata: Option<ResourceMetadata>,
}

#[derive(Deserialize)]
struct Metadata {
    token_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
}

#[derive(Deserialize)]
struct ResourceMetadata {
    resource: Option<String>,
}

#[derive(Deserialize)]
struct ClientInformation {
    client_id: String,
    client_secret: Option<String>,
    token_endpoint_auth_method: Option<String>,
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
    error_description: Option<String>,
}

impl PiAuth {
    /// The sign-in of server `name` at `url`, as configured in the Pi `mcp.json` at `pi_config`.
    pub fn new(pi_config: &Path, name: &str, url: &str) -> Self {
        let dir = pi_config.parent().unwrap_or(Path::new("."));
        let legacy_key = url::Url::parse(url).map_or_else(|_| url.to_owned(), String::from);
        let key = format!("mcp__{}|{legacy_key}", name.replace('-', "_"));
        let hash: String = Sha256::digest(key.as_bytes())[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        Self {
            name: name.to_owned(),
            file: dir.join("mcp-auth.json"),
            refresh_lock: dir.join(format!("mcp-auth-refresh-{hash}")),
            key,
            legacy_key,
            http: reqwest::Client::builder()
                .timeout(TOKEN_REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("a client without TLS options builds"),
        }
    }

    /// The stored access token, refreshed first when it is about to expire.
    pub async fn token(&self) -> Option<String> {
        let state = self.load_logged().await?;
        let token = state.tokens?;

        if !expired(state.tokens_expire_at) || token.refresh_token.is_none() {
            return Some(token.access_token);
        }

        self.refresh(Some(&token.access_token)).await;

        Some(self.load_logged().await?.tokens?.access_token)
    }

    /// Replaces `stale`, the token that expired or was rejected, returning whether a different token
    /// is now stored. A token another process stored meanwhile is used without refreshing.
    pub async fn refresh(&self, stale: Option<&str>) -> bool {
        match self.refresh_locked(stale).await {
            Ok(refreshed) => refreshed,
            Err(error) => {
                eprintln!("cannot refresh the Pi sign-in of {}: {error}", self.name);

                false
            }
        }
    }

    async fn refresh_locked(&self, stale: Option<&str>) -> Result<bool, RefreshError> {
        let _lock = DirLock::acquire(
            self.refresh_lock.clone(),
            REFRESH_LOCK_STALE,
            REFRESH_LOCK_WAIT,
        )
        .await
        .map_err(RefreshError::Lock)?;
        let (key, state) = self.load().await?;
        let Some(tokens) = state.tokens else {
            return Ok(false);
        };

        if stale != Some(tokens.access_token.as_str()) {
            return Ok(true);
        }

        let (Some(refresh_token), Some(client)) =
            (&tokens.refresh_token, &state.client_information)
        else {
            return Ok(false);
        };
        let discovery = state.discovery.as_ref();
        let metadata = discovery.and_then(|d| d.authorization_server_metadata.as_ref());
        let endpoint = metadata
            .and_then(|m| m.token_endpoint.clone())
            .or_else(|| {
                let base = url::Url::parse(discovery?.authorization_server_url.as_deref()?).ok()?;

                Some(base.join("/token").ok()?.into())
            })
            .ok_or(RefreshError::NoTokenEndpoint)?;

        let mut form = vec![
            ("grant_type", "refresh_token".to_owned()),
            ("refresh_token", refresh_token.clone()),
        ];

        if let Some(resource) = discovery
            .and_then(|d| d.resource_metadata.as_ref())
            .and_then(|r| r.resource.clone())
        {
            form.push(("resource", resource));
        }

        let supported = metadata.map_or(&[][..], |m| &m.token_endpoint_auth_methods_supported);
        let mut request = self
            .http
            .post(endpoint)
            .header(reqwest::header::ACCEPT, "application/json");

        match client_auth_method(client, supported) {
            "client_secret_basic" => {
                request = request.basic_auth(&client.client_id, client.client_secret.as_ref());
            }
            method => {
                form.push(("client_id", client.client_id.clone()));

                if let ("client_secret_post", Some(secret)) = (method, &client.client_secret) {
                    form.push(("client_secret", secret.clone()));
                }
            }
        }

        let response = request
            .form(&form)
            .send()
            .await
            .map_err(RefreshError::Request)?;
        let status = response.status();
        let body = response.text().await.map_err(RefreshError::Request)?;

        // Servers may report OAuth errors with any status, so the body is checked first, as Pi does.
        if let Ok(error) = serde_json::from_str::<ErrorResponse>(&body) {
            return Err(RefreshError::OAuth(
                error.error_description.unwrap_or(error.error),
            ));
        }

        let mut granted = match serde_json::from_str::<Map<String, Value>>(&body) {
            Ok(granted)
                if status.is_success()
                    && granted.get("access_token").is_some_and(Value::is_string) =>
            {
                granted
            }
            _ => return Err(RefreshError::OAuth(format!("HTTP {status}: {body}"))),
        };

        // A response without a refresh token keeps the old one, and one without a scope keeps the
        // grant's scope (RFC 6749 section 6).
        granted
            .entry("refresh_token")
            .or_insert_with(|| refresh_token.clone().into());

        if let Some(scope) = tokens.scope {
            granted.entry("scope").or_insert(scope.into());
        }

        // Pi stores the expiry as whole milliseconds.
        let expire_at = granted
            .get("expires_in")
            .and_then(Value::as_f64)
            .map(|seconds| now_millis() + seconds * 1000.0);
        self.save(&key, granted, expire_at).await?;

        Ok(true)
    }

    async fn load_logged(&self) -> Option<State> {
        match self.load().await {
            Ok((_, state)) => Some(state),
            Err(error) => {
                eprintln!("cannot read the Pi sign-in of {}: {error}", self.name);

                None
            }
        }
    }

    /// The server's state and the key it is stored under.
    async fn load(&self) -> Result<(String, State), RefreshError> {
        let states = self.read_states().await?;

        for key in [&self.key, &self.legacy_key] {
            if let Some(state) = states.get(key) {
                let state = State::deserialize(state).map_err(|source| {
                    RefreshError::Config(ConfigError::Parse {
                        path: self.file.clone(),
                        source,
                    })
                })?;

                return Ok((key.clone(), state));
            }
        }

        Ok((self.key.clone(), State::default()))
    }

    async fn read_states(&self) -> Result<Map<String, Value>, RefreshError> {
        let _lock = self.lock_file().await?;

        read_states(&self.file)
    }

    /// Stores new tokens unless the user signed out meanwhile.
    async fn save(
        &self,
        key: &str,
        tokens: Map<String, Value>,
        expire_at: Option<f64>,
    ) -> Result<(), RefreshError> {
        let _lock = self.lock_file().await?;
        let mut states = read_states(&self.file)?;
        let Some(Value::Object(state)) = states.get_mut(key) else {
            return Ok(());
        };

        state.insert("tokens".to_owned(), tokens.into());

        match expire_at {
            Some(expire_at) => state.insert("tokensExpireAt".to_owned(), (expire_at as u64).into()),
            None => state.remove("tokensExpireAt"),
        };

        let text = serde_json::to_string_pretty(&states).expect("JSON values serialize") + "\n";

        std::fs::write(&self.file, text).map_err(RefreshError::Write)
    }

    async fn lock_file(&self) -> Result<DirLock, RefreshError> {
        DirLock::acquire(self.file.clone(), FILE_LOCK_STALE, FILE_LOCK_WAIT)
            .await
            .map_err(RefreshError::Lock)
    }
}

fn read_states(file: &Path) -> Result<Map<String, Value>, RefreshError> {
    Ok(config::read_json_if_exists(file)
        .map_err(RefreshError::Config)?
        .unwrap_or_default())
}

/// How the client authenticates to the token endpoint, chosen as Pi chooses it.
fn client_auth_method(client: &ClientInformation, supported: &[String]) -> &'static str {
    let allowed = |method: &str| supported.is_empty() || supported.iter().any(|s| s == method);
    let secret = client.client_secret.is_some();

    if let Some(hinted) = ["client_secret_basic", "client_secret_post", "none"]
        .into_iter()
        .find(|method| client.token_endpoint_auth_method.as_deref() == Some(*method))
        && allowed(hinted)
    {
        return hinted;
    }

    let offers = |method: &str| supported.iter().any(|s| s == method);

    match () {
        _ if supported.is_empty() => {
            if secret {
                "client_secret_basic"
            } else {
                "none"
            }
        }
        _ if secret && offers("client_secret_basic") => "client_secret_basic",
        _ if secret && offers("client_secret_post") => "client_secret_post",
        _ if offers("none") => "none",
        _ if secret => "client_secret_post",
        _ => "none",
    }
}

fn expired(expire_at: Option<f64>) -> bool {
    expire_at.is_some_and(|at| at - REFRESH_SKEW.as_millis() as f64 <= now_millis())
}

fn now_millis() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64
}
