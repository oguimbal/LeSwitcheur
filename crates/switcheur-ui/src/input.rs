//! Query input with cursor and selection.
//!
//! Not a full IME-aware TextElement (that would require implementing
//! `EntityInputHandler` the way `gpui/examples/input.rs` does). This is a
//! pragmatic middle ground: caret position, anchor-based selection,
//! word-unaware left/right motion, Home/End, SelectAll — enough to feel
//! like a normal single-line input for a quick switcher.

use std::ops::Range;

#[derive(Debug, Clone, Default)]
pub struct QueryInput {
    text: String,
    /// Byte offset of the caret. Always on a char boundary.
    cursor: usize,
    /// Byte offset of the selection anchor, if the user is selecting.
    /// Selection runs between `anchor` and `cursor` (either direction).
    anchor: Option<usize>,
}

impl QueryInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn selection(&self) -> Option<Range<usize>> {
        self.anchor.and_then(|a| {
            if a == self.cursor {
                None
            } else if a < self.cursor {
                Some(a..self.cursor)
            } else {
                Some(self.cursor..a)
            }
        })
    }

    pub fn has_selection(&self) -> bool {
        self.selection().is_some()
    }

    pub fn insert_str(&mut self, s: &str) {
        if let Some(sel) = self.selection() {
            self.text.replace_range(sel.clone(), s);
            self.cursor = sel.start + s.len();
        } else {
            self.text.insert_str(self.cursor, s);
            self.cursor += s.len();
        }
        self.anchor = None;
    }

    pub fn backspace(&mut self) {
        if let Some(sel) = self.selection() {
            self.text.replace_range(sel.clone(), "");
            self.cursor = sel.start;
        } else if self.cursor > 0 {
            let prev = prev_boundary(&self.text, self.cursor);
            self.text.replace_range(prev..self.cursor, "");
            self.cursor = prev;
        }
        self.anchor = None;
    }

    pub fn delete(&mut self) {
        if let Some(sel) = self.selection() {
            self.text.replace_range(sel.clone(), "");
            self.cursor = sel.start;
        } else if self.cursor < self.text.len() {
            let next = next_boundary(&self.text, self.cursor);
            self.text.replace_range(self.cursor..next, "");
        }
        self.anchor = None;
    }

    pub fn move_left(&mut self, extend: bool) {
        if !extend {
            if let Some(sel) = self.selection() {
                self.cursor = sel.start;
                self.anchor = None;
                return;
            }
            self.anchor = None;
        } else {
            self.anchor.get_or_insert(self.cursor);
        }
        self.cursor = prev_boundary(&self.text, self.cursor);
    }

    pub fn move_right(&mut self, extend: bool) {
        if !extend {
            if let Some(sel) = self.selection() {
                self.cursor = sel.end;
                self.anchor = None;
                return;
            }
            self.anchor = None;
        } else {
            self.anchor.get_or_insert(self.cursor);
        }
        self.cursor = next_boundary(&self.text, self.cursor);
    }

    pub fn move_word_left(&mut self, extend: bool) {
        if !extend {
            if let Some(sel) = self.selection() {
                self.cursor = sel.start;
                self.anchor = None;
                return;
            }
            self.anchor = None;
        } else {
            self.anchor.get_or_insert(self.cursor);
        }
        self.cursor = prev_word_boundary(&self.text, self.cursor);
    }

    pub fn move_word_right(&mut self, extend: bool) {
        if !extend {
            if let Some(sel) = self.selection() {
                self.cursor = sel.end;
                self.anchor = None;
                return;
            }
            self.anchor = None;
        } else {
            self.anchor.get_or_insert(self.cursor);
        }
        self.cursor = next_word_boundary(&self.text, self.cursor);
    }

    pub fn move_home(&mut self, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = 0;
    }

    pub fn move_end(&mut self, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = self.text.len();
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Kept for compat with older call sites.
    pub fn push_str(&mut self, s: &str) {
        self.insert_str(s);
    }

    /// Convenience for callers that want the currently selected slice.
    pub fn selected_text(&self) -> Option<&str> {
        self.selection().map(|r| &self.text[r])
    }
}

fn prev_boundary(s: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut p = pos - 1;
    while p > 0 && !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

fn next_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos.saturating_add(1).min(s.len());
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Walk backward: skip non-word chars, then skip word chars. Lands on the
/// start of the previous word (or 0).
fn prev_word_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos;
    while p > 0 {
        let prev = prev_boundary(s, p);
        let c = s[prev..p].chars().next().unwrap();
        if is_word_char(c) {
            break;
        }
        p = prev;
    }
    while p > 0 {
        let prev = prev_boundary(s, p);
        let c = s[prev..p].chars().next().unwrap();
        if !is_word_char(c) {
            break;
        }
        p = prev;
    }
    p
}

/// Walk forward: skip non-word chars, then skip word chars. Lands on the
/// byte past the current/next word (or text.len()).
fn next_word_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos;
    while p < s.len() {
        let next = next_boundary(s, p);
        let c = s[p..next].chars().next().unwrap();
        if is_word_char(c) {
            break;
        }
        p = next;
    }
    while p < s.len() {
        let next = next_boundary(s, p);
        let c = s[p..next].chars().next().unwrap();
        if !is_word_char(c) {
            break;
        }
        p = next;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_appends() {
        let mut q = QueryInput::new();
        q.insert_str("ab");
        q.insert_str("c");
        assert_eq!(q.text(), "abc");
        assert_eq!(q.cursor(), 3);
    }

    #[test]
    fn backspace_deletes_prev_char() {
        let mut q = QueryInput::new();
        q.insert_str("abc");
        q.backspace();
        assert_eq!(q.text(), "ab");
    }

    #[test]
    fn move_left_right_clamped() {
        let mut q = QueryInput::new();
        q.insert_str("abc");
        q.move_left(false);
        q.move_left(false);
        q.move_left(false);
        q.move_left(false); // no-op at start
        assert_eq!(q.cursor(), 0);
        q.move_right(false);
        assert_eq!(q.cursor(), 1);
    }

    #[test]
    fn selection_replace_on_insert() {
        let mut q = QueryInput::new();
        q.insert_str("hello");
        q.move_home(false);
        q.move_right(true);
        q.move_right(true); // select "he"
        q.insert_str("X");
        assert_eq!(q.text(), "Xllo");
        assert_eq!(q.cursor(), 1);
    }

    #[test]
    fn select_all_then_backspace_clears() {
        let mut q = QueryInput::new();
        q.insert_str("hello");
        q.select_all();
        q.backspace();
        assert_eq!(q.text(), "");
        assert_eq!(q.cursor(), 0);
    }

    #[test]
    fn unicode_boundaries() {
        let mut q = QueryInput::new();
        q.insert_str("é"); // 2 bytes
        assert_eq!(q.cursor(), 2);
        q.move_left(false);
        assert_eq!(q.cursor(), 0);
        q.move_right(false);
        assert_eq!(q.cursor(), 2);
        q.backspace();
        assert_eq!(q.text(), "");
    }
}
