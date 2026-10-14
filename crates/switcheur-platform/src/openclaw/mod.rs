//! OpenClaw gateway: send the switcher query as an instruction to a session.
//!
//! The shared gateway token typed in the setup window is only used to pair
//! an Ed25519 device identity; it is never written to disk. The device token
//! the gateway mints at pairing is stored in an owner-only file next to
//! `config.toml`. Instructions are never logged, and gateway error messages
//! (which may echo them) are never decoded.

pub mod gateway;
pub mod url;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use ::url::Url;
pub use gateway::GatewayError;
use gateway::{Connection, Credential, Identity};

const DEVICE_FILE: &str = "openclaw-device.json";
/// `chat.send` acknowledges once the turn is queued; leave room for a busy
/// gateway admitting it.
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
/// Pairing retry cadence while an approval is pending.
pub const PAIRING_RETRY: Duration = Duration::from_secs(3);
/// Give up waiting for `openclaw devices approve` after this long.
pub const PAIRING_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Parse the Control UI URL typed by the user.
pub fn parse_gateway_url(raw: &str) -> Result<::url::Url, GatewayError> {
    let url = ::url::Url::parse(raw.trim()).map_err(|_| GatewayError::Protocol("invalid-url"))?;
    gateway::websocket_url(&url)?;
    if url.host_str().is_none() {
        return Err(GatewayError::Protocol("invalid-url"));
    }
    Ok(url)
}

/// Control UI page of `session_key`, opened after an instruction is sent.
pub fn conversation_url(base: &::url::Url, session_key: &str) -> Option<String> {
    url::session_url(base, session_key, None)
}

/// Outcome of one pairing attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairOutcome {
    Paired,
    /// Waiting for `openclaw devices approve <request_id>` on the gateway.
    /// `paused`: the gateway asks clients not to retry on a timer.
    AwaitingApproval {
        request_id: Option<String>,
        paused: bool,
    },
}

/// One pairing attempt with the shared gateway token. Stores the minted
/// device token on success.
pub fn pair(base: &::url::Url, token: &str) -> Result<PairOutcome, GatewayError> {
    let dir = data_dir().ok_or(GatewayError::Protocol("save-failed"))?;
    let mut store = DeviceStore::open(&dir).map_err(|_| GatewayError::Protocol("save-failed"))?;
    match Connection::connect(base, &store.identity, Credential::SharedToken(token)) {
        Ok((_connection, hello)) => {
            check_write_scope(&hello.scopes)?;
            // The shared secret is never stored: without a device token the
            // next send would have no credential.
            let device_token = hello
                .device_token
                .ok_or(GatewayError::Protocol("no-device-token"))?;
            store
                .set_device_token(origin_of(base), Some(device_token))
                .map_err(|_| GatewayError::Protocol("save-failed"))?;
            Ok(PairOutcome::Paired)
        }
        Err(GatewayError::PairingRequired { request_id, pause }) => {
            Ok(PairOutcome::AwaitingApproval {
                request_id,
                paused: pause,
            })
        }
        Err(error) => Err(error),
    }
}

/// Whether a device token is stored for this gateway.
pub fn is_paired(base: &::url::Url) -> bool {
    data_dir()
        .and_then(|dir| DeviceStore::open(&dir).ok())
        .is_some_and(|store| store.device_token(&origin_of(base)).is_some())
}

/// Drop the stored device token for this gateway (the key pair is kept).
pub fn forget(base: &::url::Url) -> Result<()> {
    let dir = data_dir().context("no data directory")?;
    DeviceStore::open(&dir)?.set_device_token(origin_of(base), None)
}

