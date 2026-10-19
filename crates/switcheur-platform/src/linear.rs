//! Linear GraphQL client: connect a workspace (OAuth with PKCE, or a
//! personal API key), look an issue up by identifier, create an issue.
//!
//! One credential per workspace. Tokens and keys live in an owner-only file
//! next to `config.toml`, never in it. Neither credentials nor GraphQL error
//! messages (they may echo input) are logged; callers only see
//! [`LinearError::code`].

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const API_URL: &str = "https://api.linear.app/graphql";
const AUTHORIZE_URL: &str = "https://linear.app/oauth/authorize";
const TOKEN_URL: &str = "https://api.linear.app/oauth/token";
const REVOKE_URL: &str = "https://api.linear.app/oauth/revoke";
/// Public OAuth application "LeSwitcheur" (PKCE, no client secret).
const OAUTH_CLIENT_ID: &str = "a80da8da691ad13e8c9b769829a6712d";
/// Loopback redirect registered on the OAuth application.
const OAUTH_REDIRECT_PORT: u16 = 47823;
const OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:47823/callback";
const OAUTH_SCOPES: &str = "read,issues:create";
/// Refresh access tokens this long before they expire.
const TOKEN_REFRESH_MARGIN: u64 = 300;
const STORE_FILE: &str = "linear-accounts.json";
/// Page where the user creates a personal API key.
pub const API_KEY_SETTINGS_URL: &str = "https://linear.app/settings/account/security";
/// Teams are re-fetched in the background past this age.
pub const TEAMS_MAX_AGE: Duration = Duration::from_secs(6 * 3600);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub key: String,
    pub name: String,
}

/// A connected workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    /// Linear organization id.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub url_key: String,
    /// Who the credential belongs to (email, else name).
    #[serde(default)]
    pub user: String,
    /// OAuth access token when `oauth` is set, else a personal API key.
    pub api_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthTokens>,
    #[serde(default)]
    pub teams: Vec<Team>,
    #[serde(default)]
    pub teams_fetched_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthTokens {
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: u64,
}

impl Account {
    pub fn teams_stale(&self) -> bool {
        now_secs().saturating_sub(self.teams_fetched_at) > TEAMS_MAX_AGE.as_secs()
    }
}

/// A team the "Create a Linear issue" row can target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamChoice {
    pub account: Account,
    pub team: Team,
    /// Workspace name, for labels when several workspaces are connected.
    pub workspace: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Store {
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// Team ids, most recently used first.
    #[serde(default)]
    pub recent_teams: Vec<String>,
}

impl Store {
    /// Missing or unreadable file = no account.
    pub fn load() -> Self {
        let Some(path) = store_path() else {
            return Self::default();
        };
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!("linear store unreadable: {e}");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = store_path().context("no config directory")?;
        crate::openclaw::write_private(&path, &serde_json::to_vec_pretty(self)?)
    }

    /// Insert or replace (same workspace connected again = new key).
    pub fn upsert(&mut self, account: Account) {
        match self.accounts.iter_mut().find(|a| a.id == account.id) {
            Some(existing) => *existing = account,
            None => self.accounts.push(account),
        }
    }

    pub fn remove(&mut self, account_id: &str) {
        self.accounts.retain(|a| a.id != account_id);
    }

    /// Every team, most recently used first, then by workspace and key.
    pub fn team_choices(&self) -> Vec<TeamChoice> {
        let mut choices: Vec<TeamChoice> = self
            .accounts
            .iter()
            .flat_map(|account| {
                account.teams.iter().map(|team| TeamChoice {
                    account: account.clone(),
                    team: team.clone(),
                    workspace: account.name.clone(),
                })
            })
            .collect();
        choices.sort_by(|a, b| {
            let rank = |c: &TeamChoice| {
                self.recent_teams
                    .iter()
                    .position(|id| *id == c.team.id)
                    .unwrap_or(usize::MAX)
            };
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.workspace.cmp(&b.workspace))
                .then_with(|| a.team.key.cmp(&b.team.key))
        });
        choices
    }

    pub fn promote_team(&mut self, team_id: &str) {
        self.recent_teams.retain(|id| id != team_id);
        self.recent_teams.insert(0, team_id.to_string());
        self.recent_teams.truncate(32);
    }

    /// Accounts owning a team with this key (case-insensitive).
    pub fn accounts_with_team_key(&self, key: &str) -> Vec<&Account> {
        self.accounts
            .iter()
            .filter(|a| a.teams.iter().any(|t| t.key.eq_ignore_ascii_case(key)))
            .collect()
    }
}

