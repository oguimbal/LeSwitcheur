//! Control UI deep links, ported from OpenClaw's `packages/session-url-contract`
//! (`buildControlUiSessionPath`) with its defaults: the `chat` namespace, main
//! key `main`, no exact-key mode and no display slug. The slug is left out on
//! purpose: it is derived from the conversation title.

const DEFAULT_AGENT_ID: &str = "main";
const MAIN_KEY: &str = "main";
const RESERVED_RESTS: [&str; 4] = ["main", "global", "boot", "sessions"];
const INCOGNITO_SURFACES: [&str; 3] = ["dashboard", "subagent", "internal-session-effects"];

/// Absolute Control UI URL for `session_key`, below the gateway's `base` URL
/// (scheme, host, port and optional base path, as typed in the browser).
pub fn session_url(
    base: &url::Url,
    session_key: &str,
    fallback_agent: Option<&str>,
) -> Option<String> {
    let path = session_path(base.path(), session_key, fallback_agent)?;
    let origin = base.origin();
    origin
        .is_tuple()
        .then(|| format!("{}{path}", origin.ascii_serialization()))
}

pub fn session_path(
    base_path: &str,
    session_key: &str,
    fallback_agent: Option<&str>,
) -> Option<String> {
    let raw_key = session_key.trim();
    if raw_key.is_empty() {
        return None;
    }
    let parsed = agent_session_key_parts(raw_key);
    if parsed.is_none() && raw_key.to_lowercase().starts_with("agent:") {
        return None;
    }
    let is_parsed = parsed.is_some();
    let (agent_id, rest) = match parsed {
        Some((agent_id, rest)) => (agent_id, rest),
        None => (
            normalize_agent_id(fallback_agent.filter(|id| !id.trim().is_empty())?),
            raw_key,
        ),
    };
    let namespace = format!("{}/chat", normalize_base_path(base_path));
    let agent = encode_path_segment(&agent_id);
    let normalized_rest = rest.to_lowercase();
    if normalized_rest == MAIN_KEY
        || (!is_parsed && (normalized_rest == MAIN_KEY || normalized_rest == "global"))
    {
        return Some(format!("{namespace}/{agent}"));
    }
    let segments: Vec<&str> = rest.split(':').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    let literal = || {
        let encoded: Vec<_> = segments.iter().map(|s| encode_path_segment(s)).collect();
        format!("{namespace}/{agent}/{}", encoded.join("/"))
    };
    let exact_key = || {
        format!(
            "{namespace}/{agent}/~key/{}",
            encode_path_segment(segments[0])
        )
    };
    if is_incognito_session_key(raw_key) || normalized_rest == "global" {
        return Some(
            if segments.len() == 1
                && (is_reserved_rest(segments[0]) || is_short_session_ref(segments[0]))
            {
                exact_key()
            } else {
                literal()
            },
        );
    }
    if is_parsed {
        if let Some(uuid) = uuid_suffix(rest) {
            let mut length = 8;
            while length < uuid.len() && is_reserved_rest(&uuid[..length]) {
                length += 1;
            }
            let session_ref = &uuid[..length];
            return (!is_reserved_rest(session_ref))
                .then(|| format!("{namespace}/{agent}/{session_ref}"));
        }
    }
    if segments.len() == 1 && !is_reserved_rest(segments[0]) && is_short_session_ref(segments[0]) {
        return Some(exact_key());
    }
    Some(literal())
}

/// `agent:<id>:<rest>` with a normalized agent id and an untouched rest.
fn agent_session_key_parts(key: &str) -> Option<(String, &str)> {
    if !key.get(..6)?.eq_ignore_ascii_case("agent:") {
        return None;
    }
    let (agent_id, rest) = key[6..].split_once(':')?;
    let agent_id = agent_id.trim();
    if agent_id.is_empty()
        || rest.is_empty()
        || rest.starts_with(':')
        || rest.split(':').any(str::is_empty)
    {
        return None;
    }
    Some((normalize_agent_id(agent_id), rest))
}

pub(crate) fn normalize_agent_id(value: &str) -> String {
    let trimmed = value.trim();
    let valid = trimmed.len() <= 64
        && trimmed
            .chars()
            .enumerate()
            .all(|(index, c)| c.is_ascii_alphanumeric() || (index > 0 && (c == '_' || c == '-')))
        && !trimmed.is_empty();
    if valid {
        return trimmed.to_ascii_lowercase();
    }
    let mut normalized = String::new();
    let mut in_invalid_run = false;
    for c in trimmed.to_lowercase().chars() {
        let valid = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
        if valid {
            normalized.push(c);
        } else if !in_invalid_run {
            // A run of invalid characters becomes one dash.
            normalized.push('-');
        }
        in_invalid_run = !valid;
    }
    let normalized: String = normalized.trim_matches('-').chars().take(64).collect();
    if normalized.is_empty() {
        DEFAULT_AGENT_ID.to_owned()
    } else {
        normalized
    }
}

fn normalize_base_path(base_path: &str) -> String {
    let trimmed = base_path.trim().trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("/{trimmed}")
    }
}

