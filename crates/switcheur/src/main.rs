//! LeSwitcheur entry point.
//!
//! Boot: tracing → config → Accessibility check → GPUI app. Inside the app,
//! we register the global hotkey and poll for presses from a foreground async
//! loop. Each press opens a fresh switcher window; confirm/dismiss removes it.
//!
//! NOTE: Building this binary requires a full Xcode install (not just CLT), as
//! gpui_macos invokes the `metal` shader compiler in its build script.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use gpui::{
    point, px, size, AnyWindowHandle, App, AppContext, AsyncApp, Bounds, Entity, KeyBinding,
    Pixels, Subscription, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind,
    WindowOptions,
};
use switcheur_core::{
    sort_items, AppMatchSet, Appearance, Config, ExclusionFilter, Item, RecencyTracker, SortOrder,
};
use switcheur_platform::{
    default_platform, ensure_accessibility, has_screen_recording_permission,
    prompt_input_monitoring, register_hotkey, request_accessibility_prompt,
    request_screen_recording_permission, startup, BrowserTabSource, ExclusionCell,
    FocusedAppCell, HotkeyEvent, LlmLauncher, MacHotkeyService, MacPlatform, ProgramSource,
    QuickTypeError, QuickTypeEvent, QuickTypeService, RecencyService, ScrollDir,
    SystemSwitcherError, SystemSwitcherEvent, SystemSwitcherService, WindowSource,
};
use switcheur_ui::{
    onboarding_view::{OnboardingView, OnboardingViewEvent},
    settings_view::{SettingsView, SettingsViewEvent},
    switcher_view::{NagPhase, SwitcherView, SwitcherViewEvent, UpdateBannerState},
    thanks_view::{ThanksState, ThanksView, ThanksViewEvent},
    Theme,
};
use tracing_subscriber::EnvFilter;

const WIDTH: f32 = 640.0;
const HEIGHT: f32 = 400.0;
const SETTINGS_WIDTH: f32 = 640.0;
const SETTINGS_HEIGHT: f32 = 560.0;
const ONBOARDING_WIDTH: f32 = 520.0;
const ONBOARDING_HEIGHT: f32 = 440.0;
const THANKS_WIDTH: f32 = 420.0;
const THANKS_HEIGHT: f32 = 380.0;

/// How many switcher opens between nag cards when unlicensed. The counter
/// advances on every opening of the panel; when it hits this threshold the
/// next open renders the in-panel support card instead of the result list.
const NAG_EVERY_N_USES: u32 = 50;
/// Public-facing site origin. Used as a base for API endpoints.
const LICENSE_SITE: &str = "https://leswitcheur.app";
/// Buy-a-licence page. Opens in the user's browser when they click "Buy a
/// licence" from the nag card or settings. The page's CTA hits `/checkout`
/// which redirects to Stripe; post-purchase the success page redirects back
/// to the app via `leswitcheur://activate?key=...`.
const LICENSE_BUY_URL: &str = "https://leswitcheur.app/activate";
/// Activation endpoint. POST { key, machine_id } → { token, key } or error.
const LICENSE_API_ACTIVATE: &str = "https://leswitcheur.app/api/activate";
/// Custom URL scheme the post-purchase success page redirects to.
const LICENSE_URL_SCHEME: &str = "leswitcheur";

/// Polling interval for the installed-app drift watcher. We read
/// `/Applications/LeSwitcheur.app/Contents/Info.plist` and compare
/// `CFBundleShortVersionString` to our own build. If the installed copy is
/// newer, the user has dropped in an updated `.app` while we were running —
/// we quit so their next launch starts a fresh instance of the new binary.
const UPDATE_DRIFT_POLL: Duration = Duration::from_secs(60);
/// Re-run the remote update check this often. Paired with the one-shot check
/// at startup so long-lived LSUIElement processes still notice new releases.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60 * 24);

/// Delay between a Cmd+Tab press and the switcher panel actually appearing.
/// A fast Cmd+Tab+release within this window activates the MRU window
/// silently, mirroring how macOS's native switcher behaves — the panel only
/// materialises if the user holds Cmd long enough to signal they want the
/// picker. 180 ms is short enough that holding the keys feels immediate but
/// long enough to swallow typical quick taps (~60–100 ms).
const CMD_TAB_GRACE_PERIOD: Duration = Duration::from_millis(180);

fn main() -> Result<()> {
    // `gpui::window` logs benign "window not found" ERRORs when macOS delivers
    // focus/frame callbacks to a window we've already closed (e.g. after
    // Confirm raises the target app). The race is internal to GPUI and can't
    // be resolved from user code, so downgrade that target to `warn` unless
    // the user explicitly overrides via RUST_LOG.
    let default_filter = EnvFilter::new("info,gpui::window=warn");
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or(default_filter))
        .init();

    switcheur_i18n::init();

    let args: Vec<String> = std::env::args().collect();
    let forced_open = args.iter().any(|a| a == "--open");
    let launched_at_login = args.iter().any(|a| a == startup::LAUNCHED_AT_LOGIN_ARG);
    // Developer-only: fake the post-activation popup without a backend
    // round-trip. `--thanks-debug` → success card; `--thanks-debug-error` →
    // error card with the token-rejected message.
    let thanks_debug: Option<ThanksState> = if args.iter().any(|a| a == "--thanks-debug") {
        Some(ThanksState::Success {
            key: "LSWT-DEMO-CAFE-1234".into(),
        })
    } else if args.iter().any(|a| a == "--thanks-debug-error") {
        Some(ThanksState::Error {
            key: "LSWT-DEMO-CAFE-1234".into(),
            message_i18n: "license.error_token".into(),
        })
    } else {
        None
    };

    let mut config = Config::load_or_default();
    let show_onboarding = !config.onboarding_completed;
    // Manual cold launches (Finder, `open -a`, dev runs) open the switcher
    // immediately — same affordance as pressing the hotkey. Two exceptions:
    // auto-launched at login (plist carries `LAUNCHED_AT_LOGIN_ARG`), where we
    // stay headless like any other login agent; and first-run onboarding,
    // which takes the screen instead.
    let open_on_start = forced_open || (!show_onboarding && !launched_at_login);
    tracing::info!(
        ?config,
        show_onboarding,
        forced_open,
        launched_at_login,
        open_on_start,
        "loaded config"
    );

    // Existing users who toggled launch-at-startup before the plist gained
    // `LAUNCHED_AT_LOGIN_ARG` would otherwise keep re-entering the "manual
    // cold launch" branch after every boot. Rewriting the plist is cheap and
    // also self-heals a moved .app bundle.
    if config.launch_at_startup {
        if let Err(e) = startup::enable() {
            tracing::warn!("refresh launch-at-startup plist: {e:#}");
        }
    }

    let (initial_filter, filter_errs) = ExclusionFilter::compile(&config.exclusions);
    for (idx, err) in &filter_errs {
        tracing::warn!("exclusion rule #{idx} has invalid regex: {err}");
    }

    // Cells shared across main thread, hotkey loop, and HID tap threads.
    // `ArcSwap` gives the hot paths lock-free reads; settings edits swap a
    // fresh `AppMatchSet` and the next event sees it.
    let focused: FocusedAppCell = Arc::new(ArcSwap::from_pointee(None));
    let hotkey_excluded: ExclusionCell = Arc::new(ArcSwap::from_pointee(
        AppMatchSet::compile(&config.hotkey_excluded_apps),
    ));
    let quick_type_excluded: ExclusionCell = Arc::new(ArcSwap::from_pointee(
        AppMatchSet::compile(&config.quick_type_excluded_apps),
    ));

    // Try to start Quick Type if the user had it enabled; clear the flag in
    // memory (not on disk) if we don't have Input Monitoring yet so the user
    // can retry from settings without losing their intent.
    let (quick_type_service, quick_type_rx) = match config.quick_type {
        true => match QuickTypeService::start(focused.clone(), quick_type_excluded.clone()) {
            Ok(svc) => {
                let rx = svc.receiver();
                (Some(svc), Some(rx))
            }
            Err(e) => {
                tracing::warn!("quick_type disabled: {e}");
                config.quick_type = false;
                (None, None)
            }
        },
        false => (None, None),
    };

    // Same pattern for the Cmd+Tab replacement tap.
    let (system_switcher_service, system_switcher_rx) = match config.replace_system_switcher {
        true => match SystemSwitcherService::start() {
            Ok(svc) => {
                let rx = svc.receiver();
                (Some(svc), Some(rx))
            }
            Err(e) => {
                tracing::warn!("replace_system_switcher disabled: {e}");
                config.replace_system_switcher = false;
                (None, None)
            }
        },
        false => (None, None),
    };

    // Accessibility is requested from the onboarding wizard (first step) so
    // users see an explanation before the system prompt, and existing users
    // whose config already has `onboarding_completed = true` keep their
    // previously granted permission untouched.
    if !ensure_accessibility(false) {
        tracing::info!(
            "Accessibility permission not yet granted — onboarding or settings will prompt."
        );
    }

    let platform: Arc<MacPlatform> =
        Arc::new(default_platform().context("init macOS platform")?);
    let hotkey = Arc::new(register_hotkey(&config.hotkey).context("register hotkey")?);

    let app = gpui_platform::application().with_assets(switcheur_ui::Assets);
    // Re-launch via Finder / `open -a LeSwitcheur` lands here on the
    // already-running process. Synthesize a hotkey press so the switcher
    // pops up just like Ctrl+=.
    {
        let hotkey = hotkey.clone();
        app.on_reopen(move |_cx| {
            tracing::info!("reopen: triggering switcher");
            hotkey.trigger();
        });
    }

    // URL-scheme handler: post-purchase the success page redirects to
    // `leswitcheur://activate?key=...`, which lands here (potentially on a
    // cold launch). Buffer URLs onto an async channel drained by a task
    // spawned inside `app.run` so GPUI state is available when we act.
    let (url_tx, url_rx) = async_channel::unbounded::<String>();
    app.on_open_urls(move |urls| {
        for u in urls {
            tracing::info!(url = %u, "open_urls");
            let _ = url_tx.send_blocking(u);
        }
    });
    app.run(move |cx| {
        install_key_bindings(cx);

        // Recency tracker + observers. App-level observer is always on; the
        // window-level observer is started lazily if the user picked
        // `SortOrder::RecentWindow`.
        let tracker = Arc::new(Mutex::new(RecencyTracker::new()));
        let mut recency = RecencyService::start(tracker.clone(), focused.clone());
        if matches!(config.sort_order, SortOrder::RecentWindow) {
            if let Ok(apps) = platform.list_apps() {
                let pids: Vec<_> = apps.iter().map(|a| a.pid).collect();
                recency.enable_window_tracking(&pids);
            }
        }


        let licensed_now = match config.license_token.as_deref() {
            Some(tok) => match switcheur_core::license::verify_embedded(tok) {
                Ok(t) => {
                    tracing::info!(key = %t.key, "license valid");
                    true
                }
                Err(e) => {
                    tracing::warn!("license token rejected: {e:#}");
                    false
                }
            },
            None => false,
        };

        let state = AppState {
            platform,
            hotkey,
            config: Rc::new(RefCell::new(config)),
            filter: Rc::new(RefCell::new(initial_filter)),
            quick_type: Rc::new(RefCell::new(quick_type_service)),
            system_switcher: Rc::new(RefCell::new(system_switcher_service)),
            pending_cycle: Rc::new(RefCell::new(None)),
            system_switcher_epoch: Rc::new(Cell::new(0)),
            tracker,
            recency: Rc::new(RefCell::new(recency)),
            current: Rc::new(RefCell::new(None)),
            settings: Rc::new(RefCell::new(None)),
            onboarding: Rc::new(RefCell::new(None)),
            thanks: Rc::new(RefCell::new(None)),
            licensed: Rc::new(Cell::new(licensed_now)),
            nag_shown_this_session: Rc::new(Cell::new(false)),
            pending_update: Rc::new(RefCell::new(None)),
            update_stage: Rc::new(RefCell::new(UpdateBannerState::Hidden)),
            update_dismissed_this_session: Rc::new(Cell::new(false)),
            focused,
            hotkey_excluded,
            quick_type_excluded,
            // Resolve once at boot. macOS GUI apps don't see the user's
            // shell PATH unless we walk it explicitly, so the detector
            // probes both PATH and well-known absolute install locations.
            zoxide_bin: Rc::new(RefCell::new(switcheur_platform::zoxide::detect())),
        };
        if open_on_start {
            tracing::info!("cold launch: triggering switcher on startup");
            state.hotkey.trigger();
        }
        if show_onboarding {
            if let Err(e) = open_onboarding_window(&state, cx) {
                tracing::warn!("open_onboarding_window: {e:#}");
            }
        }
        if let Some(t) = thanks_debug.clone() {
            if let Err(e) = open_thanks_window(&state, t, cx) {
                tracing::warn!("open_thanks_window (debug): {e:#}");
            }
        }
        if let Some(rx) = quick_type_rx {
            spawn_quick_type_loop(cx, state.clone(), rx);
        }
        if let Some(rx) = system_switcher_rx {
            spawn_system_switcher_loop(cx, state.clone(), rx);
        }
        spawn_hotkey_loop(cx, state.clone());
        spawn_update_checker(cx, state.clone());
        spawn_url_scheme_loop(cx, state.clone(), url_rx);
        spawn_install_drift_watcher(cx, state);
    });

    Ok(())
}