fn store_path() -> Option<PathBuf> {
    switcheur_core::config::config_dir().map(|d| d.join(STORE_FILE))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinearError {
    #[error("invalid or revoked API key")]
    Unauthorized,
    #[error("rate limited")]
    RateLimited,
    #[error("network error")]
    Network,
    #[error("Linear refused the request ({0})")]
    Api(String),
    #[error("authorization cancelled")]
    Cancelled,
    #[error("authorization denied in the browser")]
    Denied,
    #[error("authorization timed out")]
    TimedOut,
    #[error("callback port in use")]
    PortInUse,
}

impl LinearError {
    /// Content-free code, safe to log.
    pub fn code(&self) -> String {
        match self {
            LinearError::Unauthorized => "AUTHENTICATION_ERROR".into(),
            LinearError::RateLimited => "RATELIMITED".into(),
            LinearError::Network => "NETWORK".into(),
            LinearError::Api(code) => code.clone(),
            LinearError::Cancelled => "CANCELLED".into(),
            LinearError::Denied => "ACCESS_DENIED".into(),
            LinearError::TimedOut => "TIMED_OUT".into(),
            LinearError::PortInUse => "PORT_IN_USE".into(),
        }
    }
}

/// Validate a personal API key and fetch its workspace + teams. Blocking.
pub fn connect(api_key: &str) -> Result<Account, LinearError> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(LinearError::Unauthorized);
    }
    fetch_account(api_key, None)
}

fn fetch_account(secret: &str, oauth: Option<OAuthTokens>) -> Result<Account, LinearError> {
    let authorization = match oauth {
        Some(_) => format!("Bearer {secret}"),
        None => secret.to_string(),
    };
    let data = graphql(
        &authorization,
        "query { viewer { name email } organization { id name urlKey } \
         teams(first: 250) { nodes { id key name } } }",
        json!({}),
    )?;
    let mut account = parse_account(&data, secret)?;
    account.oauth = oauth;
    Ok(account)
}

/// `Authorization` header for `account`, refreshing an expiring OAuth token
/// first. `refreshed` must be written back to the store: refresh tokens
/// rotate. Blocking.
pub struct Authorized {
    pub header: String,
    pub refreshed: Option<Account>,
}

pub fn authorize(account: &Account) -> Result<Authorized, LinearError> {
    let Some(oauth) = &account.oauth else {
        return Ok(Authorized {
            header: account.api_key.clone(),
            refreshed: None,
        });
    };
    if oauth.expires_at > now_secs() + TOKEN_REFRESH_MARGIN {
        return Ok(Authorized {
            header: format!("Bearer {}", account.api_key),
            refreshed: None,
        });
    }
    let tokens = token_request(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &oauth.refresh_token),
        ("client_id", OAUTH_CLIENT_ID),
    ])?;
    let mut refreshed = account.clone();
    refreshed.api_key = tokens.access_token;
    refreshed.oauth = Some(tokens.oauth);
    Ok(Authorized {
        header: format!("Bearer {}", refreshed.api_key),
        refreshed: Some(refreshed),
    })
}

/// Re-fetch the teams of a connected workspace whose token is current
/// (run [`authorize`] first). Blocking.
pub fn refresh(account: &Account) -> Result<Account, LinearError> {
    let fresh = fetch_account(&account.api_key, account.oauth.clone())?;
    if fresh.id != account.id {
        return Err(LinearError::Api("WORKSPACE_CHANGED".into()));
    }
    Ok(fresh)
}

