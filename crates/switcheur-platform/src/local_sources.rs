//! Registry of local sources; transport and contract validation are independent of UI.
#[cfg(unix)]
use local_source_ipc::{invalid, Envelope, Focus, Snapshot};
use std::io;
use switcheur_core::{Item, LocalSourceRef};

pub struct SourceDefinition {
    pub id: &'static str,
    pub name: &'static str,
}
pub const SOURCES: &[SourceDefinition] = &[SourceDefinition {
    id: local_source_ipc::AGENTSMON,
    name: "AgentsMon",
}];

/// Poll live sources every second; absent sources back off to eight seconds.
#[derive(Default)]
pub struct PollBackoff {
    failures: u32,
}

impl PollBackoff {
    pub fn next_delay(&mut self, available: bool) -> std::time::Duration {
        if available {
            self.failures = 0;
            std::time::Duration::from_secs(1)
        } else {
            let delay = std::time::Duration::from_secs(1 << self.failures.min(3));
            self.failures = (self.failures + 1).min(3);
            delay
        }
    }
}

pub fn is_disconnection(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
    )
}

pub fn list(source: &str) -> io::Result<Vec<Item>> {
    if !SOURCES.iter().any(|definition| definition.id == source) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unknown local source",
        ));
    }
    #[cfg(unix)]
    {
        use local_source_ipc::unix::{endpoint, request, LIST_TIMEOUT};
        let response = request(
            &endpoint(source)?,
            &Envelope::new(source, "list", serde_json::Value::Null),
            LIST_TIMEOUT,
        )?;
        let snapshot: Snapshot = serde_json::from_value(response.payload).map_err(invalid)?;
        snapshot.validate()?;
        let name = SOURCES
            .iter()
            .find(|definition| definition.id == source)
            .map(|definition| definition.name)
            .unwrap_or(source);
        Ok(snapshot
            .entries
            .into_iter()
            .map(|entry| {
                let mut target =
                    LocalSourceRef::new(source.into(), snapshot.instance.clone(), entry);
                target.source_name = name.into();
                Item::LocalSource(std::sync::Arc::new(target))
            })
            .collect())
    }
    #[cfg(not(unix))]
    Ok(Vec::new())
}

pub fn focus(target: &LocalSourceRef) -> io::Result<()> {
    if !SOURCES
        .iter()
        .any(|definition| definition.id == target.source)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unknown local source",
        ));
    }
    #[cfg(unix)]
    {
        use local_source_ipc::unix::{endpoint, request, FOCUS_TIMEOUT};
        let payload = serde_json::to_value(Focus {
            instance: target.instance.clone(),
            id: target.entry.id.clone(),
        })
        .map_err(invalid)?;
        let response = request(
            &endpoint(&target.source)?,
            &Envelope::new(&target.source, "focus", payload),
            FOCUS_TIMEOUT,
        )?;
        if response.payload.get("focused").and_then(|v| v.as_bool()) != Some(true) {
            return Err(invalid("focus was not acknowledged"));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "local sources require a Unix socket transport",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_source_retries_are_bounded_and_reset_on_reconnection() {
        let mut backoff = PollBackoff::default();
        let delays: Vec<_> = (0..7)
            .map(|_| backoff.next_delay(false).as_secs())
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 8, 8, 8]);
        assert_eq!(backoff.next_delay(true).as_secs(), 1);
        assert_eq!(backoff.next_delay(false).as_secs(), 1);
    }

    #[test]
    fn normal_disconnects_are_quiet_but_contract_errors_remain_visible() {
        for kind in [
            io::ErrorKind::NotFound,
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::UnexpectedEof,
        ] {
            assert!(is_disconnection(&io::Error::from(kind)));
        }
        assert!(!is_disconnection(&io::Error::from(
            io::ErrorKind::InvalidData
        )));
        assert!(!is_disconnection(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }
}