#[derive(Clone)]
struct AppState {
    platform: Arc<MacPlatform>,
    hotkey: Arc<MacHotkeyService>,
    config: Rc<RefCell<Config>>,
    filter: Rc<RefCell<ExclusionFilter>>,
    quick_type: Rc<RefCell<Option<QuickTypeService>>>,
    system_switcher: Rc<RefCell<Option<SystemSwitcherService>>>,
    /// If present, a Cmd+Tab cycle is in its grace period: we've snapshotted
    /// the items and are advancing a selection internally but haven't shown
    /// the panel yet. If Cmd is released before the timer fires we activate
    /// directly (no flash); otherwise the timer promotes this to a real panel.
    pending_cycle: Rc<RefCell<Option<PendingCycle>>>,
    /// Monotonic generation counter bumped each time a pending cycle starts.
    /// The armed timer captures its value at spawn and only acts if it still
    /// matches — prevents a stale timer from opening a panel for a cycle that
    /// already ended (Confirm → silent activation) and got re-opened since.
    system_switcher_epoch: Rc<Cell<u64>>,
    tracker: Arc<Mutex<RecencyTracker>>,
    recency: Rc<RefCell<RecencyService>>,
    current: Rc<RefCell<Option<SwitcherSlot>>>,
    settings: Rc<RefCell<Option<WindowSlot>>>,
    onboarding: Rc<RefCell<Option<WindowSlot>>>,
    /// Post-activation confirmation popup (thank-you card on success, error
    /// card on failure). At most one at a time.
    thanks: Rc<RefCell<Option<WindowSlot>>>,
    licensed: Rc<Cell<bool>>,
    /// Set to true after the first unlicensed switcher open of this process.
    /// Drives the "nag on first open each launch" behaviour; reset implicitly
    /// on every app restart since the cell lives only in memory.
    nag_shown_this_session: Rc<Cell<bool>>,
    /// Latest positive result from the update checker (or None if up to date
    /// / check failed). Read when opening the switcher to decide whether to
    /// show the update banner. Not persisted — re-evaluated every launch.
    pending_update: Rc<RefCell<Option<UpdateInfo>>>,
    /// Current banner stage. Driven by the update checker (→ Available), the
    /// download handler (→ Downloading, then Ready), and the × dismiss (→
    /// Hidden via `update_dismissed_this_session`). Read when opening a
    /// fresh switcher window so the banner carries across sessions.
    update_stage: Rc<RefCell<UpdateBannerState>>,
    /// Set by the banner's × dismiss. Suppresses the banner until next app
    /// restart; matches the nag-card `nag_shown_this_session` pattern.
    update_dismissed_this_session: Rc<Cell<bool>>,
    /// Shared snapshot of the frontmost (non-self) app, updated by the
    /// NSWorkspace activation observer. Read from the hotkey loop and HID taps.
    focused: FocusedAppCell,
    /// Apps where the popup hotkey is silently ignored.
    hotkey_excluded: ExclusionCell,
    /// Apps where Quick Type passes through instead of intercepting.
    quick_type_excluded: ExclusionCell,
    /// Path to the user's `zoxide` binary, resolved once at boot. `None` when
    /// zoxide isn't installed — the right-side dirs panel stays hidden then.
    zoxide_bin: Rc<RefCell<Option<std::path::PathBuf>>>,
}

/// In-flight Cmd+Tab cycle whose panel hasn't been shown yet. Lives entirely
/// in memory on the main thread; no window exists until the grace timer
/// promotes it (or it's thrown away on an early Confirm).
struct PendingCycle {
    /// MRU-ordered items snapshotted when the cycle opened. Reused verbatim
    /// if we later promote to a real panel so item identity (and pre-ranking)
    /// stays stable across the transition.
    items: Vec<Item>,
    /// Current selection within `items`. Starts at 1 (or len-1 for reverse)
    /// and wraps with each subsequent Tab.
    selected: usize,
    /// Generation this cycle was tagged with at `Open` time. The armed timer
    /// captured the same value and checks it before promoting — guards
    /// against a stale timer firing after a new cycle has replaced this one.
    generation: u64,
}

/// A live window handle + its event subscription. Dropping a slot unregisters
/// the subscription — important so callbacks stop firing once the window is
/// removed (otherwise GPUI spams `window not found` each frame).
struct WindowSlot {
    handle: AnyWindowHandle,
    _sub: Subscription,
}

/// The switcher slot carries the typed entity in addition to the handle so
/// Quick Type can route intercepted keystrokes straight into the view.
struct SwitcherSlot {
    handle: AnyWindowHandle,
    entity: Entity<SwitcherView>,
    _sub: Subscription,
}

fn install_key_bindings(cx: &mut App) {
    use switcheur_ui::actions::{
        Backspace, Confirm, Copy, Cut, Delete, Dismiss, ExtendEnd, ExtendHome, ExtendLeft,
        ExtendRight, ExtendWordLeft, ExtendWordRight, FocusNextPane, FocusPrevPane, MoveEnd,
        MoveHome, MoveLeft, MoveRight, MoveWordLeft, MoveWordRight, Paste, SelectAll, SelectNext,
        SelectPrev,
    };
    cx.bind_keys([
        // List navigation (up/down + ctrl-p/ctrl-n, ala Zed/Emacs).
        KeyBinding::new("up", SelectPrev, Some("Switcher")),
        KeyBinding::new("down", SelectNext, Some("Switcher")),
        KeyBinding::new("ctrl-p", SelectPrev, Some("Switcher")),
        KeyBinding::new("ctrl-n", SelectNext, Some("Switcher")),
        // Confirm / dismiss.
        KeyBinding::new("enter", Confirm, Some("Switcher")),
        KeyBinding::new("escape", Dismiss, Some("Switcher")),
        // Text editing.
        KeyBinding::new("backspace", Backspace, Some("Switcher")),
        KeyBinding::new("delete", Delete, Some("Switcher")),
        // Caret motion.
        KeyBinding::new("left", MoveLeft, Some("Switcher")),
        KeyBinding::new("right", MoveRight, Some("Switcher")),
        KeyBinding::new("home", MoveHome, Some("Switcher")),
        KeyBinding::new("end", MoveEnd, Some("Switcher")),
        KeyBinding::new("cmd-left", MoveHome, Some("Switcher")),
        KeyBinding::new("cmd-right", MoveEnd, Some("Switcher")),
        KeyBinding::new("ctrl-a", MoveHome, Some("Switcher")),
        KeyBinding::new("ctrl-e", MoveEnd, Some("Switcher")),
        // Extending the selection.
        KeyBinding::new("shift-left", ExtendLeft, Some("Switcher")),
        KeyBinding::new("shift-right", ExtendRight, Some("Switcher")),
        KeyBinding::new("shift-home", ExtendHome, Some("Switcher")),
        KeyBinding::new("shift-end", ExtendEnd, Some("Switcher")),
        KeyBinding::new("cmd-shift-left", ExtendHome, Some("Switcher")),
        KeyBinding::new("cmd-shift-right", ExtendEnd, Some("Switcher")),
        // Word motion (Option on macOS) + selection.
        KeyBinding::new("alt-left", MoveWordLeft, Some("Switcher")),
        KeyBinding::new("alt-right", MoveWordRight, Some("Switcher")),
        KeyBinding::new("alt-shift-left", ExtendWordLeft, Some("Switcher")),
        KeyBinding::new("alt-shift-right", ExtendWordRight, Some("Switcher")),
        KeyBinding::new("ctrl-shift-left", ExtendWordLeft, Some("Switcher")),
        KeyBinding::new("ctrl-shift-right", ExtendWordRight, Some("Switcher")),
        KeyBinding::new("cmd-a", SelectAll, Some("Switcher")),
        // Clipboard.
        KeyBinding::new("cmd-c", Copy, Some("Switcher")),
        KeyBinding::new("cmd-x", Cut, Some("Switcher")),
        KeyBinding::new("cmd-v", Paste, Some("Switcher")),
        // Cycle keyboard focus to/from the right-side dirs pane.
        KeyBinding::new("tab", FocusNextPane, Some("Switcher")),
        KeyBinding::new("shift-tab", FocusPrevPane, Some("Switcher")),
    ]);
}

fn spawn_hotkey_loop(cx: &mut App, state: AppState) {
    let rx = state.hotkey.receiver();
    cx.spawn(async move |cx: &mut AsyncApp| {
        // `.recv().await` parks the task until a hotkey event lands in the
        // async channel — zero CPU when idle, no timer wakeups.
        while let Ok(HotkeyEvent::Pressed) = rx.recv().await {
            if hotkey_suppressed(&state) {
                tracing::debug!("hotkey press suppressed: frontmost app is excluded");
                continue;
            }
            close_current(cx, &state);
            if let Err(e) = open_switcher(cx, &state) {
                tracing::warn!("open_switcher: {e:#}");
            }
        }
    })
    .detach();
}

fn hotkey_suppressed(state: &AppState) -> bool {
    let excl = state.hotkey_excluded.load();
    if excl.is_empty() {
        return false;
    }
    let snap = state.focused.load();
    let Some(app) = snap.as_ref().as_ref() else {
        return false;
    };
    excl.any_match(&app.name, app.bundle_id.as_deref())
}

/// Drive the switcher from Quick Type events. The task exits cleanly when the
/// service is stopped (the channel closes).
fn spawn_quick_type_loop(
    cx: &mut App,
    state: AppState,
    rx: async_channel::Receiver<QuickTypeEvent>,
) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Ok(ev) = rx.recv().await {
            apply_quick_type_event(cx, &state, ev);
        }
    })
    .detach();
}