/// Best effort: tell Linear an OAuth grant is no longer used. Blocking.
pub fn revoke(account: &Account) {
    if account.oauth.is_none() {
        return;
    }
    let result = ureq::post(REVOKE_URL)
        .timeout(Duration::from_secs(10))
        .set("Authorization", &format!("Bearer {}", account.api_key))
        .call();
    if let Err(error) = result {
        let status = match error {
            ureq::Error::Status(code, _) => code,
            _ => 0,
        };
        tracing::warn!(status, "linear token revoke failed");
    }
}

/// An authorization in progress: the loopback listener is bound before the
/// browser opens so the redirect can't race it.
pub struct OAuthFlow {
    pub authorize_url: String,
    listener: TcpListener,
    verifier: String,
    state: String,
}

/// Bind the callback port and build the consent URL. `prompt=consent`
/// lets the user pick another workspace when connecting a second one.
pub fn begin_oauth() -> Result<OAuthFlow, LinearError> {
    let listener = TcpListener::bind(("127.0.0.1", OAUTH_REDIRECT_PORT))
        .map_err(|_| LinearError::PortInUse)?;
    listener
        .set_nonblocking(true)
        .map_err(|_| LinearError::PortInUse)?;
    let verifier = random_token(32);
    let state = random_token(16);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = ::url::Url::parse(AUTHORIZE_URL).expect("static URL");
    url.query_pairs_mut()
        .append_pair("client_id", OAUTH_CLIENT_ID)
        .append_pair("redirect_uri", OAUTH_REDIRECT_URI)
        .append_pair("response_type", "code")
        .append_pair("scope", OAUTH_SCOPES)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("prompt", "consent")
        .append_pair("actor", "user");
    Ok(OAuthFlow {
        authorize_url: url.into(),
        listener,
        verifier,
        state,
    })
}

/// Wait for the browser redirect, exchange the code, fetch the workspace.
/// `done_page` is the HTML shown in the browser tab afterwards. Blocking.
pub fn finish_oauth(
    flow: OAuthFlow,
    cancel: &AtomicBool,
    timeout: Duration,
    done_page: &str,
) -> Result<Account, LinearError> {
    let deadline = Instant::now() + timeout;
    let code = loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(LinearError::Cancelled);
        }
        if Instant::now() > deadline {
            return Err(LinearError::TimedOut);
        }
        match flow.listener.accept() {
            Ok((stream, _)) => {
                if let Some(result) = handle_callback(stream, &flow.state, done_page) {
                    break result?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return Err(LinearError::Network),
        }
    };
    let tokens = token_request(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", OAUTH_REDIRECT_URI),
        ("client_id", OAUTH_CLIENT_ID),
        ("code_verifier", &flow.verifier),
    ])?;
    fetch_account(&tokens.access_token, Some(tokens.oauth))
}

/// `None` for unrelated requests (favicon, wrong state), so the flow keeps
/// waiting.
fn handle_callback(
    mut stream: TcpStream,
    state: &str,
    done_page: &str,
) -> Option<Result<String, LinearError>> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    let outcome = parse_callback(&line, state);
    let (status, body) = match &outcome {
        Some(_) => ("200 OK", done_page),
        None => ("404 Not Found", ""),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    outcome
}

fn parse_callback(request_line: &str, state: &str) -> Option<Result<String, LinearError>> {
    let target = request_line.strip_prefix("GET ")?.split(' ').next()?;
    let url = ::url::Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
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
    if param("error").is_some() {
        return Some(Err(LinearError::Denied));
    }
    param("code").map(Ok)
}

struct TokenResponse {
    access_token: String,
    oauth: OAuthTokens,
}

