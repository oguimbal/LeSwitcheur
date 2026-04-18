//! Settings panel: hotkey editor, toggles, and exclusion rules.
//!
//! Changes emit events upwards so the caller can apply them live (re-register
//! hotkey, flip auto-launch, recompile the exclusion filter, persist config).

use std::collections::HashMap;

use gpui::{
    deferred, div, prelude::*, px, AnyElement, App, Context, EventEmitter, FocusHandle, Focusable,
    IntoElement, KeyDownEvent, Keystroke, MouseButton, MouseDownEvent, ParentElement, Render,
    SharedString, Styled, Window,
};
use switcheur_core::{AppMatch, ExclusionFilter, ExclusionRule, HotkeySpec, SortOrder};
use switcheur_i18n::{modifier_symbol, tr as _tr, tr_sub};

use crate::theme::Theme;

fn tr(key: &str) -> SharedString {
    SharedString::from(_tr(key))
}

/// Which list the app-picker modal is feeding when the user finally selects
/// an app. Threaded through `PickerOpenRequested` → host → `set_picker_apps`
/// → `pick_app`, so one picker UI serves three distinct lists.
#[derive(Debug, Clone)]
pub enum PickerTarget {
    /// Switcher exclusion list: `None` appends a new rule, `Some(i)` replaces
    /// the app of rule at index `i`.
    Exclusion(Option<usize>),
    /// Appends to `hotkey_excluded_apps`.
    HotkeyException,
    /// Appends to `quick_type_excluded_apps`.
    QuickTypeException,
}