/// Connect with the stored device token, keeping the store in sync with
/// what the gateway says about it.
fn connect_device(base: &::url::Url) -> Result<Connection, GatewayError> {
    let dir = data_dir().ok_or(GatewayError::Protocol("not-paired"))?;
    let mut store = DeviceStore::open(&dir).map_err(|_| GatewayError::Protocol("not-paired"))?;
    let origin = origin_of(base);
    let token = store
        .device_token(&origin)
        .ok_or(GatewayError::Protocol("not-paired"))?
        .to_owned();
    let (connection, hello) =
        match Connection::connect(base, &store.identity, Credential::Device(&token)) {
            Ok(connected) => connected,
            Err(error) => {
                // A mismatched device token can never succeed again: forget
                // it so the next attempt goes through pairing. Other refusals
                // (rate limit, device proof) keep it.
                if error.code() == "AUTH_DEVICE_TOKEN_MISMATCH" {
                    let _ = store.set_device_token(origin, None);
                }
                return Err(error);
            }
        };
    check_write_scope(&hello.scopes)?;
    if let Some(rotated) = hello.device_token.filter(|t| *t != token) {
        let _ = store.set_device_token(origin, Some(rotated));
    }
    Ok(connection)
}

/// Where [`send_instruction`] delivers the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendTarget {
    /// `chat.send` into an existing session key.
    Session(String),
    /// `sessions.create` with the message as its first turn.
    NewSession { agent_id: String },
}

/// Send `message` and return the key of the session that received it. Returns
/// once the gateway accepted the turn, without waiting for the agent's reply.
pub fn send_instruction(
    base: &::url::Url,
    target: &SendTarget,
    message: &str,
) -> Result<String, GatewayError> {
    let mut connection = connect_device(base)?;
    match target {
        SendTarget::Session(session_key) => {
            connection.request_with_timeout(
                "chat.send",
                json!({
                    "sessionKey": session_key,
                    "message": message,
                    "idempotencyKey": idempotency_key(),
                }),
                SEND_TIMEOUT,
            )?;
            Ok(session_key.clone())
        }
        SendTarget::NewSession { agent_id } => {
            let raw = connection.request_with_timeout(
                "sessions.create",
                json!({
                    "agentId": agent_id,
                    "message": message,
                    "idempotencyKey": idempotency_key(),
                }),
                SEND_TIMEOUT,
            )?;
            parse_created_session(raw.get())
        }
    }
}

fn parse_created_session(raw: &str) -> Result<String, GatewayError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Created {
        key: String,
        run_started: Option<bool>,
        run_error: Option<RunError>,
    }
    #[derive(Deserialize)]
    struct RunError {
        code: Option<String>,
    }
    let created: Created =
        serde_json::from_str(raw).map_err(|_| GatewayError::Protocol("sessions-create"))?;
    if created.run_started == Some(false) {
        let code = created
            .run_error
            .and_then(|e| e.code)
            .filter(|c| c.len() <= 64 && c.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or_else(|| "run-not-started".into());
        return Err(GatewayError::Rejected { code });
    }
    Ok(created.key)
}