fn apply_quick_type_event(cx: &mut AsyncApp, state: &AppState, ev: QuickTypeEvent) {
    // FnReleased never opens the switcher: it only confirms an already-open
    // selection, and only when the user actually scrolled during the hold.
    if let QuickTypeEvent::FnReleased { scrolled } = ev {
        if !scrolled {
            return;
        }
        let entity = match state.current.borrow().as_ref() {
            Some(slot) => slot.entity.clone(),
            None => return,
        };
        let _ = cx.update(|cx| {
            entity.update(cx, |view, cx| view.confirm_external(cx));
        });
        return;
    }

    // Open the switcher on first keystroke or first scroll tick, ignoring a
    // stray Backspace before anything else has opened it.
    if state.current.borrow().is_none() {
        if matches!(ev, QuickTypeEvent::Backspace) {
            return;
        }
        if let Err(e) = open_switcher(cx, state) {
            tracing::warn!("quick_type open_switcher: {e:#}");
            return;
        }
    }

    let entity = match state.current.borrow().as_ref() {
        Some(slot) => slot.entity.clone(),
        None => return,
    };

    let _ = cx.update(|cx| match ev {
        QuickTypeEvent::InsertText(s) => {
            entity.update(cx, |view, cx| view.append_query(&s, cx))
        }
        QuickTypeEvent::Backspace => entity.update(cx, |view, cx| view.backspace_query(cx)),
        QuickTypeEvent::Scroll(dir) => entity.update(cx, |view, cx| match dir {
            ScrollDir::Up => view.select_prev_external(cx),
            ScrollDir::Down => view.select_next_external(cx),
        }),
        QuickTypeEvent::FnReleased { .. } => {}
    });
}

/// Drive the switcher from Cmd+Tab interception events.
fn spawn_system_switcher_loop(
    cx: &mut App,
    state: AppState,
    rx: async_channel::Receiver<SystemSwitcherEvent>,
) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Ok(ev) = rx.recv().await {
            apply_system_switcher_event(cx, &state, ev);
        }
    })
    .detach();
}

fn apply_system_switcher_event(cx: &mut AsyncApp, state: &AppState, ev: SystemSwitcherEvent) {
    tracing::info!(?ev, "apply_system_switcher_event");
    match ev {
        SystemSwitcherEvent::Open { reverse } => {
            if state.current.borrow().is_some() {
                // Panel already visible — shouldn't normally happen on Open,
                // but treat defensively as an extra Cycle step.
                advance_visible_panel(cx, state, reverse);
                return;
            }
            // Already in a pending cycle (e.g. the HID tap re-fired Open for
            // some reason): just advance the selection and keep going.
            if advance_pending_cycle(state, reverse) {
                return;
            }
            // Fresh cycle: snapshot items, arm the grace timer. Panel stays
            // invisible unless the timer fires before Cmd release.
            let items = {
                let cfg = state.config.borrow();
                let tracker = state.tracker.lock().unwrap();
                collect_items(&state.platform, &cfg, &state.filter.borrow(), &tracker)
            };
            if items.is_empty() {
                return;
            }
            let generation = state.system_switcher_epoch.get().wrapping_add(1);
            state.system_switcher_epoch.set(generation);
            let selected = step_index(0, items.len(), reverse);
            *state.pending_cycle.borrow_mut() = Some(PendingCycle {
                items,
                selected,
                generation,
            });
            spawn_grace_promotion(cx, state.clone(), generation);
        }
        SystemSwitcherEvent::Cycle { reverse } => {
            if advance_pending_cycle(state, reverse) {
                return;
            }
            advance_visible_panel(cx, state, reverse);
        }
        SystemSwitcherEvent::Confirm => {
            // If the grace timer never fired, this is a quick tap — activate
            // the pre-selected MRU item directly, skip the panel entirely.
            let pending = state.pending_cycle.borrow_mut().take();
            if let Some(pc) = pending {
                confirm_pending_cycle(state, pc);
                reset_system_switcher_cycle(state);
                return;
            }
            // Panel was shown: route through the normal confirm path.
            let entity = match state.current.borrow().as_ref() {
                Some(slot) => slot.entity.clone(),
                None => return,
            };
            let _ = cx.update(|cx| {
                entity.update(cx, |view, cx| view.confirm_external(cx));
            });
        }
        SystemSwitcherEvent::TypeText(s) => {
            // Typing mid-cycle means the user wants the picker — promote any
            // pending cycle to a visible panel first, then append the text.
            if state.pending_cycle.borrow().is_some() {
                if let Err(e) = promote_pending_to_panel(cx, state) {
                    tracing::warn!("promote pending_cycle on type: {e:#}");
                    return;
                }
            }
            if state.current.borrow().is_none() {
                if let Err(e) = open_switcher(cx, state) {
                    tracing::warn!("system_switcher open-on-type: {e:#}");
                    return;
                }
            }
            let (entity, handle) = match state.current.borrow().as_ref() {
                Some(slot) => (slot.entity.clone(), slot.handle),
                None => return,
            };
            let _ = cx.update(|cx: &mut App| {
                let _ = cx.update_window(handle, |_any, window, app_cx| {
                    entity.update(app_cx, |view, view_cx| {
                        view.append_query(&s, view_cx);
                        view.dismiss_on_blur(window, view_cx);
                    });
                });
            });
        }
    }
}

/// Advance the selection on the already-visible switcher panel. Extracted so
/// both `Open` (defensive branch) and `Cycle` can share the code path.
fn advance_visible_panel(cx: &mut AsyncApp, state: &AppState, reverse: bool) {
    let entity = match state.current.borrow().as_ref() {
        Some(slot) => slot.entity.clone(),
        None => return,
    };
    let _ = cx.update(|cx| {
        entity.update(cx, |view, cx| {
            if reverse {
                view.select_prev_external(cx);
            } else {
                view.select_next_external(cx);
            }
        });
    });
}

/// Step the pending (invisible) cycle by one if it exists. Returns true if
/// we absorbed the event, false if there was no pending cycle to advance.
fn advance_pending_cycle(state: &AppState, reverse: bool) -> bool {
    let mut guard = state.pending_cycle.borrow_mut();
    let Some(pc) = guard.as_mut() else {
        return false;
    };
    pc.selected = step_index(pc.selected, pc.items.len(), reverse);
    true
}

fn step_index(current: usize, len: usize, reverse: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if reverse {
        if current == 0 {
            len - 1
        } else {
            current - 1
        }
    } else {
        (current + 1) % len
    }
}

/// Confirm a grace-period cycle without ever showing the panel: bump recency
/// and activate the pre-selected item directly.
fn confirm_pending_cycle(state: &AppState, pc: PendingCycle) {
    let Some(item) = pc.items.get(pc.selected).cloned() else {
        return;
    };
    if let Ok(mut t) = state.tracker.lock() {
        match &item {
            Item::Window(w) => t.note_window(w.pid, &w.title),
            Item::App(a) => t.note_app(a.pid),
            Item::Program(_)
            | Item::AskLlm { .. }
            | Item::OpenUrl(_)
            | Item::Dir(_)
            | Item::BrowserTab(_) => {
                /* Cmd+Tab cycles only ever carry Window/App items */
            }
        }
    }
    let res = match &item {
        Item::Window(w) => state.platform.activate_window(w),
        Item::App(a) => state.platform.activate_app(a),
        Item::Program(p) => state.platform.launch_program(p),
        Item::AskLlm { .. } | Item::OpenUrl(_) | Item::Dir(_) | Item::BrowserTab(_) => {
            tracing::warn!("unexpected non-window/app item in Cmd+Tab cycle");
            Ok(())
        }
    };
    if let Err(e) = res {
        tracing::warn!("pending_cycle activate: {e:#}");
    }
}

fn spawn_grace_promotion(cx: &mut AsyncApp, state: AppState, generation: u64) {
    let executor = cx.background_executor().clone();
    cx.spawn(async move |cx: &mut AsyncApp| {
        executor.timer(CMD_TAB_GRACE_PERIOD).await;
        // Bail out if the cycle was already consumed (Confirm → silent
        // activation) or replaced by a newer Open. Only a matching generation
        // means "still the same cycle that armed this timer".
        let still_pending = state
            .pending_cycle
            .borrow()
            .as_ref()
            .is_some_and(|pc| pc.generation == generation);
        if !still_pending {
            return;
        }
        if let Err(e) = promote_pending_to_panel(cx, &state) {
            tracing::warn!("promote pending_cycle on timer: {e:#}");
        }
    })
    .detach();
}

/// Open the switcher panel using the pending cycle's snapshot and pre-advance
/// the selection to where it would have been. Clears `pending_cycle`.
fn promote_pending_to_panel(cx: &mut AsyncApp, state: &AppState) -> Result<()> {
    let Some(pc) = state.pending_cycle.borrow_mut().take() else {
        return Ok(());
    };
    open_switcher_with_items(cx, state, pc.items, pc.selected, false)?;
    Ok(())
}

fn close_current(cx: &mut AsyncApp, state: &AppState) {
    let slot = state.current.borrow_mut().take();
    if let Some(slot) = slot {
        let _ = cx.update(|cx| {
            let _ = cx.update_window(slot.handle, |_ent, window, _cx| {
                window.remove_window();
            });
        });
    }
    reset_system_switcher_cycle(state);
}

fn reset_system_switcher_cycle(state: &AppState) {
    // Drop any invisible cycle too — a visible panel dismissal means the
    // user is done; the grace timer (if still armed) will find `None` and
    // no-op on fire.
    *state.pending_cycle.borrow_mut() = None;
    if let Some(svc) = state.system_switcher.borrow().as_ref() {
        svc.reset_cycle();
    }
}

fn open_switcher(cx: &mut AsyncApp, state: &AppState) -> Result<()> {
    open_switcher_with(cx, state, true)
}

fn open_switcher_with(
    cx: &mut AsyncApp,
    state: &AppState,
    dismiss_on_blur: bool,
) -> Result<()> {
    let items = {
        let cfg = state.config.borrow();
        let tracker = state.tracker.lock().unwrap();
        collect_items(&state.platform, &cfg, &state.filter.borrow(), &tracker)
    };
    open_switcher_with_items(cx, state, items, 0, dismiss_on_blur)
}

