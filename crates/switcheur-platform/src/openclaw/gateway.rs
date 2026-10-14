//! Minimal, blocking OpenClaw Gateway operator client (wire protocol v4).
//! Ported from AgentsMon's read-only client; this one asks for
//! `operator.write` because it sends `chat.send`.
//!
//! The connection advertises `session-scoped-events` so chat/agent streams
//! are never pushed and never subscribes to anything. Event payloads are
//! skipped without being decoded, except the connect challenge. Errors carry
//! protocol codes, never server messages (they may echo the instruction).

use std::io::ErrorKind;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use serde::Deserialize;
use serde_json::{json, value::RawValue, Value};
use sha2::{Digest, Sha256};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

pub const PROTOCOL_VERSION: u32 = 4;
const CLIENT_ID: &str = "cli";
const CLIENT_MODE: &str = "cli";
const ROLE: &str = "operator";
const SCOPES: [&str; 1] = ["operator.write"];
const CAPS: [&str; 1] = ["session-scoped-events"];
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// hello-ok carries the method registry and a presence snapshot.
const MAX_MESSAGE: usize = 8 << 20;
/// Frames drained while waiting for one response, a bound against floods.
const MAX_FRAMES_PER_REQUEST: usize = 512;

/// Persistent Ed25519 device identity, as paired with the gateway.
#[derive(Clone)]
pub struct Identity {
    key: SigningKey,
}

impl Identity {
    pub fn generate() -> std::io::Result<Self> {
        let mut secret = [0u8; 32];
        getrandom::getrandom(&mut secret).map_err(std::io::Error::other)?;
        Ok(Self::from_secret(secret))
    }

    pub fn from_secret(secret: [u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(&secret),
        }
    }

    pub fn secret(&self) -> [u8; 32] {
        self.key.to_bytes()
    }

    /// Hex SHA-256 of the raw public key.
    pub fn device_id(&self) -> String {
        Sha256::digest(self.key.verifying_key().to_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.key.verifying_key().to_bytes())
    }

    fn sign(&self, payload: &str) -> String {
        URL_SAFE_NO_PAD.encode(self.key.sign(payload.as_bytes()).to_bytes())
    }
}