/// An agent of the gateway, as shown in the setup picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub emoji: Option<String>,
    pub avatar: Option<AvatarSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvatarSource {
    /// Inlined image (the gateway embeds local avatar files).
    Inline { mime: String, bytes: Vec<u8> },
    Remote(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRoster {
    pub default_id: String,
    pub agents: Vec<Agent>,
}

/// Agents visible to this device (`agents.list`, `operator.read`).
pub fn list_agents(base: &::url::Url) -> Result<AgentRoster, GatewayError> {
    let mut connection = connect_device(base)?;
    let raw = connection.request("agents.list", json!({}))?;
    let mut roster = parse_roster(raw.get())?;
    // `agents.list` only carries avatars set in the gateway config; the
    // effective identity (workspace IDENTITY file included) comes from
    // `agent.identity.get`, inlined as a data URL for non-Control-UI clients.
    for agent in &mut roster.agents {
        let Ok(raw) = connection.request("agent.identity.get", json!({ "agentId": agent.id }))
        else {
            continue;
        };
        if let Some(identity) = parse_identity(raw.get()) {
            if let Some(name) = identity.name {
                agent.name = name;
            }
            if identity.emoji.is_some() {
                agent.emoji = identity.emoji;
            }
            if identity.avatar.is_some() {
                agent.avatar = identity.avatar;
            }
        }
    }
    Ok(roster)
}

struct EffectiveIdentity {
    name: Option<String>,
    emoji: Option<String>,
    avatar: Option<AvatarSource>,
}

fn parse_identity(raw: &str) -> Option<EffectiveIdentity> {
    #[derive(Deserialize)]
    struct Fields {
        name: Option<String>,
        emoji: Option<String>,
        avatar: Option<String>,
    }
    let fields: Fields = serde_json::from_str(raw).ok()?;
    Some(EffectiveIdentity {
        name: fields.name.filter(|n| !n.trim().is_empty()),
        emoji: fields.emoji,
        // Without an image the gateway puts the emoji or a default letter
        // here; only URLs are avatars.
        avatar: fields.avatar.as_deref().and_then(avatar_source),
    })
}

fn parse_roster(raw: &str) -> Result<AgentRoster, GatewayError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Roster {
        default_id: String,
        agents: Vec<Summary>,
    }
    #[derive(Deserialize)]
    struct Summary {
        id: String,
        name: Option<String>,
        kind: Option<String>,
        identity: Option<IdentityFields>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct IdentityFields {
        name: Option<String>,
        emoji: Option<String>,
        avatar_url: Option<String>,
    }
    let roster: Roster =
        serde_json::from_str(raw).map_err(|_| GatewayError::Protocol("agents-list"))?;
    let agents = roster
        .agents
        .into_iter()
        .filter(|a| a.kind.as_deref() != Some("system"))
        .map(|a| {
            let identity = a.identity;
            let name = identity
                .as_ref()
                .and_then(|i| i.name.clone())
                .or(a.name)
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| a.id.clone());
            let emoji = identity.as_ref().and_then(|i| i.emoji.clone());
            let avatar = identity
                .and_then(|i| i.avatar_url)
                .and_then(|url| avatar_source(&url));
            Agent {
                id: a.id,
                name,
                emoji,
                avatar,
            }
        })
        .collect();
    Ok(AgentRoster {
        default_id: roster.default_id,
        agents,
    })
}

fn avatar_source(url: &str) -> Option<AvatarSource> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',')?;
        let mime = meta.strip_suffix(";base64")?;
        let bytes = STANDARD.decode(data).ok()?;
        return Some(AvatarSource::Inline {
            mime: mime.to_ascii_lowercase(),
            bytes,
        });
    }
    let parsed = ::url::Url::parse(url).ok()?;
    matches!(parsed.scheme(), "http" | "https").then(|| AvatarSource::Remote(url.to_string()))
}

const AVATAR_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Image bytes and file extension of an avatar. Remote avatars are fetched
/// without credentials; blocking, call off the UI thread.
pub fn load_avatar(source: &AvatarSource) -> Option<(Vec<u8>, &'static str)> {
    let (mime, bytes) = match source {
        AvatarSource::Inline { mime, bytes } => (mime.clone(), bytes.clone()),
        AvatarSource::Remote(url) => {
            let response = ureq::get(url)
                .timeout(Duration::from_secs(10))
                .call()
                .ok()?;
            let mime = response.content_type().to_ascii_lowercase();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(
                &mut std::io::Read::take(response.into_reader(), AVATAR_MAX_BYTES),
                &mut bytes,
            )
            .ok()?;
            (mime, bytes)
        }
    };
    let ext = match mime.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        _ => return None,
    };
    (!bytes.is_empty()).then_some((bytes, ext))
}