/// Open the switcher panel with a caller-supplied item list and initial
/// selection index. Used by the Cmd+Tab grace-period flow so the promoted
/// panel reuses the exact snapshot (and cursor position) that was being
/// tracked invisibly.
fn open_switcher_with_items(
    cx: &mut AsyncApp,
    state: &AppState,
    items: Vec<Item>,
    initial_selected: usize,
    dismiss_on_blur: bool,
) -> Result<()> {
    let cfg = state.config.borrow();
    let theme = theme_for(cfg.appearance);
    let search_apps = cfg.search_apps;
    drop(cfg);

    // Computed on the main thread where `primary_display` is cheap.
    let bounds = cx.update(|cx| initial_bounds(cx, WIDTH, HEIGHT));

    let options = WindowOptions {
        titlebar: None,
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        kind: WindowKind::PopUp,
        is_movable: false,
        focus: true,
        show: true,
        window_background: WindowBackgroundAppearance::Blurred,
        ..Default::default()
    };

    let show_nag = tick_nag_counter(state);
    let update_banner = current_update_banner(state);

    let entity_slot: Rc<RefCell<Option<Entity<SwitcherView>>>> = Rc::new(RefCell::new(None));
    let slot_for_builder = entity_slot.clone();
    let items_for_builder = items;
    // Snapshot the installed-program catalogue at open time. Cheap: all
    // ProgramRef's are Arc-cloned behind the scenes. The list can be empty
    // on very first open if the Spotlight prefetch hasn't finished — that's
    // a fine no-op for the Program Launcher section.
    let programs_for_builder: Vec<std::sync::Arc<switcheur_core::ProgramRef>> = if search_apps {
        state
            .platform
            .list_programs()
            .unwrap_or_else(|e| {
                tracing::warn!("list_programs: {e:#}");
                Vec::new()
            })
            .into_iter()
            .map(std::sync::Arc::new)
            .collect()
    } else {
        Vec::new()
    };
    tracing::info!(
        programs = programs_for_builder.len(),
        "opening switcher with program catalogue"
    );
    let llm_order = state.config.borrow().llm_provider_order.clone();
    let ask_llm_enabled = state.config.borrow().ask_llm_enabled;
    // Zoxide is enabled only if both the user setting is on AND the binary
    // was found at boot. The view emits QueryChanged events only when on,
    // so the host's zoxide call below stays cheap when off.
    let zoxide_enabled = state.config.borrow().zoxide_integration
        && state.zoxide_bin.borrow().is_some();
    let browser_tabs_enabled = state.config.borrow().browser_tabs_integration;
    let handle: WindowHandle<SwitcherView> = cx.open_window(options, move |window, cx| {
        let entity = cx.new(|cx| {
            let mut view = SwitcherView::new(cx);
            view.set_theme(theme, cx);
            view.set_zoxide_enabled(zoxide_enabled, cx);
            view.set_browser_tabs_integration(browser_tabs_enabled, cx);
            view.set_items(items_for_builder, cx);
            view.set_programs(programs_for_builder, cx);
            view.set_llm_provider_order(llm_order, cx);
            view.set_ask_llm_enabled(ask_llm_enabled, cx);
            if show_nag {
                view.set_nag_phase(NagPhase::Visible, cx);
            }
            if !matches!(update_banner, UpdateBannerState::Hidden) {
                view.set_update_banner(update_banner.clone(), cx);
            }
            view
        });
        *slot_for_builder.borrow_mut() = Some(entity.clone());

        // Grab keyboard focus + auto-dismiss on blur. Done outside `cx.new` so
        // the Window reference is available.
        let focus = entity.read(cx).focus_handle().clone();
        focus.focus(window, cx);
        if dismiss_on_blur {
            entity.update(cx, |view, cx| {
                view.dismiss_on_blur(window, cx);
            });
        }

        entity
    })?;

    let _ = cx.update(|cx| {
        // LSUIElement + PopUp window ⇒ our NSApp never becomes active on its
        // own, which breaks the modern yield-based activation path we use on
        // confirm: `NSRunningApplication.activateFromApplication:` returns
        // NO unless the source (us) holds activation, and on macOS 14+ that
        // is enforced — the "old" `activate(ignoringOtherApps:)` fallback
        // silently no-ops for non-active callers. Calling `cx.activate(true)`
        // here promotes us to the active app for the duration the switcher
        // is visible, so the yield on confirm is legitimate and the Space-
        // switching/cross-fullscreen path works.
        cx.activate(true);
        let _ = handle.update(cx, |_view, window, _cx| {
            window.activate_window();
        });
    });

    // Wire confirm/dismiss → platform + close.
    let entity = entity_slot.borrow().clone().expect("builder populated slot");
    let state_sub = state.clone();
    let sub = cx.subscribe(
        &entity,
        move |_entity, ev: &SwitcherViewEvent, cx: &mut App| {
            handle_view_event(ev, &state_sub, cx);
        },
    );

    *state.current.borrow_mut() = Some(SwitcherSlot {
        handle: handle.into(),
        entity: entity.clone(),
        _sub: sub,
    });

    if initial_selected != 0 {
        let _ = cx.update(|cx| {
            entity.update(cx, |view, cx| view.set_selected_external(initial_selected, cx));
        });
    }

    Ok(())
}

fn handle_view_event(ev: &SwitcherViewEvent, state: &AppState, cx: &mut App) {
    tracing::info!(?ev, "switcher view event");

    // License-card events keep the panel open — the nag card lives inside it.
    match ev {
        SwitcherViewEvent::LicenseActivateRequested => {
            // Nag's "Buy a licence" button: open the buy page in the user's
            // browser and close the in-panel nag. Post-purchase the success
            // page redirects via `leswitcheur://activate?key=...`.
            if let Err(e) = open::that(LICENSE_BUY_URL) {
                tracing::warn!("open license site: {e:#}");
            }
            let entity = state.current.borrow().as_ref().map(|s| s.entity.clone());
            if let Some(entity) = entity {
                entity.update(cx, |v, cx| v.set_nag_phase(NagPhase::Hidden, cx));
            }
            return;
        }
        SwitcherViewEvent::LicenseDismissed => {
            // View already flipped its own `nag_phase` to Hidden; the host
            // doesn't need to touch the window — the results list re-appears.
            return;
        }
        SwitcherViewEvent::UpdateDownloadRequested => {
            let entity = state.current.borrow().as_ref().map(|s| s.entity.clone());
            if let Some(entity) = entity {
                start_update_download(entity, state.clone(), cx);
            }
            return;
        }
        SwitcherViewEvent::UpdateDismissed => {
            state.update_dismissed_this_session.set(true);
            return;
        }
        SwitcherViewEvent::CloseWindowRequested(w) => {
            if let Err(e) = state.platform.close_window(w) {
                tracing::warn!("close_window: {e:#}");
            }
            // Optimistically drop the row right now — `list_windows` may still
            // see the dying window for a beat. The row reappears only if the
            // app refused to close (e.g. unsaved-changes dialog) and the next
            // open snapshots it back in.
            let entity_opt = state.current.borrow().as_ref().map(|s| s.entity.clone());
            if let Some(entity) = entity_opt {
                let id = w.id;
                let _ = cx.update_entity(&entity, |view, cx| view.drop_window(id, cx));
            }
            return;
        }
        SwitcherViewEvent::NeedsBrowserTabs => {
            // Fired once per switcher session the first time the fallback
            // tier is reached and no scan has been delivered yet. Shell out
            // to AppleScript off the UI thread so a slow / permission-
            // blocked osascript can't stall typing, then feed the result
            // back into the view (empty vec when Chrome isn't running or
            // the automation prompt was denied — UX falls through to LLM).
            let entity_opt = state.current.borrow().as_ref().map(|s| s.entity.clone());
            let Some(entity) = entity_opt else {
                return;
            };
            let platform = state.platform.clone();
            let weak = entity.downgrade();
            cx.spawn(async move |cx: &mut AsyncApp| {
                let tabs = cx
                    .background_executor()
                    .spawn(async move { platform.list_browser_tabs() })
                    .await;
                let items: Vec<Item> = tabs.into_iter().map(Item::from).collect();
                let _ = weak.update(cx, |view, cx| {
                    view.set_browser_tabs(items, cx);
                });
            })
            .detach();
            return;
        }
        SwitcherViewEvent::QueryChanged(query) => {
            // The view emits QueryChanged on every keystroke when zoxide is
            // enabled. Spawn the subprocess on the background executor so a
            // slow disk doesn't stutter the UI; cancellation is implicit
            // via a per-view generation counter inside the view itself —
            // here we just deliver the freshest result we can compute.
            let Some(bin) = state.zoxide_bin.borrow().clone() else {
                return;
            };
            let entity_opt = state.current.borrow().as_ref().map(|s| s.entity.clone());
            let Some(entity) = entity_opt else {
                return;
            };
            let query = query.clone();
            let weak = entity.downgrade();
            cx.spawn(async move |cx: &mut AsyncApp| {
                // 30 ms debounce so a fast typist doesn't fire a subprocess
                // per keystroke. Held even on empty queries to keep the
                // pane stable while the user clears the input.
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(30))
                    .await;
                let bin_for_task = bin.clone();
                let query_for_task = query.clone();
                let hits = cx
                    .background_executor()
                    .spawn(async move {
                        switcheur_platform::zoxide::query(
                            &bin_for_task,
                            &query_for_task,
                            8,
                        )
                    })
                    .await;
                // One generic folder icon shared by every row. Resolved on
                // the worker so the synchronous AppKit calls don't run on
                // the GPUI main thread; the result is OnceLock-cached
                // inside the platform crate so this is free after the
                // first call.
                let icon = switcheur_platform::macos::icons::folder_icon_path();
                let items: Vec<Item> = hits
                    .into_iter()
                    .map(|h| {
                        Item::Dir(Arc::new(switcheur_core::DirRef::new(
                            h.path,
                            switcheur_core::DirSource::Zoxide,
                            icon.clone(),
                        )))
                    })
                    .collect();
                let _ = weak.update(cx, |view, cx| {
                    // Apply only if the live query still matches what we
                    // queried for. Avoids flashing stale results past the
                    // user mid-typing.
                    if view.zoxide_enabled() {
                        view.set_dirs(items, cx);
                    }
                });
            })
            .detach();
            return;
        }
        _ => {}
    }

    // The resize event is the only one that must keep the panel open — the
    // others all close it before handing control to the target app. Handle
    // resize first and bail out so the panel-close logic below doesn't fire.
    if let SwitcherViewEvent::FrameDeltaChanged {
        delta_origin_y,
        delta_height,
    } = ev
    {
        // Deferred via foreground_executor so the NSWindow::setFrame (which
        // synchronously flushes a redraw) doesn't re-enter GPUI while we're
        // still inside the event-dispatch borrow — doing it inline produces
        // "RefCell already borrowed" errors.
        let d_origin = *delta_origin_y;
        let d_height = *delta_height;
        cx.foreground_executor()
            .spawn(async move {
                switcheur_platform::adjust_key_window_frame(d_origin, d_height);
            })
            .detach();
        return;
    }

    // Order matters here. Two opposing constraints:
    //   - If we close the panel *before* activating the target, our process
    //     loses frontmost status, and on macOS 14+ `activateFromApplication:`
    //     silently no-ops when the caller doesn't hold activation (that's
    //     the Sonoma lock-down on external activation). Cross-Space / cross-
    //     Space-fullscreen switches stop working entirely.
    //   - If we activate first and *then* close, keyboard focus can briefly
    //     linger on our dying panel. That was the prior concern.
    //
    // The new activation path uses the modern yield-based API
    // (`activateFromApplication:options:`), which transfers activation
    // atomically with the source — so as long as our panel is still alive
    // when the call is made, keyboard focus moves cleanly to the target.
    // We close the panel *after* the activation call returns.
    let slot_to_close = match ev {
        SwitcherViewEvent::Confirmed(item) => {
            // Bump recency *before* activating — the OS activation notification
            // will bump it again, but we want the item ranked immediately even
            // if the OS notif is delayed or coalesced.
            if let Ok(mut t) = state.tracker.lock() {
                match item {
                    Item::Window(w) => t.note_window(w.pid, &w.title),
                    Item::App(a) => t.note_app(a.pid),
                    Item::Program(_)
                    | Item::AskLlm { .. }
                    | Item::OpenUrl(_)
                    | Item::Dir(_)
                    | Item::BrowserTab(_) => {
                        /* programs, LLM, URL, dir and browser-tab rows don't participate in recency */
                    }
                }
            }
            let res = match item {
                Item::Window(w) => state.platform.activate_window(w),
                Item::App(a) => state.platform.activate_app(a),
                Item::Program(p) => state.platform.launch_program(p),
                Item::AskLlm { provider, query } => {
                    // Remember the pick so next time this provider floats to
                    // the top of the fallback row.
                    let promote = {
                        let mut cfg = state.config.borrow_mut();
                        cfg.promote_llm_provider(*provider);
                        cfg.clone()
                    };
                    if let Err(e) = promote.save() {
                        tracing::warn!("save config after llm promote: {e:#}");
                    }
                    state.platform.open_llm(*provider, query)
                }
                Item::OpenUrl(url) => state.platform.open_url(url),
                Item::Dir(d) => {
                    let bundle_id = resolve_file_manager_bundle_id(&state.config.borrow());
                    switcheur_platform::file_manager::open_folder_with(
                        bundle_id.as_deref(),
                        &d.path,
                    )
                }
                Item::BrowserTab(t) => state.platform.activate_browser_tab(t),
            };
            if let Err(e) = res {
                tracing::warn!("activate: {e:#}");
            }
            state.current.borrow_mut().take()
        }
        SwitcherViewEvent::Dismissed => state.current.borrow_mut().take(),
        SwitcherViewEvent::OpenSettings => {
            let slot = state.current.borrow_mut().take();
            if let Err(e) = open_settings_window(state, cx) {
                tracing::warn!("open_settings_window: {e:#}");
            }
            slot
        }
        SwitcherViewEvent::FrameDeltaChanged { .. } => unreachable!("handled above"),
        SwitcherViewEvent::LicenseActivateRequested
        | SwitcherViewEvent::LicenseDismissed
        | SwitcherViewEvent::CloseWindowRequested(_)
        | SwitcherViewEvent::UpdateDownloadRequested
        | SwitcherViewEvent::UpdateDismissed
        | SwitcherViewEvent::QueryChanged(_)
        | SwitcherViewEvent::NeedsBrowserTabs => unreachable!("handled above"),
    };

    if let Some(slot) = slot_to_close {
        let _ = cx.update_window(slot.handle, |_ent, window, _cx| {
            window.remove_window();
        });
    }
    reset_system_switcher_cycle(state);
}