impl Default for PickerTarget {
    fn default() -> Self {
        PickerTarget::Exclusion(None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Shortcuts,
    Exclusions,
}

impl Default for SettingsTab {
    fn default() -> Self {
        SettingsTab::General
    }
}

#[derive(Debug, Clone)]
pub enum SettingsViewEvent {
    HotkeyChanged(HotkeySpec),
    LaunchAtStartupChanged(bool),
    SearchAppsChanged(bool),
    AskLlmEnabledChanged(bool),
    IncludeMinimizedChanged(bool),
    ShowAllSpacesChanged(bool),
    /// User clicked the "grant permission" button under the
    /// show-all-Spaces toggle. The host should invoke the native macOS
    /// permission prompt and reflect the new state back via
    /// [`SettingsView::set_screen_recording_granted`].
    OpenScreenRecordingSettingsRequested,
    ExclusionsChanged(Vec<ExclusionRule>),
    SortOrderChanged(SortOrder),
    /// Ask the host (main.rs) to load the running-apps list and call
    /// `set_picker_apps` with the same target.
    PickerOpenRequested { target: PickerTarget },
    QuickTypeChanged(bool),
    ReplaceSystemSwitcherChanged(bool),
    HotkeyExcludedAppsChanged(Vec<AppMatch>),
    QuickTypeExcludedAppsChanged(Vec<AppMatch>),
    /// The user confirmed quitting from the settings panel — the host should
    /// tear down any windows and call `cx.quit()`.
    QuitRequested,
    /// User clicked "Buy a licence" in the settings. Host should open the
    /// marketing site so the user can purchase (post-purchase flow returns
    /// via the `leswitcheur://` URL scheme).
    LicensePurchaseRequested,
    /// User typed an existing key into the settings input and hit Activate.
    /// Host runs the `/api/activate` round-trip and reports back via
    /// [`SettingsView::set_license_key`] / [`SettingsView::set_license_error`].
    LicenseActivateWithKey(String),
    /// User clicked "Sign out" on a licensed install. Host should clear the
    /// stored token + key and re-show the nag popup at next boot.
    LicenseLogoutRequested,
    Dismissed,
}

pub struct SettingsView {
    hotkey: HotkeySpec,
    launch_at_startup: bool,
    search_apps: bool,
    ask_llm_enabled: bool,
    include_minimized: bool,
    show_all_spaces: bool,
    /// Whether the OS currently trusts us to read window titles via
    /// Screen Recording. Used to decide whether to allow the
    /// show-all-Spaces toggle to flip on, and to hide the warning once
    /// the user grants the permission.
    screen_recording_granted: bool,
    /// Visible state of the inline warning under the show-all-Spaces
    /// toggle. Set to true when the user tries to flip it on without
    /// the permission; cleared when they turn the toggle off or the
    /// permission becomes granted.
    show_all_spaces_needs_permission: bool,
    quick_type: bool,
    replace_system_switcher: bool,
    sort_order: SortOrder,
    sort_picker_open: bool,
    recording: bool,
    theme: Theme,
    focus: FocusHandle,

    rules: Vec<ExclusionRule>,
    rule_errors: HashMap<usize, String>,
    editing_title: Option<usize>,

    hotkey_exceptions: Vec<AppMatch>,
    quick_type_exceptions: Vec<AppMatch>,
    hotkey_popover_open: bool,
    quick_type_popover_open: bool,

    picker_open: bool,
    picker_target: PickerTarget,
    picker_apps: Vec<(String, Option<String>)>,
    picker_query: String,

    current_tab: SettingsTab,

    /// Active license key (if any). `None` means the "Buy a licence" +
    /// manual-entry UI is shown instead.
    license_key: Option<SharedString>,
    /// Whether the collapsible "Enter existing key" input is expanded.
    license_entry_open: bool,
    /// Contents of the manual license-key text input.
    license_entry_value: String,
    /// When an activation returned an error, we show this i18n key inline
    /// under the input. Cleared on next user edit or successful activation.
    license_error_key: Option<SharedString>,
    /// True while the host is running `/api/activate` for a key the user just
    /// submitted. Disables the button and swaps its label to the activating
    /// variant.
    license_activating: bool,
    /// True for ~2s after the user clicks the Copy button, driving the label
    /// flip. Reset by a foreground timer.
    license_copied_at: Option<std::time::Instant>,

    /// True between click #1 and (click #2 | 5s timeout) on the Quit button.
    quit_pending: bool,
    /// Incremented on every arming; the 5s reset task checks it to avoid
    /// reverting after a later re-arm.
    quit_gen: u64,
    /// Same pattern as `quit_pending`, but for the "Remove license" button.
    license_remove_pending: bool,
    license_remove_gen: u64,
}

impl SettingsView {
    pub fn new(
        hotkey: HotkeySpec,
        launch_at_startup: bool,
        search_apps: bool,
        ask_llm_enabled: bool,
        include_minimized: bool,
        show_all_spaces: bool,
        screen_recording_granted: bool,
        quick_type: bool,
        replace_system_switcher: bool,
        sort_order: SortOrder,
        exclusions: Vec<ExclusionRule>,
        hotkey_exceptions: Vec<AppMatch>,
        quick_type_exceptions: Vec<AppMatch>,
        license_key: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let mut view = Self {
            hotkey,
            launch_at_startup,
            search_apps,
            ask_llm_enabled,
            include_minimized,
            show_all_spaces,
            screen_recording_granted,
            show_all_spaces_needs_permission: false,
            quick_type,
            replace_system_switcher,
            sort_order,
            sort_picker_open: false,
            recording: false,
            theme: Theme::default(),
            focus,
            rules: exclusions,
            rule_errors: HashMap::new(),
            editing_title: None,
            hotkey_exceptions,
            quick_type_exceptions,
            hotkey_popover_open: false,
            quick_type_popover_open: false,
            picker_open: false,
            picker_target: PickerTarget::default(),
            picker_apps: Vec::new(),
            picker_query: String::new(),
            current_tab: SettingsTab::default(),
            license_key: license_key.map(SharedString::from),
            license_entry_open: false,
            license_entry_value: String::new(),
            license_error_key: None,
            license_activating: false,
            license_copied_at: None,
            quit_pending: false,
            quit_gen: 0,
            license_remove_pending: false,
            license_remove_gen: 0,
        };
        view.recompute_errors();
        let _ = cx;
        view
    }

    pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>) {
        self.theme = theme;
        cx.notify();
    }

    /// Reflect a change in licensed state live (e.g. after a successful
    /// activation started from inside the settings window).
    pub fn set_license_key(&mut self, key: Option<String>, cx: &mut Context<Self>) {
        self.license_key = key.map(SharedString::from);
        if self.license_key.is_some() {
            self.license_entry_open = false;
            self.license_entry_value.clear();
            self.license_activating = false;
        }
        cx.notify();
    }

    /// Surface a translated error under the license input. Pass `None` to
    /// clear the banner (called after any successful activation).
    pub fn set_license_error(&mut self, i18n_key: Option<String>, cx: &mut Context<Self>) {
        self.license_error_key = i18n_key.map(SharedString::from);
        self.license_activating = false;
        cx.notify();
    }

    /// Reflect Quick Type state back to the UI without re-emitting the event.
    /// Used when the host rejects a toggle attempt (e.g. permission denied).
    pub fn set_quick_type(&mut self, on: bool, cx: &mut Context<Self>) {
        self.quick_type = on;
        cx.notify();
    }

    /// Same idea for the Cmd+Tab replacement toggle.
    pub fn set_replace_system_switcher(&mut self, on: bool, cx: &mut Context<Self>) {
        self.replace_system_switcher = on;
        cx.notify();
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Called by the host after it has gathered the running-apps list.
    pub fn set_picker_apps(
        &mut self,
        target: PickerTarget,
        apps: Vec<(String, Option<String>)>,
        cx: &mut Context<Self>,
    ) {
        self.picker_target = target;
        self.picker_apps = apps;
        self.picker_query.clear();
        self.picker_open = true;
        // Opening the modal visually supersedes any open popover; close them
        // so the state doesn't linger behind the modal.
        self.close_all_popovers();
        cx.notify();
    }

    fn close_all_popovers(&mut self) {
        self.sort_picker_open = false;
        self.hotkey_popover_open = false;
        self.quick_type_popover_open = false;
    }

    fn recompute_errors(&mut self) {
        let (_, errs) = ExclusionFilter::compile(&self.rules);
        self.rule_errors = errs.into_iter().map(|(i, e)| (i, e.to_string())).collect();
    }

    fn emit_changed(&mut self, cx: &mut Context<Self>) {
        self.recompute_errors();
        cx.emit(SettingsViewEvent::ExclusionsChanged(self.rules.clone()));
        cx.notify();
    }

    fn request_picker(&mut self, target: PickerTarget, cx: &mut Context<Self>) {
        self.editing_title = None;
        cx.emit(SettingsViewEvent::PickerOpenRequested { target });
    }

    fn close_picker(&mut self, cx: &mut Context<Self>) {
        self.picker_open = false;
        self.picker_query.clear();
        self.picker_target = PickerTarget::default();
        cx.notify();
    }

    fn pick_app(&mut self, name: String, cx: &mut Context<Self>) {
        match self.picker_target.clone() {
            PickerTarget::Exclusion(Some(i)) if i < self.rules.len() => {
                self.rules[i].app = name;
                self.close_picker(cx);
                self.editing_title = Some(i);
                self.emit_changed(cx);
            }
            PickerTarget::Exclusion(_) => {
                self.rules.push(ExclusionRule {
                    app: name,
                    title_pattern: String::new(),
                });
                let idx = self.rules.len() - 1;
                self.close_picker(cx);
                self.editing_title = Some(idx);
                self.emit_changed(cx);
            }
            PickerTarget::HotkeyException => {
                if !self
                    .hotkey_exceptions
                    .iter()
                    .any(|m| m.as_str().eq_ignore_ascii_case(&name))
                {
                    self.hotkey_exceptions.push(AppMatch::new(name));
                }
                self.close_picker(cx);
                cx.emit(SettingsViewEvent::HotkeyExcludedAppsChanged(
                    self.hotkey_exceptions.clone(),
                ));
                cx.notify();
            }
            PickerTarget::QuickTypeException => {
                if !self
                    .quick_type_exceptions
                    .iter()
                    .any(|m| m.as_str().eq_ignore_ascii_case(&name))
                {
                    self.quick_type_exceptions.push(AppMatch::new(name));
                }
                self.close_picker(cx);
                cx.emit(SettingsViewEvent::QuickTypeExcludedAppsChanged(
                    self.quick_type_exceptions.clone(),
                ));
                cx.notify();
            }
        }
    }

    fn filtered_picker_apps(&self) -> Vec<usize> {
        let q = self.picker_query.trim().to_lowercase();
        self.picker_apps
            .iter()
            .enumerate()
            .filter(|(_, (name, _))| q.is_empty() || name.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }

    fn start_recording(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recording = true;
        self.editing_title = None;
        self.focus.focus(window, cx);
        cx.notify();
    }

    fn toggle_launch(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.launch_at_startup = !self.launch_at_startup;
        cx.emit(SettingsViewEvent::LaunchAtStartupChanged(
            self.launch_at_startup,
        ));
        cx.notify();
    }

    fn toggle_search_apps(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_apps = !self.search_apps;
        cx.emit(SettingsViewEvent::SearchAppsChanged(self.search_apps));
        cx.notify();
    }

    fn toggle_ask_llm(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ask_llm_enabled = !self.ask_llm_enabled;
        cx.emit(SettingsViewEvent::AskLlmEnabledChanged(self.ask_llm_enabled));
        cx.notify();
    }

    fn toggle_include_minimized(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.include_minimized = !self.include_minimized;
        cx.emit(SettingsViewEvent::IncludeMinimizedChanged(
            self.include_minimized,
        ));
        cx.notify();
    }

    fn toggle_show_all_spaces(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = !self.show_all_spaces;
        // Flip locally first — the host will revert with `set_show_all_spaces`
        // if the permission gate blocks the "on" case. Mirrors how
        // Quick Type and Replace-Cmd+Tab report permission denials.
        self.show_all_spaces = target;
        if !target {
            self.show_all_spaces_needs_permission = false;
        }
        cx.emit(SettingsViewEvent::ShowAllSpacesChanged(target));
        cx.notify();
    }

    /// Reflect the show-all-Spaces state back to the UI without re-emitting
    /// the event. Used when the host rejects a toggle attempt because
    /// Screen Recording isn't granted.
    pub fn set_show_all_spaces(&mut self, on: bool, cx: &mut Context<Self>) {
        self.show_all_spaces = on;
        cx.notify();
    }

    /// Show or hide the inline "requires Screen Recording" warning below
    /// the show-all-Spaces toggle.
    pub fn set_show_all_spaces_needs_permission(
        &mut self,
        visible: bool,
        cx: &mut Context<Self>,
    ) {
        self.show_all_spaces_needs_permission = visible;
        cx.notify();
    }

    /// Update the cached Screen Recording grant status. If it just flipped
    /// to true, also clear any lingering permission warning.
    pub fn set_screen_recording_granted(&mut self, granted: bool, cx: &mut Context<Self>) {
        self.screen_recording_granted = granted;
        if granted {
            self.show_all_spaces_needs_permission = false;
        }
        cx.notify();
    }

    fn toggle_sort_picker(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_open = self.sort_picker_open;
        self.close_all_popovers();
        self.sort_picker_open = !was_open;
        cx.notify();
    }

    fn toggle_hotkey_popover(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_open = self.hotkey_popover_open;
        self.close_all_popovers();
        self.hotkey_popover_open = !was_open;
        cx.notify();
    }

    fn toggle_quick_type_popover(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_open = self.quick_type_popover_open;
        self.close_all_popovers();
        self.quick_type_popover_open = !was_open;
        cx.notify();
    }

    fn pick_sort_order(&mut self, order: SortOrder, cx: &mut Context<Self>) {
        self.sort_order = order;
        self.sort_picker_open = false;
        cx.emit(SettingsViewEvent::SortOrderChanged(order));
        cx.notify();
    }

    fn on_quit_click(&mut self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.quit_pending {
            cx.emit(SettingsViewEvent::QuitRequested);
            return;
        }
        self.quit_pending = true;
        self.quit_gen = self.quit_gen.wrapping_add(1);
        let gen = self.quit_gen;
        cx.notify();

        // 5s auto-revert. We capture the generation; if the user re-arms the
        // button later, the later timer will see a mismatch and skip the reset.
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(5))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.quit_gen == gen && view.quit_pending {
                    view.quit_pending = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn on_license_remove_click(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.license_remove_pending {
            self.license_remove_pending = false;
            cx.emit(SettingsViewEvent::LicenseLogoutRequested);
            cx.notify();
            return;
        }
        self.license_remove_pending = true;
        self.license_remove_gen = self.license_remove_gen.wrapping_add(1);
        let gen = self.license_remove_gen;
        cx.notify();

        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(5))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.license_remove_gen == gen && view.license_remove_pending {
                    view.license_remove_pending = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn toggle_quick_type(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.quick_type = !self.quick_type;
        cx.emit(SettingsViewEvent::QuickTypeChanged(self.quick_type));
        cx.notify();
    }

    fn toggle_replace_system_switcher(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_system_switcher = !self.replace_system_switcher;
        cx.emit(SettingsViewEvent::ReplaceSystemSwitcherChanged(
            self.replace_system_switcher,
        ));
        cx.notify();
    }

    fn submit_license_key(&mut self, cx: &mut Context<Self>) {
        let trimmed = self.license_entry_value.trim().to_ascii_uppercase();
        if trimmed.is_empty() || self.license_activating {
            return;
        }
        self.license_activating = true;
        self.license_error_key = None;
        cx.emit(SettingsViewEvent::LicenseActivateWithKey(trimmed));
        cx.notify();
    }

    fn copy_license_key_to_clipboard(&mut self, cx: &mut Context<Self>) {
        if let Some(k) = self.license_key.clone() {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(k.to_string()));
            self.license_copied_at = Some(std::time::Instant::now());
            cx.notify();
            // Revert the "Copied" label after 2s. Uses a foreground task so the
            // notify happens on the UI thread.
            cx.spawn(async move |view, cx: &mut gpui::AsyncApp| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                let _ = view.update(cx, |v, cx| {
                    v.license_copied_at = None;
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &ev.keystroke;

        // Cmd+W closes the settings window, matching standard macOS behavior.
        // Skipped while recording a hotkey so the user can bind Cmd+W itself.
        if !self.recording && k.modifiers.platform && k.key == "w" {
            cx.emit(SettingsViewEvent::Dismissed);
            return;
        }

        if self.picker_open {
            if k.key == "escape" {
                self.close_picker(cx);
                return;
            }
            if k.key == "return" || k.key == "enter" {
                if let Some(first) = self.filtered_picker_apps().first().copied() {
                    let name = self.picker_apps[first].0.clone();
                    self.pick_app(name, cx);
                }
                return;
            }
            if k.key == "backspace" {
                self.picker_query.pop();
                cx.notify();
                return;
            }
            if k.modifiers.control || k.modifiers.platform || k.modifiers.function {
                return;
            }
            if let Some(ch) = k.key_char.as_deref() {
                if !ch.is_empty() && !ch.chars().any(|c| c.is_control()) {
                    self.picker_query.push_str(ch);
                    cx.notify();
                }
            }
            return;
        }

        if self.recording {
            if let Some(spec) = keystroke_to_spec(k) {
                self.hotkey = spec.clone();
                self.recording = false;
                cx.emit(SettingsViewEvent::HotkeyChanged(spec));
                cx.notify();
                return;
            }
            if k.key == "escape" {
                self.recording = false;
                cx.notify();
                return;
            }
            return;
        }

        if self.license_entry_open && self.license_key.is_none() {
            if k.key == "escape" {
                self.license_entry_open = false;
                self.license_entry_value.clear();
                self.license_error_key = None;
                cx.notify();
                return;
            }
            if k.key == "return" || k.key == "enter" {
                self.submit_license_key(cx);
                return;
            }
            if k.key == "backspace" {
                self.license_entry_value.pop();
                self.license_error_key = None;
                cx.notify();
                return;
            }
            // Cmd+V: paste from clipboard. Normalised like typed input —
            // control chars stripped, rest uppercased.
            if k.modifiers.platform && k.key == "v" {
                if let Some(item) = cx.read_from_clipboard() {
                    if let Some(text) = item.text() {
                        let mut changed = false;
                        for c in text.chars() {
                            if c.is_control() {
                                continue;
                            }
                            self.license_entry_value.push(c.to_ascii_uppercase());
                            changed = true;
                        }
                        if changed {
                            self.license_error_key = None;
                            cx.notify();
                        }
                    }
                }
                return;
            }
            if k.modifiers.control || k.modifiers.platform || k.modifiers.function {
                return;
            }
            if let Some(ch) = k.key_char.as_deref() {
                if !ch.is_empty() && !ch.chars().any(|c| c.is_control()) {
                    // Normalise in-place: keys are uppercase + dash-grouped.
                    for c in ch.chars() {
                        self.license_entry_value.push(c.to_ascii_uppercase());
                    }
                    self.license_error_key = None;
                    cx.notify();
                }
            }
            return;
        }

        if let Some(idx) = self.editing_title {
            if idx >= self.rules.len() {
                self.editing_title = None;
                cx.notify();
                return;
            }
            if k.key == "escape" {
                self.editing_title = None;
                cx.notify();
                return;
            }
            if k.key == "backspace" {
                self.rules[idx].title_pattern.pop();
                self.emit_changed(cx);
                return;
            }
            if k.modifiers.control || k.modifiers.platform || k.modifiers.function {
                return;
            }
            if let Some(ch) = k.key_char.as_deref() {
                if !ch.is_empty() && !ch.chars().any(|c| c.is_control()) {
                    self.rules[idx].title_pattern.push_str(ch);
                    self.emit_changed(cx);
                    return;
                }
            }
            return;
        }

        if k.key == "escape" {
            if self.sort_picker_open
                || self.hotkey_popover_open
                || self.quick_type_popover_open
            {
                self.close_all_popovers();
                cx.notify();
                return;
            }
            cx.emit(SettingsViewEvent::Dismissed);
        }
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<SettingsViewEvent> for SettingsView {}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        let body: AnyElement = if self.picker_open {
            self.render_picker(cx).into_any_element()
        } else {
            self.render_main(cx).into_any_element()
        };

        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_size(px(14.0))
            .child(body)
    }
}

impl SettingsView {
    fn render_main(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        let body: AnyElement = match self.current_tab {
            SettingsTab::General => self.render_general_tab(cx),
            SettingsTab::Shortcuts => self.render_shortcuts_tab(cx),
            SettingsTab::Exclusions => self.render_exclusions_section(cx),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(self.render_tabs(cx))
            .child(
                div()
                    .id("settings-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_y_scroll()
                    .px_6()
                    .py_5()
                    .gap_5()
                    .child(body),
            )
            .child(
                div()
                    .px_6()
                    .pb_6()
                    .bg(theme.background)
                    .child(self.render_quit_row(cx)),
            )
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        div()
            .flex()
            .flex_row()
            .gap_1()
            .px_6()
            .pt_4()
            .pb_2()
            .border_b_1()
            .border_color(theme.border)
            .child(self.render_tab_button(tr("settings.tab_general"), SettingsTab::General, cx))
            .child(self.render_tab_button(tr("settings.tab_shortcuts"), SettingsTab::Shortcuts, cx))
            .child(self.render_tab_button(tr("settings.tab_exclusions"), SettingsTab::Exclusions, cx))
            .into_any_element()
    }

    fn render_tab_button(
        &self,
        label: SharedString,
        target: SettingsTab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let active = self.current_tab == target;
        let mut btn = div()
            .px_3()
            .py_1p5()
            .rounded_md()
            .cursor_pointer()
            .text_color(if active { theme.foreground } else { theme.muted })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.switch_tab(target, cx);
                }),
            )
            .child(label);
        if active {
            btn = btn.bg(theme.selection);
        } else {
            btn = btn.hover(|s| s.bg(theme.selection));
        }
        btn.into_any_element()
    }

    fn switch_tab(&mut self, tab: SettingsTab, cx: &mut Context<Self>) {
        if self.current_tab == tab {
            return;
        }
        // Reset transient UI from the previous tab so popovers and in-progress
        // edits don't reappear on return.
        self.close_all_popovers();
        self.editing_title = None;
        self.recording = false;
        self.current_tab = tab;
        cx.notify();
    }

    fn render_general_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(toggle_row(
                tr("settings.launch_at_startup"),
                None,
                self.launch_at_startup,
                &theme,
                cx.listener(Self::toggle_launch),
            ))
            .child(toggle_row(
                tr("settings.search_apps"),
                Some(tr("settings.search_apps_hint")),
                self.search_apps,
                &theme,
                cx.listener(Self::toggle_search_apps),
            ))
            .child(toggle_row(
                tr("settings.ask_llm"),
                Some(tr("settings.ask_llm_hint")),
                self.ask_llm_enabled,
                &theme,
                cx.listener(Self::toggle_ask_llm),
            ))
            .child(toggle_row(
                tr("settings.include_minimized"),
                Some(tr("settings.include_minimized_hint")),
                self.include_minimized,
                &theme,
                cx.listener(Self::toggle_include_minimized),
            ))
            .child(self.render_show_all_spaces_block(cx))
            .child(self.render_sort_row(cx))
            .child(self.render_license_row(cx))
            .into_any_element()
    }

    fn render_license_row(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(key) = self.license_key.clone() {
            self.render_licensed_row(key, cx)
        } else {
            self.render_unlicensed_row(cx)
        }
    }

    fn render_licensed_row(&self, key: SharedString, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let copied = self
            .license_copied_at
            .map(|t| t.elapsed() < std::time::Duration::from_secs(2))
            .unwrap_or(false);
        let copy_label: SharedString = if copied {
            tr("license.copied")
        } else {
            tr("license.copy")
        };

        let key_pill = div()
            .flex_1()
            .px_2()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .text_color(theme.foreground)
            .child(key.clone());

        let copy_btn = div()
            .px_3()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.selection)
            .text_color(theme.foreground)
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.copy_license_key_to_clipboard(cx);
                }),
            )
            .child(copy_label);

        // Two-step confirmation: click #1 arms (label flips + border/bg go
        // destructive), click #2 within 5s actually clears the license.
        let remove_pending = self.license_remove_pending;
        let (remove_label, remove_bg, remove_text, remove_border) = if remove_pending {
            (
                tr("license.remove_confirm"),
                theme.destructive,
                gpui::rgb(0xffffff),
                theme.destructive,
            )
        } else {
            (
                tr("license.remove"),
                theme.background,
                theme.destructive,
                theme.destructive,
            )
        };
        let logout_btn = div()
            .px_3()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(remove_border)
            .bg(remove_bg)
            .text_color(remove_text)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_license_remove_click))
            .child(remove_label);

        let row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(
                div()
                    .w(px(140.0))
                    .text_color(theme.muted)
                    .child(tr("license.key_label")),
            )
            .child(key_pill)
            .child(copy_btn)
            .child(logout_btn);

        div()
            .flex()
            .flex_col()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(theme.border)
            .child(row)
            .into_any_element()
    }

