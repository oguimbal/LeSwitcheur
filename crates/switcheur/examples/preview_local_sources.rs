//! Visual QA with synthetic data. Does not register hotkeys or activate real sessions.
use gpui::{px, size, AppContext, Bounds, Focusable, WindowBounds, WindowOptions};
use std::sync::Arc;
use switcheur_core::{
    AudioRowRef, Item, LocalSessionState, LocalSourceRef, PlaybackState, WindowRef,
};
use switcheur_ui::{SwitcherView, SwitcherViewEvent, Theme};

fn main() {
    let light = std::env::args().any(|arg| arg == "--light")
        || std::env::var_os("LESWITCHEUR_PREVIEW_LIGHT").is_some();
    gpui_platform::application()
        .with_assets(switcheur_ui::Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            switcheur_ui::theme_adapter::apply(
                &if light { Theme::light() } else { Theme::dark() },
                cx,
            );
            cx.bind_keys([
                gpui::KeyBinding::new("escape", switcheur_ui::actions::Dismiss, Some("Switcher")),
                gpui::KeyBinding::new("up", switcheur_ui::actions::SelectPrev, Some("Switcher")),
                gpui::KeyBinding::new("down", switcheur_ui::actions::SelectNext, Some("Switcher")),
                gpui::KeyBinding::new("up", switcheur_ui::actions::SelectPrev, Some("Input")),
                gpui::KeyBinding::new("down", switcheur_ui::actions::SelectNext, Some("Input")),
            ]);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        size(px(640.0), px(620.0)),
                        cx,
                    ))),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("Local source preview".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let input = cx.new(|cx| {
                        gpui_component::input::InputState::new(window, cx)
                            .placeholder("Search test sessions")
                    });
                    let view = cx.new(|cx| {
                        let mut view = SwitcherView::new(input.clone(), cx);
                        view.set_theme(if light { Theme::light() } else { Theme::dark() }, cx);
                        view.set_browser_tabs_integration(false, cx);
                        view.set_ask_llm_enabled(false, cx);
                        view.set_items(
                            vec![Item::Window(Arc::new(WindowRef {
                                id: 1,
                                pid: 1,
                                title: "Preview window".into(),
                                app_name: "Example".into(),
                                bundle_id: None,
                                icon_path: None,
                                minimized: false,
                            }))],
                            cx,
                        );
                        let envelope = local_source_ipc::Envelope::decode(include_bytes!(
                            "../../local-source-ipc/fixtures/agentsmon-v1.json"
                        ))
                        .unwrap();
                        let snapshot: local_source_ipc::Snapshot =
                            serde_json::from_value(envelope.payload).unwrap();
                        let variants = [
                            (
                                "API authentication",
                                "Review token validation",
                                LocalSessionState::Waiting,
                                true,
                                Some(42),
                                "Claude",
                                0xcc785c,
                            ),
                            (
                                "Switcher sessions",
                                "Implement local IPC",
                                LocalSessionState::Waiting,
                                false,
                                Some(78),
                                "Codex",
                                0x10a37f,
                            ),
                            (
                                "Documentation",
                                "Describe protocol compatibility",
                                LocalSessionState::Idle,
                                true,
                                Some(0),
                                "Vibe",
                                0xff7000,
                            ),
                            (
                                "Background task",
                                "Add contract tests",
                                LocalSessionState::Active,
                                false,
                                None,
                                "Codex",
                                0x10a37f,
                            ),
                        ];
                        let rows = variants
                            .into_iter()
                            .enumerate()
                            .map(
                                |(
                                    index,
                                    (label, title, state, unread, context, provider, accent),
                                )| {
                                    let mut entry = snapshot.entries[0].clone();
                                    entry.id = format!("test-{index}");
                                    entry.label = label.into();
                                    entry.custom_name = Some(label.into());
                                    entry.title = Some(title.into());
                                    entry.state = state;
                                    entry.unread = unread;
                                    entry.context_percent = context;
                                    entry.provider_name = provider.into();
                                    entry.accent_rgb = Some(accent);
                                    let mut source = LocalSourceRef::new(
                                        "agentsmon".into(),
                                        "preview".into(),
                                        entry,
                                    );
                                    source.source_name = "AgentsMon".into();
                                    Item::LocalSource(Arc::new(source))
                                },
                            )
                            .collect();
                        view.set_local_source("agentsmon", rows, cx);
                        view.set_currently_playing(
                            vec![Item::CurrentlyPlaying(Arc::new(AudioRowRef {
                                pid: 1,
                                app_name: "Music".into(),
                                bundle_id: None,
                                icon_path: None,
                                state: PlaybackState::Playing,
                                browser: None,
                                browser_tab: None,
                                track_title: Some("Example track".into()),
                                track_artist: Some("Example artist".into()),
                            }))],
                            cx,
                        );
                        view
                    });
                    cx.subscribe(&view, |_, event, cx| {
                        if matches!(event, SwitcherViewEvent::Dismissed) {
                            cx.quit();
                        }
                    })
                    .detach();
                    input.read(cx).focus_handle(cx).focus(window, cx);
                    cx.new(|cx| gpui_component::Root::new(view, window, cx))
                },
            )
            .unwrap();
            cx.activate(true);
        });
}