fn open_onboarding_window(state: &AppState, cx: &mut App) -> Result<()> {
    tracing::info!("open_onboarding_window: start");
    // LSUIElement=true makes us an accessory app; normal windows won't come to
    // the front unless we explicitly activate the process via NSApplication.
    cx.activate(true);

    let appearance = state.config.borrow().appearance;
    let theme = theme_for(appearance);
    let bounds = initial_bounds(cx, ONBOARDING_WIDTH, ONBOARDING_HEIGHT);

    // WindowKind::Normal (not PopUp): the wizard is the only thing the user
    // sees at first launch, so it needs to participate in normal focus
    // handling (receive key events, stay open while the system prompt is
    // shown, survive Dock/Command-Tab). PopUp windows are dismissed too
    // eagerly for a wizard.
    let options = WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(switcheur_i18n::tr("window.onboarding_title").into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        kind: WindowKind::Normal,
        is_movable: true,
        is_resizable: false,
        focus: true,
        show: true,
        ..Default::default()
    };

    let launch_at_startup = state.config.borrow().launch_at_startup;
    let entity_slot: Rc<RefCell<Option<Entity<OnboardingView>>>> = Rc::new(RefCell::new(None));
    let slot_for_builder = entity_slot.clone();
    let handle: WindowHandle<OnboardingView> = cx.open_window(options, move |window, cx| {
        let entity = cx.new(|cx| {
            let mut v = OnboardingView::new(launch_at_startup, cx);
            v.set_theme(theme, cx);
            v
        });
        *slot_for_builder.borrow_mut() = Some(entity.clone());
        let focus = entity.read(cx).focus_handle().clone();
        focus.focus(window, cx);
        // Closing the onboarding window (red traffic light) should quit the
        // app — there's no menu-bar icon or alternative UI while onboarding
        // is on-screen, so leaving the process running would strand the
        // user with no way back to the wizard except relaunching.
        // Normal completion calls `window.remove_window()` directly and
        // does not trigger this callback.
        window.on_window_should_close(cx, |_window, cx| {
            cx.quit();
            true
        });
        entity
    })?;

    let _ = handle.update(cx, |_view, window, _cx| {
        window.activate_window();
    });

    let entity = entity_slot.borrow().clone().expect("builder populated slot");
    let state_sub = state.clone();
    let sub = cx.subscribe(
        &entity,
        move |_entity, ev: &OnboardingViewEvent, cx: &mut App| {
            handle_onboarding_event(ev, &state_sub, cx);
        },
    );

    // Poll the Accessibility trust status while the wizard is visible. macOS
    // doesn't notify on grant, so we re-check every 500ms and push the result
    // into the view. The loop stops as soon as the permission flips to
    // granted (it's sticky) or when the entity is dropped (window closed).
    // WeakEntity lets us detect the drop without leaking a strong ref.
    //
    // `prompt=false` is mandatory here: passing `prompt=true` from a polling
    // loop re-fires the system dialog on every iteration. The cache it reads
    // does refresh in practice on modern macOS (the binary gets re-evaluated
    // when its cdhash matches an entry the user enabled). If the user grants
    // and nothing flips, the cdhash on disk no longer matches what TCC has
    // listed — usually because the binary was rebuilt; the fix there is to
    // re-toggle the entry in Settings, not to change this poll.
    let entity_for_poll = entity.downgrade();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let executor = cx.background_executor().clone();
        loop {
            executor.timer(Duration::from_millis(500)).await;
            let granted = ensure_accessibility(false);
            tracing::trace!(granted, "onboarding accessibility poll");
            let updated = entity_for_poll.update(cx, |view, cx| {
                view.set_accessibility_granted(granted, cx);
                granted
            });
            match updated {
                Ok(true) => {
                    tracing::info!("onboarding: accessibility granted");
                    cx.update(|cx| cx.activate(true));
                    break;
                }
                Ok(false) => continue,
                Err(_) => {
                    tracing::debug!("onboarding poll: entity dropped, stopping");
                    break;
                }
            }
        }
    })
    .detach();

    *state.onboarding.borrow_mut() = Some(WindowSlot {
        handle: handle.into(),
        _sub: sub,
    });
    Ok(())
}

fn handle_onboarding_event(ev: &OnboardingViewEvent, state: &AppState, cx: &mut App) {
    match ev {
        OnboardingViewEvent::AccessibilityRequested => {
            tracing::info!("onboarding: prompting for Accessibility");
            // Just the native dialog — no separate System Settings window. If
            // the user previously denied, the dialog won't reappear; the
            // polling loop below catches the eventual toggle in Settings.
            request_accessibility_prompt();
        }
        OnboardingViewEvent::HotkeyApplied(spec) => {
            if let Err(e) = state.hotkey.reregister(spec) {
                tracing::warn!("reregister hotkey (onboarding): {e:#}");
                return;
            }
            let mut c = state.config.borrow_mut();
            c.hotkey = spec.clone();
            if let Err(e) = c.save() {
                tracing::warn!("save config: {e:#}");
            }
        }
        OnboardingViewEvent::ReplaceSystemSwitcherChosen(on) => {
            if *on {
                match SystemSwitcherService::start() {
                    Ok(svc) => {
                        let rx = svc.receiver();
                        *state.system_switcher.borrow_mut() = Some(svc);
                        spawn_system_switcher_loop(cx, state.clone(), rx);
                        let mut c = state.config.borrow_mut();
                        c.replace_system_switcher = true;
                        if let Err(e) = c.save() {
                            tracing::warn!("save config: {e:#}");
                        }
                    }
                    Err(SystemSwitcherError::PermissionDenied) => {
                        tracing::warn!(
                            "Replace Cmd+Tab (onboarding) needs Input Monitoring — prompting"
                        );
                        prompt_input_monitoring();
                    }
                    Err(e) => {
                        tracing::warn!("SystemSwitcher start (onboarding): {e}");
                    }
                }
            }
        }
        OnboardingViewEvent::LicensePurchaseRequested => {
            if let Err(e) = open::that(LICENSE_BUY_URL) {
                tracing::warn!("open license URL: {e:#}");
            }
        }
        OnboardingViewEvent::LaunchAtStartupChanged(on) => {
            let res = if *on { startup::enable() } else { startup::disable() };
            if let Err(e) = res {
                tracing::warn!("launch-at-startup toggle (onboarding): {e:#}");
                return;
            }
            let mut c = state.config.borrow_mut();
            c.launch_at_startup = *on;
            if let Err(e) = c.save() {
                tracing::warn!("save config (onboarding launch toggle): {e:#}");
            }
        }
        OnboardingViewEvent::Finished => {
            {
                let mut c = state.config.borrow_mut();
                if !c.onboarding_completed {
                    c.onboarding_completed = true;
                    if let Err(e) = c.save() {
                        tracing::warn!("save config (onboarding finished): {e:#}");
                    }
                }
            }
            // Done screen already offers the licence CTA — suppress the
            // first-open nag in this session so the user isn't asked twice
            // back-to-back. Reset on next launch via `nag_shown_this_session`.
            state.nag_shown_this_session.set(true);
            let slot = state.onboarding.borrow_mut().take();
            if let Some(slot) = slot {
                let _ = cx.update_window(slot.handle, |_ent, window, _cx| {
                    window.remove_window();
                });
            }
        }
    }
}

fn open_settings_window(state: &AppState, cx: &mut App) -> Result<()> {
    tracing::info!("open_settings_window: start");
    // LSUIElement=true makes us an accessory app; normal windows won't come to
    // the front unless we explicitly activate the process via NSApplication.
    cx.activate(true);

    // Copy the handle out of the cell so we can re-borrow mutably below without
    // tripping a double-borrow panic.
    let existing = state.settings.borrow().as_ref().map(|s| s.handle);
    if let Some(handle) = existing {
        tracing::info!("settings already open, activating");
        let ok = cx
            .update_window(handle, |_ent, window, _cx| {
                window.activate_window();
            })
            .is_ok();
        if ok {
            return Ok(());
        }
        tracing::warn!("stale settings handle, recreating");
        *state.settings.borrow_mut() = None;
    }

    let cfg = state.config.borrow();
    let hotkey = cfg.hotkey.clone();
    let launch = cfg.launch_at_startup;
    let search_apps = cfg.search_apps;
    let ask_llm_enabled = cfg.ask_llm_enabled;
    let include_min = cfg.include_minimized;
    let show_all_spaces = cfg.show_all_spaces;
    let quick_type = cfg.quick_type;
    let replace_sys = cfg.replace_system_switcher;
    let sort_order = cfg.sort_order;
    let exclusions = cfg.exclusions.clone();
    let hotkey_excluded_apps = cfg.hotkey_excluded_apps.clone();
    let quick_type_excluded_apps = cfg.quick_type_excluded_apps.clone();
    let theme = theme_for(cfg.appearance);
    let license_key = cfg.license_key.clone().or_else(|| {
        cfg.license_token
            .as_deref()
            .and_then(|t| switcheur_core::license::verify_embedded(t).ok())
            .map(|t| t.key)
    });
    let zoxide_integration = cfg.zoxide_integration;
    let browser_tabs_integration = cfg.browser_tabs_integration;
    let file_manager = cfg.file_manager.clone();
    drop(cfg);
    // Freshly check at open-time so the warning's initial state matches
    // reality (user might have granted the permission between runs).
    let screen_recording_granted = has_screen_recording_permission();
    // Same idea for zoxide — if the user installed it between runs the
    // toggle should light up immediately. Refresh the cached path too.
    let detected = switcheur_platform::zoxide::detect();
    let zoxide_available = detected.is_some();
    *state.zoxide_bin.borrow_mut() = detected;

    // Re-scan installed file managers on settings open so the list tracks
    // apps installed between runs (or mid-session).
    let available_file_managers = switcheur_core::file_manager::available_file_managers(
        &switcheur_platform::file_manager::detected_file_manager_bundle_ids(),
    );

    let bounds = initial_bounds(cx, SETTINGS_WIDTH, SETTINGS_HEIGHT);
    tracing::info!(?bounds, "settings window bounds");

    let options = WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(switcheur_i18n::tr("window.settings_title").into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        kind: WindowKind::Normal,
        is_movable: true,
        is_resizable: true,
        focus: true,
        show: true,
        ..Default::default()
    };

    let entity_slot: Rc<RefCell<Option<Entity<SettingsView>>>> = Rc::new(RefCell::new(None));
    let slot_for_builder = entity_slot.clone();
    let handle: WindowHandle<SettingsView> = cx.open_window(options, move |window, cx| {
        let entity = cx.new(|cx| {
            let mut v = SettingsView::new(
                hotkey,
                launch,
                search_apps,
                ask_llm_enabled,
                include_min,
                show_all_spaces,
                screen_recording_granted,
                quick_type,
                replace_sys,
                sort_order,
                exclusions,
                hotkey_excluded_apps,
                quick_type_excluded_apps,
                license_key,
                zoxide_integration,
                zoxide_available,
                browser_tabs_integration,
                file_manager,
                available_file_managers,
                cx,
            );
            v.set_theme(theme, cx);
            v
        });
        *slot_for_builder.borrow_mut() = Some(entity.clone());
        let focus = entity.read(cx).focus_handle().clone();
        focus.focus(window, cx);
        entity
    })?;

    tracing::info!("settings window created");

    let _ = handle.update(cx, |_view, window, _cx| {
        window.activate_window();
    });

    let entity = entity_slot.borrow().clone().expect("builder populated slot");
    let state_sub = state.clone();
    let sub = cx.subscribe(&entity, move |entity, ev: &SettingsViewEvent, cx: &mut App| {
        handle_settings_event(ev, &entity, &state_sub, cx);
    });

    // Poll Screen Recording permission while the settings window is visible.
    // macOS does not notify on grant, so the warning under "Show all spaces"
    // would otherwise stay stale until the user closes and reopens settings.
    // Cheap call (CGPreflight…), stops when the entity is dropped.
    let entity_for_poll = entity.downgrade();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let executor = cx.background_executor().clone();
        loop {
            executor.timer(Duration::from_millis(500)).await;
            let granted = has_screen_recording_permission();
            let res = entity_for_poll.update(cx, |view, cx| {
                view.set_screen_recording_granted(granted, cx);
            });
            if res.is_err() {
                break; // entity dropped → window closed
            }
        }
    })
    .detach();

    *state.settings.borrow_mut() = Some(WindowSlot {
        handle: handle.into(),
        _sub: sub,
    });

    tracing::info!("settings window activated + subscribed");
    Ok(())
}