    fn render_unlicensed_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;

        let buy_btn = div()
            .px_3()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.selection)
            .text_color(theme.foreground)
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _: &MouseDownEvent, _, cx| {
                    cx.emit(SettingsViewEvent::LicensePurchaseRequested);
                }),
            )
            .child(tr("license.purchase"));

        let toggle_entry = div()
            .text_color(theme.muted)
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.license_entry_open = !this.license_entry_open;
                    if !this.license_entry_open {
                        this.license_entry_value.clear();
                        this.license_error_key = None;
                    }
                    cx.notify();
                }),
            )
            .child(tr("license.enter_key"));

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .child(
                div()
                    .text_color(theme.muted)
                    .child(tr("license.unlicensed")),
            )
            .child(buy_btn);

        let mut col = div()
            .flex()
            .flex_col()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(theme.border)
            .child(header)
            .child(toggle_entry);

        if self.license_entry_open {
            col = col.child(self.render_license_entry(cx));
        }
        col.into_any_element()
    }

    fn render_license_entry(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let input_text: AnyElement = if self.license_entry_value.is_empty() {
            div()
                .text_color(theme.muted)
                .child(tr("license.key_placeholder"))
                .into_any_element()
        } else {
            div()
                .text_color(theme.foreground)
                .child(self.license_entry_value.clone())
                .into_any_element()
        };

        let input = div()
            .flex_1()
            .px_2()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(theme.accent)
            .cursor_text()
            .child(input_text);

        let activate_label: SharedString = if self.license_activating {
            tr("license.activating_key")
        } else {
            tr("license.activate_key")
        };
        let disabled = self.license_activating || self.license_entry_value.trim().is_empty();
        let activate_btn = {
            let mut b = div()
                .px_3()
                .py_1p5()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.selection)
                .text_color(theme.foreground)
                .child(activate_label);
            if !disabled {
                b = b.cursor_pointer().on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| {
                        this.submit_license_key(cx);
                    }),
                );
            } else {
                b = b.text_color(theme.muted);
            }
            b
        };

        let row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(input)
            .child(activate_btn);

        let mut col = div().flex().flex_col().gap_1().child(row);
        if let Some(err_key) = self.license_error_key.clone() {
            col = col.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.match_highlight)
                    .child(tr(err_key.as_ref())),
            );
        }
        col.into_any_element()
    }

    fn render_shortcuts_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let hotkey_label: SharedString = if self.recording {
            tr("settings.press_combo")
        } else {
            format_spec(&self.hotkey).into()
        };

        let hotkey_block = div()
            .flex()
            .flex_col()
            .gap_1p5()
            .child(shortcut_row(
                &hotkey_label,
                self.recording,
                &theme,
                cx.listener(Self::start_recording),
            ))
            .child(self.render_hotkey_exception_row(cx));

        let mut quick_type_block = div().flex().flex_col().gap_1p5().child(toggle_row(
            tr("settings.quick_type"),
            Some(tr("settings.quick_type_hint")),
            self.quick_type,
            &theme,
            cx.listener(Self::toggle_quick_type),
        ));
        if self.quick_type {
            quick_type_block = quick_type_block.child(self.render_quick_type_exception_row(cx));
        }

        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(hotkey_block)
            .child(quick_type_block)
            .child(toggle_row(
                tr("settings.replace_cmd_tab"),
                Some(tr("settings.replace_cmd_tab_hint")),
                self.replace_system_switcher,
                &theme,
                cx.listener(Self::toggle_replace_system_switcher),
            ))
            .into_any_element()
    }

    fn render_show_all_spaces_block(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let mut block = div().flex().flex_col().gap_1p5().child(toggle_row(
            tr("settings.show_all_spaces"),
            Some(tr("settings.show_all_spaces_hint")),
            self.show_all_spaces,
            &theme,
            cx.listener(Self::toggle_show_all_spaces),
        ));
        if self.show_all_spaces_needs_permission {
            block = block.child(self.render_screen_recording_warning(cx));
        }
        block.into_any_element()
    }

    fn render_screen_recording_warning(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let message = div()
            .flex_1()
            .text_size(px(12.0))
            .text_color(theme.match_highlight)
            .child(tr("settings.show_all_spaces_needs_permission"));
        let button = div()
            .px_3()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .text_color(theme.foreground)
            .hover(|s| s.bg(theme.selection))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _: &MouseDownEvent, _, cx| {
                    cx.emit(SettingsViewEvent::OpenScreenRecordingSettingsRequested);
                }),
            )
            .child(tr("settings.open_screen_recording_settings"));
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            // Align under the toggle label, not the track.
            .pl(px(48.0))
            .child(message)
            .child(button)
            .into_any_element()
    }

    fn render_quit_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let pending = self.quit_pending;
        let (label, bg, text_color) = if pending {
            (
                tr("settings.quit_confirm"),
                theme.destructive,
                gpui::rgb(0xffffff),
            )
        } else {
            (tr("settings.quit"), theme.background, theme.destructive)
        };

        // Thin top border acts as a visual divider between the benign settings
        // above and the destructive action below.
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .mt_4()
            .pt_4()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.destructive)
                    .bg(bg)
                    .text_color(text_color)
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_quit_click))
                    .child("⎋")
                    .child(label),
            )
            .into_any_element()
    }

    fn render_sort_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let current = self.sort_order;

        // Field: rectangle with current label + chevron. Click toggles picker.
        let field = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .px_3()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(if self.sort_picker_open {
                theme.accent
            } else {
                theme.border
            })
            .bg(theme.selection)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::toggle_sort_picker))
            .text_color(theme.foreground)
            .child(sort_order_label(current))
            .child(div().text_color(theme.muted).child("▾"));

        let mut field_container = div().relative().flex_1().child(field);
        if self.sort_picker_open {
            // `deferred` paints the popover after all sibling content, so it
            // stays on top of the exclusion section that is rendered later in
            // the tree.
            field_container = field_container.child(
                deferred(self.render_sort_popover(cx)).with_priority(10),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(
                div()
                    .min_w(px(140.0))
                    .text_color(theme.muted)
                    .child(tr("sort.header")),
            )
            .child(field_container)
            .into_any_element()
    }

    fn render_hotkey_exception_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let open = self.hotkey_popover_open;
        let summary = exceptions_summary(&self.hotkey_exceptions);
        let summary_color = if self.hotkey_exceptions.is_empty() {
            theme.muted
        } else {
            theme.foreground
        };

        let field = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .px_3()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(if open { theme.accent } else { theme.border })
            .bg(theme.selection)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::toggle_hotkey_popover))
            .text_color(summary_color)
            .child(summary)
            .child(div().text_color(theme.muted).child("▾"));

        let mut field_container = div().relative().flex_1().child(field);
        if open {
            field_container = field_container.child(
                deferred(self.render_exception_popover(
                    self.hotkey_exceptions.clone(),
                    PickerTarget::HotkeyException,
                    cx,
                ))
                .with_priority(10),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .pl(px(24.0))
            .child(
                div()
                    .min_w(px(116.0))
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child(tr("settings.disabled_in")),
            )
            .child(field_container)
            .into_any_element()
    }

    fn render_quick_type_exception_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let open = self.quick_type_popover_open;
        let summary = exceptions_summary(&self.quick_type_exceptions);
        let summary_color = if self.quick_type_exceptions.is_empty() {
            theme.muted
        } else {
            theme.foreground
        };

        let field = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .px_3()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(if open { theme.accent } else { theme.border })
            .bg(theme.selection)
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(Self::toggle_quick_type_popover),
            )
            .text_color(summary_color)
            .child(summary)
            .child(div().text_color(theme.muted).child("▾"));

        let mut field_container = div().relative().flex_1().child(field);
        if open {
            field_container = field_container.child(
                deferred(self.render_exception_popover(
                    self.quick_type_exceptions.clone(),
                    PickerTarget::QuickTypeException,
                    cx,
                ))
                .with_priority(10),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .pl(px(52.0))
            .child(
                div()
                    .min_w(px(88.0))
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child(tr("settings.disabled_in")),
            )
            .child(field_container)
            .into_any_element()
    }

    fn render_exception_popover(
        &self,
        list: Vec<AppMatch>,
        target: PickerTarget,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;

        let mut popover = div()
            .absolute()
            .top(px(38.0))
            .left(px(0.0))
            .w_full()
            .bg(theme.background)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_md()
            .flex()
            .flex_col()
            .py_1();

        if list.is_empty() {
            popover = popover.child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child(tr("settings.no_exceptions")),
            );
        } else {
            for (i, entry) in list.iter().enumerate() {
                let target_clone = target.clone();
                let row = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .child(div().flex_1().text_color(theme.foreground).child(entry.as_str().to_string()))
                    .child(
                        div()
                            .w(px(24.0))
                            .h(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .cursor_pointer()
                            .text_color(theme.muted)
                            .hover(|s| s.bg(theme.selection))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                    this.remove_exception(&target_clone, i, cx);
                                }),
                            )
                            .child("×"),
                    );
                popover = popover.child(row);
            }
        }

        let add_target = target.clone();
        let add_row = div()
            .mt_1()
            .mx_1()
            .px_3()
            .py_1p5()
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(12.0))
            .text_color(theme.foreground)
            .cursor_pointer()
            .hover(|s| s.bg(theme.selection))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.request_picker(add_target.clone(), cx);
                }),
            )
            .child(tr("settings.add_app"));

        popover.child(add_row).into_any_element()
    }

    fn remove_exception(
        &mut self,
        target: &PickerTarget,
        i: usize,
        cx: &mut Context<Self>,
    ) {
        match target {
            PickerTarget::HotkeyException => {
                if i < self.hotkey_exceptions.len() {
                    self.hotkey_exceptions.remove(i);
                    cx.emit(SettingsViewEvent::HotkeyExcludedAppsChanged(
                        self.hotkey_exceptions.clone(),
                    ));
                    cx.notify();
                }
            }
            PickerTarget::QuickTypeException => {
                if i < self.quick_type_exceptions.len() {
                    self.quick_type_exceptions.remove(i);
                    cx.emit(SettingsViewEvent::QuickTypeExcludedAppsChanged(
                        self.quick_type_exceptions.clone(),
                    ));
                    cx.notify();
                }
            }
            PickerTarget::Exclusion(_) => {}
        }
    }

    fn render_sort_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let current = self.sort_order;
        let options = [
            SortOrder::RecentApp,
            SortOrder::RecentWindow,
            SortOrder::Title,
            SortOrder::AppName,
        ];

        let mut popover = div()
            .absolute()
            .top(px(38.0))
            .left(px(0.0))
            .w_full()
            .bg(theme.background)
            .border_1()
            .border_color(theme.border)
            .rounded_md()
            .shadow_md()
            .flex()
            .flex_col()
            .py_1();

        for order in options {
            let selected = order == current;
            popover = popover.child(self.render_sort_option(order, selected, cx));
        }

        popover.into_any_element()
    }

    fn render_sort_option(
        &self,
        order: SortOrder,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let main_label = sort_order_label(order);
        let hint = sort_order_hint(order);

        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .px_3()
            .py_1p5()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.pick_sort_order(order, cx);
                }),
            );
        if selected {
            row = row.bg(theme.selection);
        }

        let mut label_col = div().flex().flex_col().gap_0p5().child(
            div()
                .text_color(theme.foreground)
                .child(main_label),
        );
        if let Some(h) = hint {
            label_col = label_col.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.muted)
                    .child(h),
            );
        }

        row.child(label_col)
            .child(
                div()
                    .text_color(theme.accent)
                    .child(if selected { "✓" } else { "" }),
            )
            .into_any_element()
    }

    fn render_exclusions_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;

        let header_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .text_size(px(11.0))
            .text_color(theme.muted)
            .child(div().min_w(px(160.0)).child(tr("exclusions.col_app")))
            .child(div().flex_1().child(tr("exclusions.col_pattern")))
            .child(div().w(px(28.0)).child(""));

        let mut rows_col = div().flex().flex_col().gap_1();
        for i in 0..self.rules.len() {
            rows_col = rows_col.child(self.render_rule_row(i, cx));
        }

        let add_btn = div()
            .mt_2()
            .self_start()
            .px_3()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .text_color(theme.foreground)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.request_picker(PickerTarget::Exclusion(None), cx);
                }),
            )
            .child(tr("exclusions.add"));

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(theme.foreground)
                    .child(tr("exclusions.header")),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child(tr("exclusions.hint")),
            )
            .child(header_row)
            .child(rows_col)
            .child(add_btn)
            .into_any_element()
    }

    fn render_rule_row(&self, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let rule = &self.rules[i];

        let app_label: SharedString = if rule.app.is_empty() {
            tr("exclusions.any_app")
        } else {
            rule.app.clone().into()
        };
        let app_color = if rule.app.is_empty() {
            theme.muted
        } else {
            theme.foreground
        };

        let app_cell = div()
            .min_w(px(160.0))
            .px_2()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .text_color(app_color)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.request_picker(PickerTarget::Exclusion(Some(i)), cx);
                }),
            )
            .child(app_label);

        let editing = self.editing_title == Some(i);
        let pattern_border = if editing { theme.accent } else { theme.border };
        let pattern_text = &rule.title_pattern;
        let pattern_inner: AnyElement = if pattern_text.is_empty() && !editing {
            div()
                .text_color(theme.muted)
                .child(tr("exclusions.all"))
                .into_any_element()
        } else {
            let mut row = div().flex().flex_row().items_center();
            if !pattern_text.is_empty() {
                row = row.child(
                    div()
                        .text_color(theme.foreground)
                        .child(pattern_text.clone()),
                );
            }
            if editing {
                row = row.child(div().w(px(2.0)).h(px(18.0)).ml_0p5().bg(theme.accent));
            }
            row.into_any_element()
        };

        let pattern_cell = div()
            .flex_1()
            .px_2()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(pattern_border)
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.editing_title = Some(i);
                    cx.notify();
                }),
            )
            .child(pattern_inner);

        let remove_btn = div()
            .w(px(28.0))
            .h(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .text_color(theme.foreground)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    if i < this.rules.len() {
                        this.rules.remove(i);
                        if this.editing_title == Some(i) {
                            this.editing_title = None;
                        } else if let Some(e) = this.editing_title {
                            if e > i {
                                this.editing_title = Some(e - 1);
                            }
                        }
                        this.emit_changed(cx);
                    }
                }),
            )
            .child("−");

        let main_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(app_cell)
            .child(pattern_cell)
            .child(remove_btn);

        let mut col = div().flex().flex_col().gap_0p5().child(main_row);
        if let Some(err) = self.rule_errors.get(&i) {
            col = col.child(
                div()
                    .pl(px(172.0))
                    .text_size(px(11.0))
                    .text_color(theme.match_highlight)
                    .child(tr_sub("exclusions.invalid_regex", &[("err", err.as_str())])),
            );
        }
        col.into_any_element()
    }

    fn render_picker(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let filtered = self.filtered_picker_apps();
        let filtered_count = filtered.len();

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(18.0))
                    .text_color(theme.foreground)
                    .child(tr("picker.title")),
            )
            .child(
                div()
                    .px_3()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .text_color(theme.foreground)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, cx| this.close_picker(cx)),
                    )
                    .child(tr("picker.cancel")),
            );

        let search_box: SharedString = if self.picker_query.is_empty() {
            tr("picker.filter_placeholder")
        } else {
            self.picker_query.clone().into()
        };
        let search_color = if self.picker_query.is_empty() {
            theme.muted
        } else {
            theme.foreground
        };
        let search = div()
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(theme.accent)
            .bg(theme.selection)
            .text_color(search_color)
            .child(search_box);

        let list: AnyElement = if filtered_count == 0 {
            div()
                .px_3()
                .py_4()
                .text_color(theme.muted)
                .child(tr("picker.no_matches"))
                .into_any_element()
        } else {
            let mut col = div()
                .id("exclusions-picker-list")
                .flex()
                .flex_col()
                .flex_1()
                .overflow_y_scroll()
                .gap_0p5();
            for app_idx in filtered {
                let (name, bundle) = &self.picker_apps[app_idx];
                let name_str = name.clone();
                let subtitle = bundle.clone().unwrap_or_default();
                col = col.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .px_3()
                        .h(px(36.0))
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.selection))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                this.pick_app(name_str.clone(), cx);
                            }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_color(theme.foreground)
                                .child(name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(11.0))
                                .text_color(theme.muted)
                                .child(subtitle),
                        ),
                );
            }
            col.into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .p_6()
            .gap_4()
            .child(header)
            .child(search)
            .child(list)
    }
}