fn check_write_scope(scopes: &[String]) -> Result<(), GatewayError> {
    // An empty list means the gateway didn't report scopes; let the
    // `chat.send` call itself be the judge.
    if scopes.is_empty()
        || scopes
            .iter()
            .any(|s| s == "operator.write" || s == "operator.admin")
    {
        Ok(())
    } else {
        Err(GatewayError::Rejected {
            code: "missing-write-scope".into(),
        })
    }
}

fn idempotency_key() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        bytes = nanos.to_le_bytes();
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("leswitcheur-{hex}")
}

const AVATAR_STEM: &str = "openclaw-avatar-";
const AVATAR_EXTS: [&str; 5] = ["png", "jpg", "webp", "gif", "svg"];

/// Cached avatar of the configured agent, shown on the switcher row. The
/// file name carries a content hash: GPUI caches images by path.
pub fn avatar_path() -> Option<PathBuf> {
    avatar_files().into_iter().next()
}

fn avatar_files() -> Vec<PathBuf> {
    let Some(entries) = data_dir().and_then(|dir| fs::read_dir(dir).ok()) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            name.starts_with(AVATAR_STEM) && AVATAR_EXTS.contains(&ext)
        })
        .collect()
}

/// Replace the cached avatar; `None` removes it.
pub fn store_avatar(avatar: Option<(&[u8], &str)>) -> Result<()> {
    use sha2::Digest as _;
    for path in avatar_files() {
        let _ = fs::remove_file(path);
    }
    if let Some((bytes, ext)) = avatar {
        anyhow::ensure!(AVATAR_EXTS.contains(&ext), "unsupported avatar format");
        let dir = data_dir().context("no data directory")?;
        let hash: String = sha2::Sha256::digest(bytes)[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(format!("{AVATAR_STEM}{hash}.{ext}")), bytes)?;
    }
    Ok(())
}

fn data_dir() -> Option<PathBuf> {
    switcheur_core::config::config_dir()
}

/// The paired device: its Ed25519 secret and the tokens gateways minted for
/// it, keyed by gateway origin. Owner-only permissions.
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct DeviceFile {
    #[serde(default)]
    secret_key: Option<String>,
    #[serde(default)]
    device_tokens: BTreeMap<String, String>,
}

struct DeviceStore {
    path: PathBuf,
    identity: Identity,
    device_tokens: BTreeMap<String, String>,
}

impl DeviceStore {
    fn open(dir: &Path) -> Result<Self> {
        let path = dir.join(DEVICE_FILE);
        let file = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<DeviceFile>(&bytes).unwrap_or_default(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DeviceFile::default(),
            Err(error) => return Err(error).context("read OpenClaw device identity"),
        };
        let secret = file
            .secret_key
            .and_then(|key| STANDARD.decode(key).ok())
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
        let store = Self {
            path,
            identity: match secret {
                Some(secret) => Identity::from_secret(secret),
                None => Identity::generate().context("generate OpenClaw device identity")?,
            },
            device_tokens: file.device_tokens,
        };
        if secret.is_none() {
            store.save()?;
        }
        Ok(store)
    }

    fn save(&self) -> Result<()> {
        let file = DeviceFile {
            secret_key: Some(STANDARD.encode(self.identity.secret())),
            device_tokens: self.device_tokens.clone(),
        };
        write_private(&self.path, &serde_json::to_vec_pretty(&file)?)
    }

    fn device_token(&self, origin: &str) -> Option<&str> {
        self.device_tokens.get(origin).map(String::as_str)
    }

    fn set_device_token(&mut self, origin: String, token: Option<String>) -> Result<()> {
        let changed = match token {
            Some(token) => self.device_tokens.insert(origin, token.clone()) != Some(token),
            None => self.device_tokens.remove(&origin).is_some(),
        };
        if changed {
            self.save()?;
        }
        Ok(())
    }
}