/// Open (or replace) the post-activation confirmation popup. Shown after a
/// `leswitcheur://activate?key=...` round-trip: success card with a heart
/// and the verified key, or an error card describing what went wrong. The
/// window closes on the OK button, on Enter/Escape, or when the red cross
/// is clicked.
fn open_thanks_window(state: &AppState, thanks: ThanksState, cx: &mut App) -> Result<()> {
    // LSUIElement=true: bring the app forward so a Normal window actually
    // shows up. Same reason as settings/onboarding.
    cx.activate(true);

    // Replace any lingering instance so rapid re-activations don't pile up
    // windows. Drop the slot first to release the subscription.
    if let Some(slot) = state.thanks.borrow_mut().take() {
        let _ = cx.update_window(slot.handle, |_ent, window, _cx| {
            window.remove_window();
        });
    }

    let appearance = state.config.borrow().appearance;
    let theme = theme_for(appearance);
    let bounds = initial_bounds(cx, THANKS_WIDTH, THANKS_HEIGHT);

    let options = WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(switcheur_i18n::tr("window.license_title").into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        kind: WindowKind::Normal,
        is_movable: true,
        is_resizable: false,
        focus: true,
        show: true,
        ..Default::default()
    };

    let thanks_for_builder = thanks.clone();
    let entity_slot: Rc<RefCell<Option<Entity<ThanksView>>>> = Rc::new(RefCell::new(None));
    let slot_for_builder = entity_slot.clone();
    let handle: WindowHandle<ThanksView> = cx.open_window(options, move |window, cx| {
        let entity = cx.new(|cx| {
            let mut v = ThanksView::new(thanks_for_builder.clone(), cx);
            v.set_theme(theme, cx);
            v
        });
        *slot_for_builder.borrow_mut() = Some(entity.clone());
        let focus = entity.read(cx).focus_handle().clone();
        focus.focus(window, cx);
        entity
    })?;

    let _ = handle.update(cx, |_view, window, _cx| {
        window.activate_window();
    });

    let entity = entity_slot.borrow().clone().expect("builder populated slot");
    let state_sub = state.clone();
    let sub = cx.subscribe(&entity, move |_entity, ev: &ThanksViewEvent, cx: &mut App| {
        match ev {
            ThanksViewEvent::Dismissed => {
                if let Some(slot) = state_sub.thanks.borrow_mut().take() {
                    let _ = cx.update_window(slot.handle, |_ent, window, _cx| {
                        window.remove_window();
                    });
                }
            }
        }
    });

    *state.thanks.borrow_mut() = Some(WindowSlot {
        handle: handle.into(),
        _sub: sub,
    });
    Ok(())
}

fn handle_settings_event(
    ev: &SettingsViewEvent,
    entity: &Entity<SettingsView>,
    state: &AppState,
    cx: &mut App,
) {
    match ev {
        SettingsViewEvent::HotkeyChanged(spec) => {
            if let Err(e) = state.hotkey.reregister(spec) {
                tracing::warn!("reregister hotkey: {e:#}");
                return;
            }
            let mut c = state.config.borrow_mut();
            c.hotkey = spec.clone();
            if let Err(e) = c.save() {
                tracing::warn!("save config: {e:#}");
            }
        }
        SettingsViewEvent::LaunchAtStartupChanged(on) => {
            let res = if *on { startup::enable() } else { startup::disable() };
            if let Err(e) = res {
                tracing::warn!("launch-at-startup toggle: {e:#}");
                return;
            }
            let mut c = state.config.borrow_mut();
            c.launch_at_startup = *on;
            if let Err(e) = c.save() {
                tracing::warn!("save config: {e:#}");
            }
        }
        SettingsViewEvent::SearchAppsChanged(on) => {
            let mut c = state.config.borrow_mut();
            c.search_apps = *on;
            if let Err(e) = c.save() {
                tracing::warn!("save config: {e:#}");
            }
        }
        SettingsViewEvent::AskLlmEnabledChanged(on) => {
            let mut c = state.config.borrow_mut();
            c.ask_llm_enabled = *on;
            if let Err(e) = c.save() {
                tracing::warn!("save config: {e:#}");
            }
        }
        SettingsViewEvent::ZoxideIntegrationChanged(on) => {
            {
                let mut c = state.config.borrow_mut();
                c.zoxide_integration = *on;
                if let Err(e) = c.save() {
                    tracing::warn!("save config: {e:#}");
                }
            }
            // Push the new state to the live switcher view if one is open
            // so the right pane appears/disappears immediately. Effective
            // on-state still requires the binary to be present.
            let effective = *on && state.zoxide_bin.borrow().is_some();
            let entity_opt = state.current.borrow().as_ref().map(|s| s.entity.clone());
            if let Some(entity) = entity_opt {
                entity.update(cx, |view, cx| view.set_zoxide_enabled(effective, cx));
            }
        }
        SettingsViewEvent::OpenZoxideHomepageRequested => {
            // Top of the GitHub README has the install instructions for
            // every platform — most accurate single landing page.
            if let Err(e) = open::that("https://github.com/ajeetdsouza/zoxide#installation") {
                tracing::warn!("open zoxide install page: {e:#}");
            }
        }
        SettingsViewEvent::BrowserTabsIntegrationChanged(on) => {
            {
                let mut c = state.config.borrow_mut();
                c.browser_tabs_integration = *on;
                if let Err(e) = c.save() {
                    tracing::warn!("save config: {e:#}");
                }
            }
            // Mirror the new value into any live switcher view so the
            // fallback tier starts / stops considering tabs immediately.
            let entity_opt = state.current.borrow().as_ref().map(|s| s.entity.clone());
            if let Some(entity) = entity_opt {
                entity.update(cx, |view, cx| {
                    view.set_browser_tabs_integration(*on, cx)
                });
            }
        }
        SettingsViewEvent::FileManagerChanged(id) => {
            let mut c = state.config.borrow_mut();
            c.file_manager = id.clone();
            if let Err(e) = c.save() {
                tracing::warn!("save config: {e:#}");
            }
        }
        SettingsViewEvent::IncludeMinimizedChanged(on) => {
            let mut c = state.config.borrow_mut();
            c.include_minimized = *on;
            if let Err(e) = c.save() {
                tracing::warn!("save config: {e:#}");
            }
        }
        SettingsViewEvent::ShowAllSpacesChanged(on) => {
            if *on && !has_screen_recording_permission() {
                // Block enabling without Screen Recording: revert the
                // optimistic toggle and surface the permission warning.
                // Don't persist — the user hasn't actually opted in.
                entity.update(cx, |v, cx| {
                    v.set_show_all_spaces(false, cx);
                    v.set_show_all_spaces_needs_permission(true, cx);
                });
                return;
            }
            entity.update(cx, |v, cx| v.set_show_all_spaces_needs_permission(false, cx));
            let mut c = state.config.borrow_mut();
            c.show_all_spaces = *on;
            if let Err(e) = c.save() {
                tracing::warn!("save config: {e:#}");
            }
        }
        SettingsViewEvent::OpenScreenRecordingSettingsRequested => {
            // First call shows the native system dialog; subsequent calls
            // after a denial fall back to opening System Settings. Either
            // way we re-query afterwards so the view can clear its warning
            // if the user just granted the permission.
            let granted = request_screen_recording_permission();
            entity.update(cx, |v, cx| v.set_screen_recording_granted(granted, cx));
        }
        SettingsViewEvent::ExclusionsChanged(rules) => {
            let (filter, errs) = ExclusionFilter::compile(rules);
            for (idx, err) in &errs {
                tracing::warn!("exclusion rule #{idx} has invalid regex: {err}");
            }
            *state.filter.borrow_mut() = filter;
            let mut c = state.config.borrow_mut();
            c.exclusions = rules.clone();
            if let Err(e) = c.save() {
                tracing::warn!("save config: {e:#}");
            }
        }
        SettingsViewEvent::SortOrderChanged(order) => {
            {
                let mut c = state.config.borrow_mut();
                c.sort_order = *order;
                if let Err(e) = c.save() {
                    tracing::warn!("save config: {e:#}");
                }
            }
            let want_window = matches!(order, SortOrder::RecentWindow);
            let mut recency = state.recency.borrow_mut();
            if want_window && !recency.window_tracking_enabled() {
                if let Ok(apps) = state.platform.list_apps() {
                    let pids: Vec<_> = apps.iter().map(|a| a.pid).collect();
                    recency.enable_window_tracking(&pids);
                }
            } else if !want_window && recency.window_tracking_enabled() {
                recency.disable_window_tracking();
            }
        }
        SettingsViewEvent::QuickTypeChanged(on) => {
            if *on {
                match QuickTypeService::start(
                    state.focused.clone(),
                    state.quick_type_excluded.clone(),
                ) {
                    Ok(svc) => {
                        let rx = svc.receiver();
                        *state.quick_type.borrow_mut() = Some(svc);
                        spawn_quick_type_loop(cx, state.clone(), rx);
                        let mut c = state.config.borrow_mut();
                        c.quick_type = true;
                        if let Err(e) = c.save() {
                            tracing::warn!("save config: {e:#}");
                        }
                    }
                    Err(QuickTypeError::PermissionDenied) => {
                        tracing::warn!(
                            "Quick Type needs Input Monitoring permission — opening System Settings"
                        );
                        prompt_input_monitoring();
                        entity.update(cx, |view, cx| view.set_quick_type(false, cx));
                    }
                    Err(e) => {
                        tracing::warn!("Quick Type start failed: {e}");
                        entity.update(cx, |view, cx| view.set_quick_type(false, cx));
                    }
                }
            } else {
                // Drop the service → CFRunLoopStop + join thread.
                state.quick_type.borrow_mut().take();
                let mut c = state.config.borrow_mut();
                c.quick_type = false;
                if let Err(e) = c.save() {
                    tracing::warn!("save config: {e:#}");
                }
            }
        }
        SettingsViewEvent::ReplaceSystemSwitcherChanged(on) => {
            if *on {
                match SystemSwitcherService::start() {
                    Ok(svc) => {
                        let rx = svc.receiver();
                        *state.system_switcher.borrow_mut() = Some(svc);
                        spawn_system_switcher_loop(cx, state.clone(), rx);
                        let mut c = state.config.borrow_mut();
                        c.replace_system_switcher = true;
                        if let Err(e) = c.save() {
                            tracing::warn!("save config: {e:#}");
                        }
                    }
                    Err(SystemSwitcherError::PermissionDenied) => {
                        tracing::warn!(
                            "Replace Cmd+Tab needs Input Monitoring — opening System Settings"
                        );
                        prompt_input_monitoring();
                        entity.update(cx, |view, cx| {
                            view.set_replace_system_switcher(false, cx)
                        });
                    }
                    Err(e) => {
                        tracing::warn!("SystemSwitcher start failed: {e}");
                        entity.update(cx, |view, cx| {
                            view.set_replace_system_switcher(false, cx)
                        });
                    }
                }
            } else {
                state.system_switcher.borrow_mut().take();
                let mut c = state.config.borrow_mut();
                c.replace_system_switcher = false;
                if let Err(e) = c.save() {
                    tracing::warn!("save config: {e:#}");
                }
            }
        }
        SettingsViewEvent::PickerOpenRequested { target } => {
            let apps = match state.platform.list_apps() {
                Ok(mut apps) => {
                    apps.sort_by_key(|a| a.name.to_lowercase());
                    apps.dedup_by(|a, b| a.name.eq_ignore_ascii_case(&b.name));
                    apps.into_iter()
                        .map(|a| (a.name, a.bundle_id))
                        .collect::<Vec<_>>()
                }
                Err(e) => {
                    tracing::warn!("list_apps for picker: {e:#}");
                    Vec::new()
                }
            };
            let target = target.clone();
            entity.update(cx, |view, cx| view.set_picker_apps(target, apps, cx));
        }
        SettingsViewEvent::HotkeyExcludedAppsChanged(list) => {
            state
                .hotkey_excluded
                .store(Arc::new(AppMatchSet::compile(list)));
            let mut c = state.config.borrow_mut();
            c.hotkey_excluded_apps = list.clone();
            if let Err(e) = c.save() {
                tracing::warn!("save config: {e:#}");
            }
        }
        SettingsViewEvent::QuickTypeExcludedAppsChanged(list) => {
            state
                .quick_type_excluded
                .store(Arc::new(AppMatchSet::compile(list)));
            let mut c = state.config.borrow_mut();
            c.quick_type_excluded_apps = list.clone();
            if let Err(e) = c.save() {
                tracing::warn!("save config: {e:#}");
            }
        }
        SettingsViewEvent::Dismissed => {
            let slot = state.settings.borrow_mut().take();
            if let Some(slot) = slot {
                let _ = cx.update_window(slot.handle, |_ent, window, _cx| {
                    window.remove_window();
                });
            }
        }
        SettingsViewEvent::LicensePurchaseRequested => {
            // Open the buy page; purchase happens on Stripe, and the post-
            // purchase flow comes back via the `leswitcheur://` URL scheme.
            if let Err(e) = open::that(LICENSE_BUY_URL) {
                tracing::warn!("open license site: {e:#}");
            }
        }
        SettingsViewEvent::LicenseActivateWithKey(key) => {
            // Settings is in front — skip the standalone thanks popup, let the
            // existing in-view success/error banner do the talking.
            start_activate_with_key(key.clone(), state.clone(), cx, false);
        }
        SettingsViewEvent::LicenseLogoutRequested => {
            tracing::info!("license logout from settings");
            state.licensed.set(false);
            state.nag_shown_this_session.set(false);
            {
                let mut cfg = state.config.borrow_mut();
                cfg.license_token = None;
                cfg.license_key = None;
                cfg.switcher_uses_since_nag = 0;
                cfg.nag_last_shown_at = None;
                if let Err(e) = cfg.save() {
                    tracing::warn!("save config on logout: {e:#}");
                }
            }
            entity.update(cx, |v, cx| v.set_license_key(None, cx));
        }
        SettingsViewEvent::QuitRequested => {
            tracing::info!("quit requested from settings");
            // Close any open windows so GPUI doesn't log a "window not found"
            // after the platform quit callback fires.
            if let Some(slot) = state.settings.borrow_mut().take() {
                let _ = cx.update_window(slot.handle, |_ent, window, _cx| {
                    window.remove_window();
                });
            }
            if let Some(slot) = state.current.borrow_mut().take() {
                let _ = cx.update_window(slot.handle, |_ent, window, _cx| {
                    window.remove_window();
                });
            }
            cx.quit();
        }
    }
}

