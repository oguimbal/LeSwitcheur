# Local source IPC, contract v1

This crate is vendored at `crates/local-source-ipc` in LeSwitcheur and AgentsMon.
Both repositories build independently; neither depends on a sibling checkout.
Keep the crate and its `fixtures/agentsmon-v1.json` fixture identical when changing
v1. Application release numbers are unrelated to contract versions.

## Transport and lifecycle

On macOS (and Unix), a running source listens on
`/tmp/oliv-local-sources-<effective-uid>/<source>.sock`. The directory must be owned
by that user with mode 0700; sockets are mode 0600. Only local processes running
as that user can access session metadata or request focus. The server holds an
exclusive lock while owning its endpoint and reclaims a stale socket on restart.
The client never launches a source and never reads its on-disk session storage.
No network port is opened.

Windows currently returns no local results. A future named-pipe transport can
reuse the contract, validation and source adapters without changing search/UI.
It must restrict access to the current user and preserve the same bounds.

Each connection carries one request and one response. A frame is a four-byte
unsigned big-endian length followed by UTF-8 JSON, at most 1 MiB. Overall client
deadlines, including connecting and receiving partial frames, are 750 ms for
listing and 3 s for focus. Requests are never automatically retried after a focus
timeout. The server uses two fixed workers, a bounded UI action queue and
expiring focus commands. Expired queued commands are discarded before activation.

## Envelope

All messages have these fields:

```json
{"protocol":"local-sources","version":1,"source":"agentsmon","source_version":1,"method":"list","payload":null}
```

`protocol`, `version`, `source` and `source_version` must match before interpreting
`payload`. The transport version and the per-source contract version are checked
in both directions. Additive optional fields are accepted. A breaking change
requires incrementing the corresponding version. Unsupported versions return an
`error` response with code `incompatible_version`; neither app crashes. Unknown
states map to a neutral `unknown` display state. Unknown methods return
`unsupported_method`. The client also validates capabilities, unique IDs, entry
count, string lengths, percentages and colours.

The source returns `method: "list"` with a snapshot payload (see the fixture):

- `instance`: opaque ID generated at process startup.
- `capabilities`: includes `list` and `focus` in v1.
- `entries`: up to 1,000 visible live sessions. Entries carry a stable opaque `id`,
  `label`, `project_name`, `repository_path`, optional `title`, `custom_name` and
  `group_name`, `provider_name`, optional `accent_rgb`, `state`, `unread`, optional
  integer `context_percent`, and `last_activity_ms` (Unix milliseconds).

No conversation messages, credentials, host PIDs or terminal focus targets are
exposed. AgentsMon uses its existing provider metadata and store, including live
renames, hidden-session filtering and unread notification state.

Focus requests echo the instance and entry ID received from the source:

```json
{"protocol":"local-sources","version":1,"source":"agentsmon","source_version":1,"method":"focus","payload":{"instance":"test-instance-1","id":"opaque-session-id"}}
```

AgentsMon verifies the instance and current visibility, then dispatches through
its existing `focus_agent_result` action on its UI thread. This is the same path
used by card clicks and notification clicks. Only successful activation clears
unread state. Success is acknowledged as `method: "focus"`,
`payload: {"focused":true}`. Errors use `method: "error"` and
`payload: {"code":"stale_session"}` (or `busy`, `timeout`, `invalid_request`,
`provider_unavailable`, `focus_failed`, `unavailable`). A restart or removed
session cannot silently redirect a stale selection to another target.

## LeSwitcheur behaviour

`switcheur-platform/src/local_sources.rs` registers source IDs and display names.
Each panel starts fresh and polls each registered source independently off the UI
thread once per second. Failed connections back off for 1, 2, 4, then 8 seconds
(maximum); a successful reply resets the interval to one second. Closing the
panel stops polling. Normal disconnects are silent and clear stale results. Unavailability, malformed replies and incompatibility clear that source's
results; diagnostics are logged only when the error changes during an open
panel. The original panel is closed after a focus acknowledgement; an activation
error leaves a translated message if the panel is still open. A late response
cannot close a newly opened panel.

With an empty query, the upper section keeps Currently Playing and adds up to
three agent sessions: waiting (orange) first, newest activity first within that
priority, then unread notifications to fill remaining places. Other sessions
are searchable. With a query, local matches follow window matches and precede
browser tabs; they suppress the LLM fallback when present.

Every query word must fuzzy-match at least one field. Nucleo scores are weighted:
visible label/custom name 100%, conversation title 85%, group override 80%,
repository name 70%, full path 35%. Scores of the best field for each word are
summed; duplicate fields do not add extra credit. A query can combine repository
and conversation words. Only visible-label offsets are returned for highlights.
The selected target is preserved across live refreshes.

Rows reuse LeSwitcheur's 44px layout and theme. Provider rings and context pies
are 26px, with AgentsMon's state colours and unread dot; status text supplements
colour. No provider-specific focus implementation is added to LeSwitcheur.

## Verification

Run `cargo test -p local-source-ipc` in either repository. This tests the v1
fixture, version drift, additive fields, unknown states, malformed frames,
size limits, real socket exchanges, duplicate servers, stale sockets and
partial-frame deadlines. Subprocess tests kill the source during a request,
reconnect after a restart at the same socket path, and kill clients midway through
a frame to verify that server workers are released. In LeSwitcheur, run
`cargo test -p switcheur-core --test local_sources` for ranking, mixed-field
queries, priorities, disconnection and selection preservation. AgentsMon's
`local_source` tests cover metadata projection and focus dispatch/acknowledgement.

For visual QA without touching live sessions, run from LeSwitcheur:
`cargo run -p switcheur --example preview_local_sources` (or add `-- --light`).
The preview uses synthetic sessions and does not register global hotkeys or
activate any real window/session. Escape closes it.

Before release, test with the updated AgentsMon process: search by repository,
path, agent title and custom session/group name; focus via click and Enter;
compare focus with the AgentsMon card; change waiting/unread state while the
panel is open; stop/restart AgentsMon; and exercise the window/tab/Space focus
regression scenarios in LeSwitcheur's AGENTS.md. The fixture and unit tests do
not establish native focus correctness for every host or Space configuration.
