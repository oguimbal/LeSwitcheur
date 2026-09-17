use std::sync::Arc;
use switcheur_core::{
    FuzzyMatcher, Item, LocalSessionState, LocalSourceEntry, LocalSourceRef, Section, SwitcherState,
};

fn session(id: &str, label: &str, state: LocalSessionState, unread: bool, time: u64) -> Item {
    Item::LocalSource(Arc::new(LocalSourceRef::new(
        "agentsmon".into(),
        "run-1".into(),
        LocalSourceEntry {
            id: id.into(),
            label: label.into(),
            project_name: "repository".into(),
            repository_path: "/Users/example/work/repository".into(),
            title: Some("Conversation title".into()),
            custom_name: None,
            group_name: None,
            provider_name: "Claude".into(),
            accent_rgb: Some(0xcc785c),
            state,
            unread,
            context_percent: Some(42),
            last_activity_ms: time,
        },
    )))
}

fn edit(item: Item, change: impl FnOnce(&mut LocalSourceEntry)) -> Item {
    let Item::LocalSource(source) = item else {
        unreachable!()
    };
    let mut entry = source.entry.clone();
    change(&mut entry);
    Item::LocalSource(Arc::new(LocalSourceRef::new(
        source.source.clone(),
        source.instance.clone(),
        entry,
    )))
}

fn id(item: &Item) -> &str {
    let Item::LocalSource(s) = item else {
        panic!("not a session")
    };
    &s.entry.id
}

#[test]
fn matching_prefers_visible_name_then_title_then_project_then_path() {
    let base = || session("x", "Other", LocalSessionState::Active, false, 0);
    let fields = vec![
        edit(base(), |e| {
            e.id = "path".into();
            e.repository_path = "/Users/needle/repo".into();
        }),
        edit(base(), |e| {
            e.id = "project".into();
            e.project_name = "needle".into();
        }),
        edit(base(), |e| {
            e.id = "title".into();
            e.title = Some("needle".into());
        }),
        edit(base(), |e| {
            e.id = "label".into();
            e.label = "needle".into();
            e.custom_name = Some("needle".into());
        }),
    ];
    let results = FuzzyMatcher::new().rank("nedl", &fields);
    assert_eq!(
        results.iter().map(|r| id(&r.item)).collect::<Vec<_>>(),
        ["label", "title", "project", "path"]
    );
    assert!(!results[0].indices.is_empty());
    assert!(results[1..].iter().all(|r| r.indices.is_empty()));
}

#[test]
fn matches_words_across_fields_and_preserves_unicode_titles_and_user_names() {
    let item = edit(
        session("x", "Réglages API", LocalSessionState::Active, false, 0),
        |e| {
            e.title = Some("Authentication retry".into());
            e.custom_name = Some("Réglages API".into());
            e.group_name = Some("Backend".into());
        },
    );
    let mut matcher = FuzzyMatcher::new();
    for query in [
        "régl",
        "auth retry",
        "repository auth",
        "example retry",
        "Backend",
        "régl auth",
    ] {
        assert_eq!(matcher.rank(query, &[item.clone()]).len(), 1, "{query}");
    }
    assert!(matcher.rank("repository nonexistent", &[item]).is_empty());
}

#[test]
fn suggestions_prioritize_waiting_then_unread_and_are_capped_at_three() {
    let mut state = SwitcherState::new();
    state.set_local_source(
        "agentsmon",
        vec![
            session("unread-old", "Repo", LocalSessionState::Idle, true, 5),
            session("active", "Repo", LocalSessionState::Active, false, 100),
            session("waiting-old", "Repo", LocalSessionState::Waiting, false, 1),
            session("unread-new", "Repo", LocalSessionState::Idle, true, 10),
            session("waiting-new", "Repo", LocalSessionState::Waiting, true, 2),
        ],
    );
    assert_eq!(
        state.suggested_items().iter().map(id).collect::<Vec<_>>(),
        ["waiting-new", "waiting-old", "unread-new"]
    );
    assert!(state.filtered().is_empty());
    state.focus_suggestions();
    assert_eq!(state.active_section(), Section::Suggestions);
    assert_eq!(id(state.selected().unwrap()), "unread-new");
    state.set_query("Repo");
    assert!(!state.suggested_items_visible());
    assert_eq!(state.filtered().len(), 5);
    assert!(!state
        .filtered()
        .iter()
        .any(|r| matches!(r.item, Item::AskLlm { .. })));
}

#[test]
fn unread_only_is_shown_and_disconnection_removes_all_results() {
    let mut state = SwitcherState::new();
    state.set_local_source(
        "agentsmon",
        vec![session("n", "Repo", LocalSessionState::Idle, true, 1)],
    );
    assert_eq!(state.suggested_items().len(), 1);
    state.focus_suggestions();
    state.set_local_source("agentsmon", vec![]);
    assert!(!state.suggested_items_visible());
    assert_ne!(state.active_section(), Section::Suggestions);
    state.set_query("Repo");
    assert!(state
        .filtered()
        .iter()
        .all(|r| !matches!(r.item, Item::LocalSource(_))));
}

#[test]
fn refresh_preserves_selected_identity_in_results_and_suggestions() {
    let mut state = SwitcherState::new();
    let a = session("a", "Repo", LocalSessionState::Waiting, false, 1);
    let b = session("b", "Repo", LocalSessionState::Waiting, true, 2);
    state.set_local_source("agentsmon", vec![a.clone(), b.clone()]);
    state.focus_suggestions();
    assert_eq!(id(state.selected().unwrap()), "a");
    let a = edit(a, |e| {
        e.last_activity_ms = 3;
        e.context_percent = Some(50);
    });
    state.set_local_source("agentsmon", vec![a.clone(), b.clone()]);
    assert_eq!(id(state.selected().unwrap()), "a");
    state.set_query("Repo");
    state.set_selected(1);
    assert_eq!(id(state.selected().unwrap()), "b");
    let b = edit(b, |e| e.unread = false);
    state.set_local_source("agentsmon", vec![b.clone(), a]);
    assert_eq!(id(state.selected().unwrap()), "b");
    state.set_local_source("agentsmon", vec![]);
    assert!(state.selected().is_none());
}