/// Server-reported reason an activation was refused. Keep the strings aligned
/// with `error` values returned by `POST /api/activate` so a future log or
/// telemetry grep matches across both sides.
#[derive(Debug, Clone)]
#[allow(dead_code)] // `Network` payload is consumed via `Debug` in tracing logs.
enum ActivationError {
    InvalidKey,
    MonthlyLimit,
    YearlyLimit,
    Network(String),
    /// Backend returned a token but the embedded public key refused its
    /// signature. Usually means the app was built with a placeholder /
    /// mismatched verification key.
    TokenRejected,
}

impl ActivationError {
    fn i18n_key(&self) -> &'static str {
        match self {
            ActivationError::InvalidKey => "license.error_invalid",
            ActivationError::MonthlyLimit => "license.error_monthly",
            ActivationError::YearlyLimit => "license.error_yearly",
            ActivationError::Network(_) => "license.error_network",
            ActivationError::TokenRejected => "license.error_token",
        }
    }
}

/// Kick off activation of `key` against the backend. Updates the live Settings
/// view (if one is open) on success or error. Called both by the Settings
/// "Activate" button and by the URL-scheme handler after a post-purchase
/// redirect. When `show_thanks` is set (URL-scheme path), also pops a
/// standalone confirmation window so the user sees feedback even when no
/// other window is open.
fn start_activate_with_key(key: String, state: AppState, cx: &mut App, show_thanks: bool) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        let (tx, rx) = async_channel::bounded::<Result<String, ActivationError>>(1);
        let key_for_thread = key.clone();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(http_activate(&key_for_thread));
        });
        let outcome = rx.recv().await.unwrap_or_else(|_| {
            Err(ActivationError::Network("channel closed".into()))
        });
        let _ = cx.update(|cx| {
            let thanks: ThanksState = match outcome {
                Ok(token) => {
                    if store_verified_token(&token, &key, &state, cx) {
                        ThanksState::Success { key: key.clone() }
                    } else {
                        let err = ActivationError::TokenRejected;
                        update_settings_license_error(&state, Some(err.clone()), cx);
                        ThanksState::Error {
                            key: key.clone(),
                            message_i18n: err.i18n_key().to_string(),
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(?err, "activation failed");
                    update_settings_license_error(&state, Some(err.clone()), cx);
                    ThanksState::Error {
                        key: key.clone(),
                        message_i18n: err.i18n_key().to_string(),
                    }
                }
            };
            if show_thanks {
                if let Err(e) = open_thanks_window(&state, thanks, cx) {
                    tracing::warn!("open_thanks_window: {e:#}");
                }
            }
        });
    })
    .detach();
}

/// Drain the URL-scheme channel. Each `leswitcheur://activate?key=...` URL
/// triggers an activation round-trip with the embedded key.
fn spawn_url_scheme_loop(
    cx: &mut App,
    state: AppState,
    rx: async_channel::Receiver<String>,
) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Ok(url) = rx.recv().await {
            let Some(key) = parse_activate_url(&url) else {
                tracing::warn!(%url, "unrecognised URL scheme event");
                continue;
            };
            tracing::info!(%key, "activation triggered by URL scheme");
            let _ = cx.update(|cx| {
                start_activate_with_key(key, state.clone(), cx, true);
            });
        }
    })
    .detach();
}

/// Extract `key` from `leswitcheur://activate?key=LSWT-...`. Returns `None` for
/// anything else.
fn parse_activate_url(raw: &str) -> Option<String> {
    let stripped = raw.strip_prefix(&format!("{LICENSE_URL_SCHEME}://"))?;
    let (host, rest) = stripped.split_once('?').unwrap_or((stripped, ""));
    if !host.eq_ignore_ascii_case("activate") {
        return None;
    }
    for pair in rest.split('&') {
        if let Some(v) = pair.strip_prefix("key=") {
            // Light URL-decoding: only `%2D` (dash) shows up in our keys, but
            // we also tolerate `+` as space just in case.
            let decoded: String = v
                .replace('+', " ")
                .replace("%2D", "-")
                .replace("%2d", "-");
            let trimmed = decoded.trim().trim_matches('/').to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

/// Verify the token, persist it + the originating key, flip `licensed`, and
/// refresh a live Settings window if one is open. Returns whether the token
/// was accepted.
fn store_verified_token(token: &str, key: &str, state: &AppState, cx: &mut App) -> bool {
    let decoded = match switcheur_core::license::verify_embedded(token) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("activation token rejected: {e:#}");
            return false;
        }
    };
    if !decoded.key.eq_ignore_ascii_case(key) {
        tracing::warn!(
            payload_key = %decoded.key,
            sent_key = %key,
            "token payload key mismatch",
        );
        return false;
    }
    tracing::info!(key = %decoded.key, "license activated");
    state.licensed.set(true);
    {
        let mut cfg = state.config.borrow_mut();
        cfg.license_token = Some(token.to_owned());
        cfg.license_key = Some(decoded.key.clone());
        cfg.switcher_uses_since_nag = 0;
        cfg.nag_last_shown_at = None;
        if let Err(e) = cfg.save() {
            tracing::warn!("save config with token: {e:#}");
        }
    }
    let settings_handle = state.settings.borrow().as_ref().map(|s| s.handle);
    if let Some(h) = settings_handle {
        let k = decoded.key.clone();
        let _ = cx.update_window(h, |any_view, _window, cx| {
            if let Ok(view) = any_view.downcast::<SettingsView>() {
                view.update(cx, |v, cx| {
                    v.set_license_key(Some(k.clone()), cx);
                    v.set_license_error(None, cx);
                });
            }
        });
    }
    true
}

/// Push an error banner onto the live Settings view so the user sees why the
/// activation failed.
fn update_settings_license_error(
    state: &AppState,
    err: Option<ActivationError>,
    cx: &mut App,
) {
    let handle = match state.settings.borrow().as_ref() {
        Some(s) => s.handle,
        None => return,
    };
    let i18n_key = err.as_ref().map(|e| e.i18n_key());
    let _ = cx.update_window(handle, |any_view, _window, cx| {
        if let Ok(view) = any_view.downcast::<SettingsView>() {
            view.update(cx, |v, cx| {
                v.set_license_error(i18n_key.map(|k| k.to_string()), cx);
            });
        }
    });
}

/// One-shot blocking POST to `/api/activate`. Runs on a dedicated thread so the
/// GPUI task just awaits the result.
fn http_activate(key: &str) -> Result<String, ActivationError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(15))
        .build();
    let body = serde_json::json!({
        "key": key,
        "machine_id": switcheur_platform::machine_id(),
    });
    match agent.post(LICENSE_API_ACTIVATE).send_json(body) {
        Ok(resp) => {
            let v: serde_json::Value = resp
                .into_json()
                .map_err(|e| ActivationError::Network(format!("parse: {e}")))?;
            match v.get("token").and_then(|t| t.as_str()) {
                Some(token) => Ok(token.to_owned()),
                None => Err(ActivationError::Network("response missing token".into())),
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let code_err = match code {
                404 => ActivationError::InvalidKey,
                _ => {
                    let v: serde_json::Value = resp
                        .into_json()
                        .unwrap_or_else(|_| serde_json::json!({}));
                    match v.get("error").and_then(|e| e.as_str()) {
                        Some("monthly_limit") => ActivationError::MonthlyLimit,
                        Some("yearly_limit") => ActivationError::YearlyLimit,
                        Some("invalid_key") => ActivationError::InvalidKey,
                        _ => ActivationError::Network(format!("HTTP {code}")),
                    }
                }
            };
            Err(code_err)
        }
        Err(e) => Err(ActivationError::Network(e.to_string())),
    }
}