fn token_request(form: &[(&str, &str)]) -> Result<TokenResponse, LinearError> {
    let response = ureq::post(TOKEN_URL)
        .timeout(Duration::from_secs(15))
        .send_form(form);
    let body: Value = match response {
        Ok(resp) => resp.into_json().map_err(|_| LinearError::Network)?,
        Err(ureq::Error::Status(400 | 401, _)) => return Err(LinearError::Unauthorized),
        Err(ureq::Error::Status(status, resp)) => {
            let body: Value = resp.into_json().unwrap_or(Value::Null);
            return Err(error_from(status, &body));
        }
        Err(_) => return Err(LinearError::Network),
    };
    parse_tokens(&body)
}

fn parse_tokens(body: &Value) -> Result<TokenResponse, LinearError> {
    let access_token = body["access_token"].as_str();
    let refresh_token = body["refresh_token"].as_str();
    match (access_token, refresh_token) {
        (Some(access), Some(refresh)) => Ok(TokenResponse {
            access_token: access.to_string(),
            oauth: OAuthTokens {
                refresh_token: refresh.to_string(),
                expires_at: now_secs() + body["expires_in"].as_u64().unwrap_or(86_399),
            },
        }),
        _ => Err(LinearError::Api("BAD_TOKEN_RESPONSE".into())),
    }
}

fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).expect("system RNG");
    URL_SAFE_NO_PAD.encode(buf)
}