fn exceptions_summary(list: &[AppMatch]) -> String {
    if list.is_empty() {
        return _tr("settings.exceptions_none");
    }
    let shown = list.iter().take(2).map(|m| m.as_str()).collect::<Vec<_>>().join(", ");
    let extra = list.len().saturating_sub(2);
    if extra == 0 {
        shown
    } else {
        format!("{shown}  +{extra}")
    }
}

fn shortcut_row(
    label: &SharedString,
    recording: bool,
    theme: &Theme,
    on_record: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let border = if recording { theme.accent } else { theme.border };
    let button_label = if recording {
        tr("settings.cancel_record")
    } else {
        tr("settings.record")
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .child(
            div()
                .min_w(px(140.0))
                .text_color(theme.muted)
                .child(tr("settings.shortcut_label")),
        )
        .child(
            div()
                .flex_1()
                .px_3()
                .py_1p5()
                .rounded_md()
                .border_1()
                .border_color(border)
                .bg(theme.selection)
                .text_color(theme.foreground)
                .child(label.clone()),
        )
        .child(
            div()
                .px_3()
                .py_1p5()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, on_record)
                .text_color(theme.foreground)
                .child(button_label),
        )
}

fn sort_order_label(order: SortOrder) -> SharedString {
    match order {
        SortOrder::RecentApp => tr("sort.recent_app"),
        SortOrder::RecentWindow => tr("sort.recent_window"),
        SortOrder::Title => tr("sort.title"),
        SortOrder::AppName => tr("sort.app_name"),
    }
}