/// Bump the per-open counter and decide whether the fresh switcher window
/// should render the nag card. Fires on the first unlicensed open of each
/// app launch and then every `NAG_EVERY_N_USES` opens thereafter. Always a
/// no-op when licensed. Saves config whether or not the threshold was
/// reached so the count survives restarts.
fn tick_nag_counter(state: &AppState) -> bool {
    if state.licensed.get() {
        return false;
    }
    let first_of_session = !state.nag_shown_this_session.get();
    let mut cfg = state.config.borrow_mut();
    cfg.switcher_uses_since_nag = cfg.switcher_uses_since_nag.saturating_add(1);
    let threshold_reached = cfg.switcher_uses_since_nag >= NAG_EVERY_N_USES;
    let show = first_of_session || threshold_reached;
    if show {
        cfg.switcher_uses_since_nag = 0;
        cfg.nag_last_shown_at = Some(now_secs());
    }
    if let Err(e) = cfg.save() {
        tracing::warn!("save config after use bump: {e:#}");
    }
    drop(cfg);
    if show {
        state.nag_shown_this_session.set(true);
    }
    show
}

/// Manifest returned by `GET {LICENSE_BASE_URL}/api/updates/latest`. Parsed
/// into this shape by `parse_update_info`; only the fields we care about for
/// the banner + download are kept.
#[derive(Debug, Clone)]
struct UpdateInfo {
    version: semver::Version,
    url: String,
}

/// Compute the banner state for a newly-opened switcher window. Honours the
/// session dismissal flag so "×" keeps the banner hidden until the next
/// restart.
fn current_update_banner(state: &AppState) -> UpdateBannerState {
    if state.update_dismissed_this_session.get() {
        return UpdateBannerState::Hidden;
    }
    state.update_stage.borrow().clone()
}

/// Background update checker. Runs once at startup then re-checks every
/// `UPDATE_CHECK_INTERVAL`. Failures are silent — any of timeout, 5xx, or
/// parse error just leaves the banner hidden.
fn spawn_update_checker(cx: &mut App, state: AppState) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        let executor = cx.background_executor().clone();
        loop {
            if let Some(info) = run_update_check().await {
                let update = announce_update(&state, info);
                if update {
                    tracing::info!("update available");
                }
            }
            executor.timer(UPDATE_CHECK_INTERVAL).await;
        }
    })
    .detach();
}

/// Store a fresh update manifest into AppState and flip the banner to
/// Available unless the user has already moved past that stage (e.g. they
/// clicked Download and we're mid-transfer) or dismissed this session.
/// Returns true when the banner actually transitioned to Available.
fn announce_update(state: &AppState, info: UpdateInfo) -> bool {
    if state.update_dismissed_this_session.get() {
        *state.pending_update.borrow_mut() = Some(info);
        return false;
    }
    let stage = state.update_stage.borrow().clone();
    *state.pending_update.borrow_mut() = Some(info);
    if matches!(stage, UpdateBannerState::Hidden) {
        *state.update_stage.borrow_mut() = UpdateBannerState::Available;
        true
    } else {
        false
    }
}

/// Spawn the blocking HTTP probe on a worker thread and await its result.
async fn run_update_check() -> Option<UpdateInfo> {
    let (tx, rx) = async_channel::bounded::<Option<UpdateInfo>>(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(blocking_update_check());
    });
    rx.recv().await.ok().flatten()
}

fn blocking_update_check() -> Option<UpdateInfo> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(8))
        .build();
    let resp = match agent
        .get(&format!("{LICENSE_SITE}/api/updates/latest"))
        .call()
    {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("update check failed: {e}");
            return None;
        }
    };
    let v: serde_json::Value = resp.into_json().ok()?;
    let info = parse_update_info(&v)?;
    let local = semver::Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
    if info.version > local {
        Some(info)
    } else {
        None
    }
}

fn parse_update_info(v: &serde_json::Value) -> Option<UpdateInfo> {
    let version = semver::Version::parse(v.get("version")?.as_str()?).ok()?;
    let url = v.get("url")?.as_str()?.to_owned();
    if url.is_empty() {
        return None;
    }
    Some(UpdateInfo { version, url })
}

/// Kick off the DMG download on a worker thread. Banner transitions to
/// `Downloading` immediately (the view already did this optimistically on
/// click); on completion we open Finder on the DMG and flip to `Ready`.
fn start_update_download(entity: Entity<SwitcherView>, state: AppState, cx: &mut App) {
    let info = match state.pending_update.borrow().clone() {
        Some(info) => info,
        None => {
            tracing::warn!("update download requested without pending_update");
            return;
        }
    };
    *state.update_stage.borrow_mut() = UpdateBannerState::Downloading;
    cx.spawn(async move |cx: &mut AsyncApp| {
        let (tx, rx) = async_channel::bounded::<Option<std::path::PathBuf>>(1);
        let url = info.url.clone();
        let version = info.version.clone();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(blocking_download_dmg(&url, &version));
        });
        let path = rx.recv().await.ok().flatten();
        let _ = cx.update(|cx| match path {
            Some(p) => {
                if let Err(e) = open::that(&p) {
                    tracing::warn!("open DMG: {e}");
                }
                *state.update_stage.borrow_mut() = UpdateBannerState::Ready;
                let _ = entity
                    .update(cx, |v, cx| v.set_update_banner(UpdateBannerState::Ready, cx));
            }
            None => {
                // Revert to Available so the user can retry.
                *state.update_stage.borrow_mut() = UpdateBannerState::Available;
                let _ = entity.update(cx, |v, cx| {
                    v.set_update_banner(UpdateBannerState::Available, cx)
                });
            }
        });
    })
    .detach();
}

fn blocking_download_dmg(url: &str, version: &semver::Version) -> Option<std::path::PathBuf> {
    let dest_dir = update_download_dir()?;
    if let Err(e) = std::fs::create_dir_all(&dest_dir) {
        tracing::warn!("create update download dir: {e}");
        return None;
    }
    let filename = format!("LeSwitcheur-{version}.dmg");
    let dest = dest_dir.join(&filename);

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(120))
        .build();
    let resp = match agent.get(url).call() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("download {url}: {e}");
            return None;
        }
    };
    let mut reader = resp.into_reader();
    let tmp = dest.with_extension("dmg.part");
    let mut file = match std::fs::File::create(&tmp) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("create {tmp:?}: {e}");
            return None;
        }
    };
    if let Err(e) = std::io::copy(&mut reader, &mut file) {
        tracing::warn!("write DMG: {e}");
        return None;
    }
    drop(file);
    if let Err(e) = std::fs::rename(&tmp, &dest) {
        tracing::warn!("rename DMG: {e}");
        return None;
    }
    tracing::info!(path = %dest.display(), "update downloaded");
    Some(dest)
}

/// Canonical place for downloaded DMGs. Uses the user's `~/Downloads` folder
/// so the file is visible in Finder after we open it — easier for a first-run
/// user to find if they want to come back to it.
fn update_download_dir() -> Option<std::path::PathBuf> {
    let dirs = directories::UserDirs::new()?;
    dirs.download_dir().map(std::path::Path::to_path_buf)
}

/// Poll `/Applications/LeSwitcheur.app/Contents/Info.plist` for a newer
/// `CFBundleShortVersionString` and quit if found. Handles the in-place
/// upgrade case: the user drags the new `.app` over ours while we're still
/// running, and we need to stop so their next launch spawns a fresh process
/// of the new binary (LaunchServices would otherwise just reactivate us,
/// same bundle id).
fn spawn_install_drift_watcher(cx: &mut App, state: AppState) {
    let local = match semver::Version::parse(env!("CARGO_PKG_VERSION")) {
        Ok(v) => v,
        Err(_) => return,
    };
    cx.spawn(async move |cx: &mut AsyncApp| {
        let executor = cx.background_executor().clone();
        loop {
            executor.timer(UPDATE_DRIFT_POLL).await;
            if installed_version_is_newer(&local) {
                tracing::info!("installed app version > running; quitting for clean relaunch");
                let _ = cx.update(|cx| cx.quit());
                return;
            }
        }
    })
    .detach();
    // `state` captured so the watcher only lives as long as the app — harmless
    // to drop but keeps the signature uniform with other spawn_* helpers.
    let _ = state;
}

fn installed_version_is_newer(local: &semver::Version) -> bool {
    let path = std::path::Path::new("/Applications/LeSwitcheur.app/Contents/Info.plist");
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    // Hand-parse CFBundleShortVersionString out of the plist XML rather than
    // pulling a plist crate for one value. Format is predictable:
    //     <key>CFBundleShortVersionString</key>
    //     <string>0.1.0</string>
    let needle = "<key>CFBundleShortVersionString</key>";
    let Some(pos) = content.find(needle) else {
        return false;
    };
    let after_key = &content[pos + needle.len()..];
    let Some(open) = after_key.find("<string>") else {
        return false;
    };
    let value_start = open + "<string>".len();
    let Some(close) = after_key[value_start..].find("</string>") else {
        return false;
    };
    let value = after_key[value_start..value_start + close].trim();
    match semver::Version::parse(value) {
        Ok(installed) => installed > *local,
        Err(_) => false,
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn theme_for(appearance: Appearance) -> Theme {
    match appearance {
        Appearance::Light => Theme::light(),
        _ => Theme::dark(),
    }
}

/// Resolve the configured file-manager `id` to an installed bundle id, or
/// `None` when the preference is unset, unknown, or points at an app that
/// isn't currently installed. In every `None` case the caller routes through
/// the system default handler (Finder).
fn resolve_file_manager_bundle_id(cfg: &Config) -> Option<String> {
    let id = cfg.file_manager.as_deref()?;
    let installed = switcheur_platform::file_manager::detected_file_manager_bundle_ids();
    switcheur_core::file_manager::resolve_bundle_id(id, &installed)
}

fn initial_bounds(cx: &mut App, width: f32, height: f32) -> Bounds<Pixels> {
    if let Some(display) = cx.primary_display() {
        let b = display.bounds();
        // Half-screen-down instead of a third so the panel has room to grow
        // upward when the Program Launcher suggestion section expands above
        // the input (NSWindow anchors its origin at the bottom, so a taller
        // content size is pushed upward, keeping the bottom edge fixed).
        let origin = point(
            b.origin.x + (b.size.width - px(width)) / 2.,
            b.origin.y + (b.size.height - px(height)) / 2.,
        );
        Bounds {
            origin,
            size: size(px(width), px(height)),
        }
    } else {
        Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(width), px(height)),
        }
    }
}

fn collect_items(
    platform: &MacPlatform,
    config: &Config,
    filter: &ExclusionFilter,
    tracker: &RecencyTracker,
) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    // Hide our own switcher popup from its own list. Any other window we own
    // (notably the settings window) has a real title and stays visible — the
    // switcher popup is the only titleless window we ever create.
    let own_pid = std::process::id() as i32;
    match platform.list_windows(config.show_all_spaces) {
        Ok(ws) => {
            let total = ws.len();
            let mut kept: Vec<_> = ws
                .into_iter()
                .filter(|w| !(w.pid == own_pid && w.title.is_empty()))
                .filter(|w| config.include_minimized || !w.minimized)
                .filter(|w| !filter.is_excluded_window(w))
                .collect();
            sort_items(&mut kept, config.sort_order, tracker);
            tracing::info!(
                total,
                kept = kept.len(),
                include_minimized = config.include_minimized,
                show_all_spaces = config.show_all_spaces,
                sort_order = ?config.sort_order,
                "list_windows ok"
            );
            items.extend(kept.into_iter().map(Item::from));
        }
        Err(e) => tracing::warn!("list_windows: {e:#}"),
    }
    tracing::info!(total = items.len(), "collect_items");
    items
}