fn is_reserved_rest(rest: &str) -> bool {
    let normalized = rest.to_lowercase();
    RESERVED_RESTS.contains(&normalized.as_str()) || normalized == MAIN_KEY
}

/// `^(?:.*-)?([0-9a-f]{8,32})$`, case-insensitively.
fn is_short_session_ref(segment: &str) -> bool {
    let tail = segment.rsplit('-').next().unwrap_or(segment);
    (8..=32).contains(&tail.len()) && tail.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_incognito_session_key(key: &str) -> bool {
    let key = key.trim().to_lowercase();
    let Some(rest) = key.strip_prefix("agent:") else {
        return false;
    };
    let mut parts = rest.split(':');
    let (Some(agent), Some(surface), Some(tail), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    !agent.is_empty()
        && INCOGNITO_SURFACES.contains(&surface)
        && tail.len() > "incognito-".len()
        && tail.starts_with("incognito-")
}

/// A trailing UUID, lowercased without dashes.
fn uuid_suffix(rest: &str) -> Option<String> {
    let candidate = rest.get(rest.len().checked_sub(36)?..)?;
    let groups: Vec<&str> = candidate.split('-').collect();
    let shaped = groups.iter().map(|group| group.len()).eq([8, 4, 4, 4, 12])
        && groups
            .iter()
            .all(|group| group.chars().all(|c| c.is_ascii_hexdigit()));
    shaped.then(|| candidate.replace('-', "").to_ascii_lowercase())
}

/// JavaScript `encodeURIComponent`, then dots escaped so the gateway never
/// mistakes a segment for a static asset.
fn encode_path_segment(segment: &str) -> String {
    match segment {
        "." => return "~dot".into(),
        ".." => return "~dotdot".into(),
        _ => {}
    }
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => encoded.push(byte as char),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    if encoded.starts_with('~') {
        format!("~{encoded}")
    } else {
        encoded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID_KEY: &str = "agent:main:dashboard:12345678-90ab-cdef-1234-567890abcdef";

    #[test]
    fn matches_the_openclaw_contract_vectors() {
        for (key, expected) in [
            (
                "agent:ops:catalog:fixture:Host:Thread",
                "/chat/ops/catalog/fixture/Host/Thread",
            ),
            ("agent:main:main", "/chat/main"),
            ("agent:research:global", "/chat/research/~key/global"),
            ("AGENT:RESEARCH:GlObAl", "/chat/research/~key/GlObAl"),
            ("agent:research:global:notes", "/chat/research/global/notes"),
            (
                "agent:main:cron:.:..:run",
                "/chat/main/cron/~dot/~dotdot/run",
            ),
            ("agent:main:channel:~dot", "/chat/main/channel/~~dot"),
            ("agent:main:~key", "/chat/main/~~key"),
            ("agent:main:12345678", "/chat/main/~key/12345678"),
            (
                "agent:main:release-deadbeef",
                "/chat/main/~key/release-deadbeef",
            ),
            (UUID_KEY, "/chat/main/12345678"),
            (
                "agent:main:subagent:incognito-12345678-90ab-cdef-1234-567890abcdef",
                "/chat/main/subagent/incognito-12345678-90ab-cdef-1234-567890abcdef",
            ),
            ("agent:AſB:control-link", "/chat/a-b/control-link"),
            ("agent:Research Agent!:x", "/chat/research-agent/x"),
            (
                "agent:main:channel:release.js",
                "/chat/main/channel/release%2Ejs",
            ),
            ("agent:main:x:$Event", "/chat/main/x/%24Event"),
        ] {
            assert_eq!(
                session_path("", key, None).as_deref(),
                Some(expected),
                "{key}"
            );
        }
        for (key, agent, expected) in [
            ("main", "research", "/chat/research"),
            (
                "telegram:group:12345",
                "research",
                "/chat/research/telegram/group/12345",
            ),
            ("control-link", "OPS_TEAM", "/chat/ops_team/control-link"),
            (
                "control-link",
                "Research Agent!",
                "/chat/research-agent/control-link",
            ),
            ("control-link", "ſ", "/chat/main/control-link"),
        ] {
            assert_eq!(
                session_path("", key, Some(agent)).as_deref(),
                Some(expected),
                "{key}"
            );
        }
    }

    #[test]
    fn rejects_malformed_keys() {
        for key in [
            "",
            "agent:main",
            "agent::control-link",
            "agent:main:",
            "agent:main:telegram::12345",
            "agent:ops::main",
            "telegram:12345",
        ] {
            assert_eq!(session_path("", key, None), None, "{key}");
        }
    }

    #[test]
    fn keeps_the_base_path_and_origin() {
        assert_eq!(
            session_path(" /control/// ", "agent:research:global", None).as_deref(),
            Some("/control/chat/research/~key/global")
        );
        let base = url::Url::parse("http://pandai.example:18789/").unwrap();
        assert_eq!(
            session_url(&base, UUID_KEY, None).as_deref(),
            Some("http://pandai.example:18789/chat/main/12345678")
        );
        let base = url::Url::parse("https://gw.example/openclaw").unwrap();
        assert_eq!(
            session_url(&base, "agent:ops:main", None).as_deref(),
            Some("https://gw.example/openclaw/chat/ops")
        );
    }
}