/// Device tokens are only ever sent back to the gateway that issued them.
fn origin_of(url: &::url::Url) -> String {
    url.origin().ascii_serialization()
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("device file has no parent")?;
    fs::create_dir_all(dir)?;
    let temporary = path.with_extension("json.tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_identity_persists_privately() {
        let dir = std::env::temp_dir().join(format!("leswitcheur-oc-{}", idempotency_key()));
        let mut store = DeviceStore::open(&dir).unwrap();
        let id = store.identity.device_id();
        store
            .set_device_token("http://gw".into(), Some("dt".into()))
            .unwrap();
        let reopened = DeviceStore::open(&dir).unwrap();
        assert_eq!(reopened.identity.device_id(), id);
        assert_eq!(reopened.device_token("http://gw"), Some("dt"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(DEVICE_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gateway_url_must_be_http_with_host() {
        assert!(parse_gateway_url(" http://gw.example:18789/ ").is_ok());
        assert!(parse_gateway_url("https://gw.example/oc").is_ok());
        assert!(parse_gateway_url("ftp://gw.example").is_err());
        assert!(parse_gateway_url("gw.example:18789").is_err());
    }

    #[test]
    fn default_session_opens_the_agent_chat() {
        let base = ::url::Url::parse("http://gw.example:18789/").unwrap();
        assert_eq!(
            conversation_url(&base, "agent:main:main").as_deref(),
            Some("http://gw.example:18789/chat/main")
        );
    }

    #[test]
    fn roster_parses_names_and_avatars() {
        let raw = r#"{"defaultId":"main","mainKey":"main","scope":"per-sender","agents":[
            {"id":"main","identity":{"name":"Pandai","emoji":"🐼","avatarUrl":"data:image/png;base64,iVBORw=="}},
            {"id":"ops","name":"Ops","identity":{"avatarUrl":"https://example.com/a.png"}},
            {"id":"bare"},
            {"id":"sys","kind":"system"}]}"#;
        let roster = parse_roster(raw).unwrap();
        assert_eq!(roster.default_id, "main");
        let names: Vec<_> = roster.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Pandai", "Ops", "bare"]);
        assert_eq!(roster.agents[0].emoji.as_deref(), Some("🐼"));
        assert!(matches!(
            &roster.agents[0].avatar,
            Some(AvatarSource::Inline { mime, .. }) if mime == "image/png"
        ));
        assert_eq!(
            roster.agents[1].avatar,
            Some(AvatarSource::Remote("https://example.com/a.png".into()))
        );
        assert_eq!(roster.agents[2].avatar, None);
    }

    #[test]
    fn identity_avatar_is_only_an_image_url() {
        let id = parse_identity(
            r#"{"agentId":"main","name":"Pandai","avatar":"data:image/webp;base64,UklGRg==","avatarStatus":"local"}"#,
        )
        .unwrap();
        assert_eq!(id.name.as_deref(), Some("Pandai"));
        assert!(matches!(id.avatar, Some(AvatarSource::Inline { ref mime, .. }) if mime == "image/webp"));
        let id = parse_identity(r#"{"agentId":"main","avatar":"🐼","avatarStatus":"none"}"#).unwrap();
        assert_eq!(id.avatar, None);
    }

    #[test]
    fn created_session_reports_refused_first_turn() {
        assert_eq!(
            parse_created_session(r#"{"ok":true,"key":"agent:main:abc","runStarted":true}"#),
            Ok("agent:main:abc".into())
        );
        assert_eq!(
            parse_created_session(
                r#"{"ok":true,"key":"agent:main:abc","runStarted":false,"runError":{"code":"UNAVAILABLE","message":"x"}}"#
            ),
            Err(GatewayError::Rejected {
                code: "UNAVAILABLE".into()
            })
        );
    }

    #[test]
    fn write_scope_is_required_when_reported() {
        assert!(check_write_scope(&[]).is_ok());
        assert!(check_write_scope(&["operator.write".into()]).is_ok());
        assert!(check_write_scope(&["operator.admin".into()]).is_ok());
        assert!(check_write_scope(&["operator.read".into()]).is_err());
    }
}