fn sort_order_hint(order: SortOrder) -> Option<SharedString> {
    match order {
        SortOrder::RecentApp => Some(tr("sort.recent_app_hint")),
        SortOrder::RecentWindow => Some(tr("sort.recent_window_hint")),
        SortOrder::Title => None,
        SortOrder::AppName => None,
    }
}

fn toggle_row(
    label: SharedString,
    hint: Option<SharedString>,
    on: bool,
    theme: &Theme,
    on_toggle: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let track_bg = if on { theme.accent } else { theme.border };

    let mut track = div()
        .w(px(36.0))
        .h(px(20.0))
        .rounded_full()
        .bg(track_bg)
        .flex()
        .items_center()
        .px(px(2.0));
    track = if on { track.justify_end() } else { track.justify_start() };
    let track = track.child(
        div()
            .w(px(16.0))
            .h(px(16.0))
            .rounded_full()
            .bg(gpui::rgb(0xffffff)),
    );

    let mut text = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap_0p5()
        .child(
            div()
                .text_color(theme.foreground)
                .child(label),
        );
    if let Some(h) = hint {
        text = text.child(
            div()
                .text_size(px(12.0))
                .text_color(theme.muted)
                .child(h),
        );
    }

    div()
        .flex()
        .flex_row()
        .items_start()
        .gap_3()
        .w_full()
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, on_toggle)
        .child(track)
        .child(text)
}