fn parse_account(data: &Value, api_key: &str) -> Result<Account, LinearError> {
    let org = &data["organization"];
    let str_at = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let id = str_at(&org["id"]);
    if id.is_empty() {
        return Err(LinearError::Api("BAD_RESPONSE".into()));
    }
    let viewer = &data["viewer"];
    let user = Some(str_at(&viewer["email"]))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| str_at(&viewer["name"]));
    let mut teams: Vec<Team> = data["teams"]["nodes"]
        .as_array()
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|n| serde_json::from_value(n.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    teams.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(Account {
        id,
        name: str_at(&org["name"]),
        url_key: str_at(&org["urlKey"]),
        user,
        api_key: api_key.to_string(),
        oauth: None,
        teams,
        teams_fetched_at: now_secs(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundIssue {
    pub identifier: String,
    pub title: String,
    pub url: String,
    pub state: Option<String>,
}

/// Issue `<team_key>-<number>`, or `None` when it doesn't exist. Blocking.
pub fn find_issue(authorization: &str, team_key: &str, number: u64) -> Result<Option<FoundIssue>, LinearError> {
    let result = graphql(
        authorization,
        "query($id: String!) { issue(id: $id) { identifier title url state { name } } }",
        json!({ "id": format!("{team_key}-{number}") }),
    );
    match result {
        Ok(data) => Ok(parse_found_issue(&data)),
        // A missing issue comes back as a GraphQL error ("entity not found").
        Err(LinearError::Api(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn parse_found_issue(data: &Value) -> Option<FoundIssue> {
    let node = &data["issue"];
    Some(FoundIssue {
        identifier: node["identifier"].as_str()?.to_string(),
        title: node["title"].as_str().unwrap_or_default().to_string(),
        url: node["url"].as_str()?.to_string(),
        state: node["state"]["name"].as_str().map(str::to_string),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedIssue {
    pub identifier: String,
    pub url: String,
}

/// Create an issue titled `title` in `team_id`, assigned to nobody, in the
/// team's default state. Blocking.
pub fn create_issue(authorization: &str, team_id: &str, title: &str) -> Result<CreatedIssue, LinearError> {
    let data = graphql(
        authorization,
        "mutation($teamId: String!, $title: String!) { issueCreate(input: \
         { teamId: $teamId, title: $title }) { success issue { identifier url } } }",
        json!({ "teamId": team_id, "title": title }),
    )?;
    parse_created(&data)
}

fn parse_created(data: &Value) -> Result<CreatedIssue, LinearError> {
    let created = &data["issueCreate"];
    let issue = &created["issue"];
    match (created["success"].as_bool(), issue["identifier"].as_str(), issue["url"].as_str()) {
        (Some(true), Some(identifier), Some(url)) => Ok(CreatedIssue {
            identifier: identifier.to_string(),
            url: url.to_string(),
        }),
        _ => Err(LinearError::Api("NOT_CREATED".into())),
    }
}

/// POST one GraphQL operation, return its `data`.
fn graphql(authorization: &str, query: &str, variables: Value) -> Result<Value, LinearError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(15))
        .build();
    let response = agent
        .post(API_URL)
        .set("Authorization", authorization)
        .set("Content-Type", "application/json")
        .send_json(json!({ "query": query, "variables": variables }));
    let body: Value = match response {
        Ok(resp) => resp.into_json().map_err(|_| LinearError::Network)?,
        Err(ureq::Error::Status(status, resp)) => {
            let body: Value = resp.into_json().unwrap_or(Value::Null);
            return Err(error_from(status, &body));
        }
        Err(_) => return Err(LinearError::Network),
    };
    if body["errors"].as_array().is_some_and(|e| !e.is_empty()) {
        return Err(error_from(200, &body));
    }
    Ok(body["data"].clone())
}

fn error_from(status: u16, body: &Value) -> LinearError {
    let extensions = &body["errors"][0]["extensions"];
    let code = extensions["code"]
        .as_str()
        .or_else(|| extensions["type"].as_str())
        .unwrap_or_default()
        .to_ascii_uppercase()
        .replace(' ', "_");
    match (status, code.as_str()) {
        (401 | 403, _) | (_, "AUTHENTICATION_ERROR" | "FORBIDDEN") => LinearError::Unauthorized,
        (429, _) | (_, "RATELIMITED") => LinearError::RateLimited,
        (_, "") => LinearError::Api(format!("HTTP_{status}")),
        // Keep the code machine-shaped so it can't carry user input.
        _ => LinearError::Api(
            code.chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .take(40)
                .collect(),
        ),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team(id: &str, key: &str) -> Team {
        Team {
            id: id.into(),
            key: key.into(),
            name: key.into(),
        }
    }

    fn account(id: &str, name: &str, teams: Vec<Team>) -> Account {
        Account {
            id: id.into(),
            name: name.into(),
            url_key: name.to_lowercase(),
            user: "me@example.com".into(),
            api_key: format!("key-{id}"),
            oauth: None,
            teams,
            teams_fetched_at: 0,
        }
    }

    #[test]
    fn account_parses_viewer_org_and_teams() {
        let data = json!({
            "viewer": { "name": "Me", "email": "me@example.com" },
            "organization": { "id": "org1", "name": "Acme", "urlKey": "acme" },
            "teams": { "nodes": [
                { "id": "t2", "key": "OPS", "name": "Ops" },
                { "id": "t1", "key": "ENG", "name": "Engineering" }
            ] }
        });
        let a = parse_account(&data, "lin_api_x").unwrap();
        assert_eq!(a.id, "org1");
        assert_eq!(a.user, "me@example.com");
        assert_eq!(a.teams.iter().map(|t| t.key.as_str()).collect::<Vec<_>>(), ["ENG", "OPS"]);
        assert!(!a.teams_stale());
    }

    #[test]
    fn missing_organization_is_an_error() {
        assert!(parse_account(&json!({}), "k").is_err());
    }

    #[test]
    fn found_and_missing_issues() {
        let data = json!({ "issue": {
            "identifier": "ENG-12", "title": "Fix", "url": "https://linear.app/acme/issue/ENG-12/fix",
            "state": { "name": "Todo" }
        } });
        let issue = parse_found_issue(&data).unwrap();
        assert_eq!(issue.identifier, "ENG-12");
        assert_eq!(issue.state.as_deref(), Some("Todo"));
        assert_eq!(parse_found_issue(&json!({ "issue": null })), None);
    }

    #[test]
    fn created_issue_requires_success() {
        let ok = json!({ "issueCreate": { "success": true, "issue": { "identifier": "ENG-13", "url": "u" } } });
        assert_eq!(parse_created(&ok).unwrap().identifier, "ENG-13");
        let refused = json!({ "issueCreate": { "success": false, "issue": null } });
        assert!(parse_created(&refused).is_err());
    }

    #[test]
    fn errors_map_to_content_free_codes() {
        let auth = json!({ "errors": [{ "message": "secret echo", "extensions": { "type": "authentication error" } }] });
        assert_eq!(error_from(400, &auth), LinearError::Unauthorized);
        assert_eq!(error_from(429, &Value::Null), LinearError::RateLimited);
        assert_eq!(error_from(500, &Value::Null).code(), "HTTP_500");
        let odd = json!({ "errors": [{ "extensions": { "code": "invalid input: <x>" } }] });
        assert_eq!(error_from(400, &odd).code(), "INVALID_INPUT_X");
    }

    #[test]
    fn callback_requires_matching_state() {
        let ok = parse_callback("GET /callback?code=abc&state=s1 HTTP/1.1\r\n", "s1");
        assert_eq!(ok, Some(Ok("abc".into())));
        assert_eq!(parse_callback("GET /callback?code=abc&state=bad HTTP/1.1", "s1"), None);
        assert_eq!(parse_callback("GET /favicon.ico HTTP/1.1", "s1"), None);
        let denied = parse_callback("GET /callback?error=access_denied&state=s1 HTTP/1.1", "s1");
        assert_eq!(denied, Some(Err(LinearError::Denied)));
    }

    #[test]
    fn token_response_needs_both_tokens() {
        let t = parse_tokens(&json!({ "access_token": "a", "refresh_token": "r", "expires_in": 100 })).unwrap();
        assert_eq!(t.access_token, "a");
        assert!(t.oauth.expires_at > now_secs());
        assert!(parse_tokens(&json!({ "access_token": "a" })).is_err());
    }

    #[test]
    fn api_key_accounts_authorize_without_refresh() {
        let a = account("o1", "Acme", Vec::new());
        let auth = authorize(&a).unwrap();
        assert_eq!(auth.header, "key-o1");
        assert!(auth.refreshed.is_none());
        let mut o = a.clone();
        o.oauth = Some(OAuthTokens { refresh_token: "r".into(), expires_at: now_secs() + 3600 });
        assert_eq!(authorize(&o).unwrap().header, "Bearer key-o1");
    }

    #[test]
    fn pkce_flow_url_carries_challenge() {
        // Port may be taken on a dev machine running the app; skip then.
        let Ok(flow) = begin_oauth() else { return };
        let url = ::url::Url::parse(&flow.authorize_url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(
            q["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(flow.verifier.as_bytes()))
        );
        assert_eq!(q["state"], flow.state);
        assert_eq!(q["redirect_uri"], OAUTH_REDIRECT_URI);
    }

    #[test]
    fn team_choices_follow_recent_use() {
        let mut store = Store::default();
        store.upsert(account("o1", "Acme", vec![team("a", "ENG"), team("b", "OPS")]));
        store.upsert(account("o2", "Perso", vec![team("c", "PERSO")]));
        let keys = |s: &Store| s.team_choices().iter().map(|c| c.team.key.clone()).collect::<Vec<_>>();
        assert_eq!(keys(&store), ["ENG", "OPS", "PERSO"]);
        store.promote_team("c");
        store.promote_team("b");
        assert_eq!(keys(&store), ["OPS", "PERSO", "ENG"]);
        store.remove("o2");
        assert_eq!(keys(&store), ["OPS", "ENG"]);
    }

    #[test]
    fn reconnecting_a_workspace_replaces_it() {
        let mut store = Store::default();
        store.upsert(account("o1", "Acme", vec![team("a", "ENG")]));
        let mut again = account("o1", "Acme", vec![team("a", "ENG"), team("b", "OPS")]);
        again.api_key = "new".into();
        store.upsert(again);
        assert_eq!(store.accounts.len(), 1);
        assert_eq!(store.accounts[0].api_key, "new");
        assert_eq!(store.accounts_with_team_key("ops").len(), 1);
    }
}
