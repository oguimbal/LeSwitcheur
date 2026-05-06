//! Keyboard actions dispatched to [`SwitcherView`].
//!
//! Text-edit actions (Backspace, Delete, MoveLeft, etc.) are owned by
//! `gpui_component::Input` once the search field has focus. We only keep
//! actions whose semantics depend on the surrounding switcher state
//! (selection, popover, audio toggle, pane focus).

use gpui::actions;

actions!(
    switcheur,
    [
        /// Move list selection up, wrapping at the top.
        SelectPrev,
        /// Move list selection down, wrapping at the bottom.
        SelectNext,
        /// Activate the selected item.
        Confirm,
        /// Hide the switcher without acting.
        Dismiss,
        /// Cursor left, but with extra semantics: exits the Open With
        /// popover or the Dirs pane when those have focus. Falls through
        /// to the Input widget's caret motion otherwise.
        MoveLeft,
        /// Cursor right, but with extra semantics: enters the Open With
        /// popover from the Dirs pane, or focuses the Dirs pane when the
        /// caret is at the end of the input. Falls through to the Input
        /// widget's caret motion otherwise.
        MoveRight,
        /// Space pressed while the Currently Playing row is selected:
        /// toggle play/pause instead of inserting a literal space.
        AudioToggle,
        /// Move keyboard focus to the next pane (Windows → Dirs).
        FocusNextPane,
        /// Move keyboard focus to the previous pane (Dirs → Windows).
        FocusPrevPane,
    ]
);