/// Convert a raw GPUI keystroke into our persisted `HotkeySpec`. Returns `None`
/// if the keystroke is a pure modifier press (no actual key) or if it has no
/// modifier at all (we refuse to bind plain keys — they'd shadow typing
/// everywhere system-wide).
fn keystroke_to_spec(k: &Keystroke) -> Option<HotkeySpec> {
    let mut modifiers = Vec::new();
    if k.modifiers.control {
        modifiers.push("ctrl".to_string());
    }
    if k.modifiers.alt {
        modifiers.push("alt".to_string());
    }
    if k.modifiers.shift {
        modifiers.push("shift".to_string());
    }
    if k.modifiers.platform {
        modifiers.push("cmd".to_string());
    }

    if modifiers.is_empty() {
        return None;
    }

    let key = k.key.to_lowercase();
    if matches!(
        key.as_str(),
        "shift" | "control" | "alt" | "platform" | "function" | "" | "escape"
    ) {
        return None;
    }

    Some(HotkeySpec { modifiers, key })
}

fn format_spec(spec: &HotkeySpec) -> String {
    // Modifiers use OS-native symbols (⌘⌃⌥⇧ on macOS) concatenated tightly,
    // then a space and the key — matching how macOS menu bars render shortcuts.
    let mut mods = String::new();
    for m in &spec.modifiers {
        mods.push_str(modifier_symbol(m));
    }
    if mods.is_empty() {
        pretty_key(&spec.key)
    } else {
        format!("{mods} {}", pretty_key(&spec.key))
    }
}

fn pretty_key(k: &str) -> String {
    match k {
        "space" => _tr("keys.space"),
        "tab" => _tr("keys.tab"),
        "return" | "enter" => _tr("keys.enter"),
        "escape" | "esc" => _tr("keys.esc"),
        other if other.len() == 1 => other.to_ascii_uppercase(),
        other => other.to_string(),
    }
}