/// Which secret authenticates the connect request.
#[derive(Clone, Copy)]
pub enum Credential<'a> {
    /// Token minted by a previous approved pairing, scoped to `operator.write`.
    Device(&'a str),
    /// The gateway's shared token (`gateway.auth.token`).
    SharedToken(&'a str),
    /// The gateway's shared password (`gateway.auth.password`).
    Password(&'a str),
    None,
}

impl<'a> Credential<'a> {
    fn auth(self) -> Value {
        match self {
            Credential::Device(token) => json!({ "deviceToken": token }),
            Credential::SharedToken(token) => json!({ "token": token }),
            Credential::Password(password) => json!({ "password": password }),
            Credential::None => json!({}),
        }
    }

    /// The token bound into the device signature: whichever token is sent.
    fn signature_token(self) -> &'a str {
        match self {
            Credential::Device(token) | Credential::SharedToken(token) => token,
            Credential::Password(_) | Credential::None => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayError {
    /// Network, TLS or WebSocket failure; retry with backoff.
    Transport(&'static str),
    /// The gateway answered something this client does not understand.
    Protocol(&'static str),
    /// A new device waits for `openclaw devices approve <request_id>`.
    /// `pause`: the gateway asks clients not to retry on a timer.
    PairingRequired {
        request_id: Option<String>,
        pause: bool,
    },
    /// Credentials refused. `code` is the gateway's detail code.
    Auth { code: String },
    /// The device proof itself was refused (`DEVICE_AUTH_*`): a client bug or
    /// a clock problem, never a reason to discard a stored device token.
    DeviceAuth { code: String },
    /// Any other refused request, by gateway error code.
    Rejected { code: String },
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GatewayError::Transport(what) => write!(f, "transport: {what}"),
            GatewayError::Protocol(what) => write!(f, "protocol: {what}"),
            GatewayError::PairingRequired { request_id, .. } => write!(
                f,
                "pairing required (request {})",
                request_id.as_deref().unwrap_or("unknown")
            ),
            GatewayError::Auth { code } => write!(f, "authentication refused: {code}"),
            GatewayError::DeviceAuth { code } => write!(f, "device proof refused: {code}"),
            GatewayError::Rejected { code } => write!(f, "request refused: {code}"),
        }
    }
}

impl std::error::Error for GatewayError {}

impl GatewayError {
    /// Short content-free code for the UI and logs.
    pub fn code(&self) -> String {
        match self {
            GatewayError::Transport(what) | GatewayError::Protocol(what) => (*what).to_owned(),
            GatewayError::PairingRequired { .. } => "PAIRING_REQUIRED".to_owned(),
            GatewayError::Auth { code }
            | GatewayError::DeviceAuth { code }
            | GatewayError::Rejected { code } => code.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Hello {
    pub server_version: String,
    pub device_token: Option<String>,
    pub scopes: Vec<String>,
}

pub struct Connection {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

#[derive(Deserialize)]
struct Frame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    payload: Option<Box<RawValue>>,
    #[serde(default)]
    error: Option<ErrorShape>,
}

/// `message` is deliberately not decoded: it may echo request input.
#[derive(Deserialize)]
struct ErrorShape {
    code: String,
    #[serde(default)]
    details: Option<ErrorDetails>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorDetails {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    pause_reconnect: Option<bool>,
}

#[derive(Deserialize)]
struct Challenge {
    nonce: String,
    ts: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelloOk {
    protocol: u32,
    server: HelloServer,
    auth: HelloAuth,
}

#[derive(Deserialize)]
struct HelloServer {
    version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelloAuth {
    #[serde(default)]
    device_token: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
}

/// `ws(s)://` endpoint for a Control UI URL (`http(s)://`), keeping its path.
pub fn websocket_url(base: &url::Url) -> Result<url::Url, GatewayError> {
    let mut url = base.clone();
    let scheme = match base.scheme() {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        _ => return Err(GatewayError::Protocol("unsupported-url-scheme")),
    };
    url.set_scheme(scheme)
        .map_err(|()| GatewayError::Protocol("unsupported-url-scheme"))?;
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

impl Connection {
    pub fn connect(
        base: &url::Url,
        identity: &Identity,
        credential: Credential<'_>,
    ) -> Result<(Self, Hello), GatewayError> {
        let url = websocket_url(base)?;
        let addrs = url
            .socket_addrs(|| None)
            .map_err(|_| GatewayError::Transport("resolve"))?;
        let stream = addrs
            .iter()
            .find_map(|addr| TcpStream::connect_timeout(addr, CONNECT_TIMEOUT).ok())
            .ok_or(GatewayError::Transport("connect"))?;
        stream
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(REQUEST_TIMEOUT)))
            .map_err(|_| GatewayError::Transport("socket-options"))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_MESSAGE));
        let (socket, _) =
            tungstenite::client_tls_with_config(url.as_str(), stream, Some(config), None)
                .map_err(|_| GatewayError::Transport("websocket-handshake"))?;
        let mut connection = Self { socket, next_id: 0 };
        let hello = connection.handshake(identity, credential)?;
        Ok((connection, hello))
    }

    fn handshake(
        &mut self,
        identity: &Identity,
        credential: Credential<'_>,
    ) -> Result<Hello, GatewayError> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let challenge = loop {
            let frame = self.read_frame(deadline)?;
            if frame.kind == "event" && frame.event.as_deref() == Some("connect.challenge") {
                let payload = frame
                    .payload
                    .ok_or(GatewayError::Protocol("challenge-payload"))?;
                break serde_json::from_str::<Challenge>(payload.get())
                    .map_err(|_| GatewayError::Protocol("challenge-payload"))?;
            }
        };
        let token = credential.signature_token();
        let platform = platform();
        let scopes = SCOPES.join(",");
        let signed_at = challenge.ts.to_string();
        let device_id = identity.device_id();
        let payload = signature_payload(&[
            &device_id,
            &scopes,
            &signed_at,
            token,
            &challenge.nonce,
            platform,
        ]);
        let params = json!({
            "minProtocol": PROTOCOL_VERSION,
            "maxProtocol": PROTOCOL_VERSION,
            "client": {
                "id": CLIENT_ID,
                "displayName": "LeSwitcheur",
                "version": env!("CARGO_PKG_VERSION"),
                "platform": platform,
                "mode": CLIENT_MODE,
            },
            "role": ROLE,
            "scopes": SCOPES,
            "caps": CAPS,
            "auth": credential.auth(),
            "userAgent": concat!("leswitcheur/", env!("CARGO_PKG_VERSION")),
            "device": {
                "id": device_id,
                "publicKey": identity.public_key(),
                "signature": identity.sign(&payload),
                "signedAt": challenge.ts,
                "nonce": challenge.nonce,
            },
        });
        let payload = self.request("connect", params)?;
        let hello: HelloOk = serde_json::from_str(payload.get())
            .map_err(|_| GatewayError::Protocol("hello-payload"))?;
        if hello.protocol != PROTOCOL_VERSION {
            return Err(GatewayError::Protocol("protocol-version"));
        }
        Ok(Hello {
            server_version: hello.server.version,
            device_token: hello.auth.device_token.filter(|token| !token.is_empty()),
            scopes: hello.auth.scopes,
        })
    }

    /// Send one request and wait for its response, skipping events unread.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Box<RawValue>, GatewayError> {
        self.request_with_timeout(method, params, REQUEST_TIMEOUT)
    }

    pub fn request_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Box<RawValue>, GatewayError> {
        self.next_id += 1;
        let id = self.next_id.to_string();
        let frame = json!({ "type": "req", "id": id, "method": method, "params": params });
        self.socket
            .send(Message::text(frame.to_string()))
            .map_err(|_| GatewayError::Transport("send"))?;
        let deadline = Instant::now() + timeout;
        for _ in 0..MAX_FRAMES_PER_REQUEST {
            let frame = self.read_frame(deadline)?;
            if frame.kind != "res" || frame.id.as_deref() != Some(id.as_str()) {
                continue;
            }
            if frame.ok == Some(true) {
                return frame
                    .payload
                    .ok_or(GatewayError::Protocol("response-payload"));
            }
            return Err(classify(frame.error));
        }
        Err(GatewayError::Protocol("response-flood"))
    }

    /// Next text frame, with its envelope decoded and its payload left raw.
    fn read_frame(&mut self, deadline: Instant) -> Result<Frame, GatewayError> {
        loop {
            if Instant::now() >= deadline {
                return Err(GatewayError::Transport("timeout"));
            }
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    return serde_json::from_str(text.as_str())
                        .map_err(|_| GatewayError::Protocol("frame"));
                }
                Ok(Message::Close(_)) => return Err(GatewayError::Transport("closed")),
                Ok(_) => {}
                // The socket timeout is shorter than slow requests' deadline:
                // keep waiting until the deadline itself passes.
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => return Err(GatewayError::Transport("read")),
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.socket.close(None);
        let _ = self.socket.flush();
    }
}

fn classify(error: Option<ErrorShape>) -> GatewayError {
    let Some(error) = error else {
        return GatewayError::Protocol("error-payload");
    };
    let details = error.details.unwrap_or(ErrorDetails {
        code: None,
        request_id: None,
        pause_reconnect: None,
    });
    match details.code.as_deref() {
        Some("PAIRING_REQUIRED") => GatewayError::PairingRequired {
            request_id: details.request_id.filter(|id| is_safe_code(id)),
            pause: details.pause_reconnect == Some(true),
        },
        Some(code) if code.starts_with("DEVICE_AUTH_") || code == "DEVICE_IDENTITY_REQUIRED" => {
            GatewayError::DeviceAuth {
                code: safe_code(code),
            }
        }
        Some(code) if code.starts_with("AUTH_") || code == "OPERATOR_ACCESS_DENIED" => {
            GatewayError::Auth {
                code: safe_code(code),
            }
        }
        Some(code) => GatewayError::Rejected {
            code: safe_code(code),
        },
        None => GatewayError::Rejected {
            code: safe_code(&error.code),
        },
    }
}

/// Codes and request ids are shown to the user: keep them short identifiers.
fn is_safe_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

fn safe_code(value: &str) -> String {
    if is_safe_code(value) {
        value.to_owned()
    } else {
        "unrecognized".to_owned()
    }
}

/// Node's `process.platform`, as the OpenClaw CLI reports it.
fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// `v3|deviceId|clientId|clientMode|role|scopes|signedAtMs|token|nonce|platform|deviceFamily`
/// (`buildDeviceAuthPayloadV3`); platform and device family are lowercased.
fn signature_payload([device_id, scopes, signed_at, token, nonce, platform]: &[&str; 6]) -> String {
    [
        "v3",
        device_id,
        CLIENT_ID,
        CLIENT_MODE,
        ROLE,
        scopes,
        signed_at,
        token,
        nonce,
        &platform.trim().to_ascii_lowercase(),
        "",
    ]
    .join("|")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier};

    #[test]
    fn device_proof_matches_the_v3_contract() {
        let identity = Identity::from_secret([7; 32]);
        let device_id = identity.device_id();
        assert_eq!(device_id.len(), 64);
        let payload = signature_payload(&[
            &device_id,
            "operator.write",
            "1737264000000",
            "tok",
            "nonce-1",
            "Darwin",
        ]);
        assert_eq!(
            payload,
            format!(
                "v3|{device_id}|cli|cli|operator|operator.write|1737264000000|tok|nonce-1|darwin|"
            )
        );
        let signature: [u8; 64] = URL_SAFE_NO_PAD
            .decode(identity.sign(&payload))
            .unwrap()
            .try_into()
            .unwrap();
        identity
            .key
            .verifying_key()
            .verify(payload.as_bytes(), &Signature::from_bytes(&signature))
            .unwrap();
        let public_key = URL_SAFE_NO_PAD.decode(identity.public_key()).unwrap();
        let expected_id: String = Sha256::digest(&public_key)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(device_id, expected_id);
    }

    #[test]
    fn tls_has_exactly_one_crypto_provider() {
        // rustls panics at the first wss:// connect when the build enables
        // no crypto provider, or more than one.
        let _ = rustls::ClientConfig::builder();
    }

    #[test]
    fn websocket_url_follows_the_control_ui_scheme() {
        let url = |raw: &str| {
            websocket_url(&url::Url::parse(raw).unwrap())
                .unwrap()
                .to_string()
        };
        assert_eq!(url("http://gw.example:18789/"), "ws://gw.example:18789/");
        assert_eq!(
            url("https://gw.example/base?x=1#token=s"),
            "wss://gw.example/base"
        );
        assert!(websocket_url(&url::Url::parse("ftp://gw.example").unwrap()).is_err());
    }

    #[test]
    fn errors_expose_codes_but_never_messages() {
        let error = |raw: &str| classify(serde_json::from_str(raw).unwrap());
        assert_eq!(
            error(
                r#"{"code":"NOT_PAIRED","message":"secret input","details":{"code":"PAIRING_REQUIRED","requestId":"req-42"}}"#
            ),
            GatewayError::PairingRequired {
                request_id: Some("req-42".into()),
                pause: false
            }
        );
        assert_eq!(
            error(
                r#"{"code":"X","message":"m","details":{"code":"PAIRING_REQUIRED","requestId":"bad id <script>","pauseReconnect":true}}"#
            ),
            GatewayError::PairingRequired {
                request_id: None,
                pause: true
            }
        );
        assert_eq!(
            error(
                r#"{"code":"X","message":"m","details":{"code":"DEVICE_AUTH_SIGNATURE_INVALID"}}"#
            ),
            GatewayError::DeviceAuth {
                code: "DEVICE_AUTH_SIGNATURE_INVALID".into()
            }
        );
        assert_eq!(
            error(
                r#"{"code":"UNAUTHORIZED","message":"m","details":{"code":"AUTH_TOKEN_MISMATCH"}}"#
            ),
            GatewayError::Auth {
                code: "AUTH_TOKEN_MISMATCH".into()
            }
        );
        assert_eq!(
            error(r#"{"code":"INVALID REQUEST!","message":"m"}"#),
            GatewayError::Rejected {
                code: "unrecognized".into()
            }
        );
    }
}
