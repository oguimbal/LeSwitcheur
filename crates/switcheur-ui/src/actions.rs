//! Keyboard actions dispatched to [`SwitcherView`].

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
        /// Delete one char before the cursor (or the selection).
        Backspace,
        /// Delete one char after the cursor (or the selection).
        Delete,
        /// Cursor left.
        MoveLeft,
        /// Cursor right.
        MoveRight,
        /// Cursor home.
        MoveHome,
        /// Cursor end.
        MoveEnd,
        /// Extend selection left.
        ExtendLeft,
        /// Extend selection right.
        ExtendRight,
        /// Extend selection to home.
        ExtendHome,
        /// Extend selection to end.
        ExtendEnd,
        /// Cursor to the start of the previous word.
        MoveWordLeft,
        /// Cursor to the end of the next word.
        MoveWordRight,
        /// Extend selection to the start of the previous word.
        ExtendWordLeft,
        /// Extend selection to the end of the next word.
        ExtendWordRight,
        /// Select entire query.
        SelectAll,
        /// Copy current selection to the system clipboard.
        Copy,
        /// Cut current selection to the system clipboard.
        Cut,
        /// Insert clipboard text at the cursor (replacing the selection).
        Paste,
        /// Move keyboard focus to the next pane (Windows → Dirs).
        FocusNextPane,
        /// Move keyboard focus to the previous pane (Dirs → Windows).
        FocusPrevPane,
    ]
);
