//! Versioned metadata and focus commands for running local applications.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;

#[cfg(unix)]
pub mod unix;

pub const PROTOCOL: &str = "local-sources";
pub const VERSION: u32 = 1;
pub const AGENTSMON: &str = "agentsmon";
pub const MAX_FRAME: usize = 1024 * 1024;
pub const MAX_ENTRIES: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Active,
    Waiting,
    Idle,
    Compacting,
    #[serde(other)]
    Unknown,
}

/// Metadata only. The owning application keeps process/window/terminal targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub label: String,
    pub project_name: String,
    pub repository_path: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub custom_name: Option<String>,
    #[serde(default)]
    pub group_name: Option<String>,
    pub provider_name: String,
    #[serde(default)]
    pub accent_rgb: Option<u32>,
    pub state: SessionState,
    #[serde(default)]
    pub unread: bool,
    #[serde(default)]
    pub context_percent: Option<u8>,
    pub last_activity_ms: u64,
}

impl Entry {
    pub fn validate(&self) -> bool {
        !self.id.is_empty()
            && !self.label.trim().is_empty()
            && [
                &self.id,
                &self.label,
                &self.project_name,
                &self.repository_path,
                &self.provider_name,
            ]
            .iter()
            .all(|s| s.len() <= 8192)
            && [&self.title, &self.custom_name, &self.group_name]
                .iter()
                .all(|s| s.as_ref().is_none_or(|s| s.len() <= 8192))
            && self.context_percent.is_none_or(|p| p <= 100)
            && self.accent_rgb.is_none_or(|c| c <= 0xffffff)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Changes whenever the source restarts. A stale result cannot focus a new instance.
    pub instance: String,
    pub capabilities: Vec<String>,
    pub entries: Vec<Entry>,
}

impl Snapshot {
    pub fn validate(&self) -> io::Result<()> {
        let mut ids = std::collections::HashSet::new();
        if self.instance.is_empty()
            || self.instance.len() > 256
            || self.entries.len() > MAX_ENTRIES
            || !self.capabilities.iter().any(|c| c == "focus")
            || !self
                .entries
                .iter()
                .all(|e| e.validate() && ids.insert(&e.id))
        {
            return Err(invalid("invalid source snapshot"));
        }
        Ok(())
    }
}

/// Parse the envelope and check its version BEFORE interpreting method-specific data.
/// Unknown additive fields are accepted; breaking changes require a new version.
#[derive(Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol: String,
    pub version: u32,
    pub source: String,
    pub source_version: u32,
    pub method: String,
    #[serde(default)]
    pub payload: Value,
}

impl Envelope {
    pub fn new(source: &str, method: &str, payload: Value) -> Self {
        Self {
            protocol: PROTOCOL.into(),
            version: VERSION,
            source: source.into(),
            source_version: 1,
            method: method.into(),
            payload,
        }
    }

    pub fn check(&self, source: &str) -> io::Result<()> {
        if self.protocol != PROTOCOL
            || self.version != VERSION
            || self.source != source
            || self.source_version != 1
        {
            return Err(invalid("incompatible local source contract"));
        }
        Ok(())
    }

    pub fn error(source: &str, code: &str) -> Self {
        Self::new(source, "error", serde_json::json!({"code": code}))
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_FRAME {
            return Err(invalid("frame too large"));
        }
        serde_json::from_slice(bytes).map_err(invalid)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Focus {
    pub instance: String,
    pub id: String,
}

pub fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_fixture_is_stable_and_snapshot_validation_rejects_invalid_entries() {
        let envelope = Envelope::decode(include_bytes!("../fixtures/agentsmon-v1.json")).unwrap();
        envelope.check(AGENTSMON).unwrap();
        let mut snapshot: Snapshot = serde_json::from_value(envelope.payload).unwrap();
        snapshot.validate().unwrap();
        assert_eq!(snapshot.entries[0].label, "Auth API");
        snapshot.entries.push(snapshot.entries[0].clone());
        assert!(snapshot.validate().is_err());
        snapshot.entries.pop();
        snapshot.entries[0].context_percent = Some(101);
        assert!(snapshot.validate().is_err());
        snapshot.entries[0].context_percent = None;
        snapshot.capabilities.clear();
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn rejects_contract_drift_before_payload_decoding() {
        for field in ["protocol", "version", "source", "source_version"] {
            let mut json =
                serde_json::to_value(Envelope::new(AGENTSMON, "list", Value::Null)).unwrap();
            json[field] = if field.contains("version") {
                Value::from(42)
            } else {
                Value::from("other")
            };
            json["payload"] = Value::from("future payload format");
            let envelope = Envelope::decode(&serde_json::to_vec(&json).unwrap()).unwrap();
            assert!(envelope.check(AGENTSMON).is_err(), "{field}");
        }
    }

    #[test]
    fn allows_additive_fields_and_unknown_states() {
        let mut json = serde_json::to_value(Envelope::new(AGENTSMON, "list", Value::Null)).unwrap();
        json["new_optional_field"] = Value::Bool(true);
        Envelope::decode(&serde_json::to_vec(&json).unwrap())
            .unwrap()
            .check(AGENTSMON)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<SessionState>("\"future_state\"").unwrap(),
            SessionState::Unknown
        );
    }

    #[test]
    fn rejects_malformed_and_oversize_frames() {
        assert!(Envelope::decode(b"{").is_err());
        assert!(Envelope::decode(&vec![b' '; MAX_FRAME + 1]).is_err());
    }
}
