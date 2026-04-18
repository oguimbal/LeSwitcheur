//! Thin wrapper over `nucleo-matcher` that returns scores + match indices
//! for highlighting. Kept focused on the switcher's needs — not a general
//! reimplementation of the nucleo API.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::model::Item;

/// One matched candidate, ready to display.
#[derive(Debug, Clone)]
pub struct MatchResult {
    pub item: Item,
    pub score: u32,
    /// Indices (char offsets into `Item::search_text`) that matched the query.
    pub indices: Vec<u32>,
}

/// Stateful matcher so per-query allocations (scratch buffers) get reused.
pub struct FuzzyMatcher {
    matcher: Matcher,
}

impl FuzzyMatcher {
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    /// Rank `items` against `query`. Empty query → preserve input order, no filtering.
    pub fn rank(&mut self, query: &str, items: &[Item]) -> Vec<MatchResult> {
        if query.trim().is_empty() {
            return items
                .iter()
                .cloned()
                .map(|item| MatchResult {
                    item,
                    score: 0,
                    indices: Vec::new(),
                })
                .collect();
        }

        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);

        let mut haystack_buf = Vec::new();
        let mut indices = Vec::new();
        let mut out = Vec::with_capacity(items.len());

        for item in items {
            let text = item.search_text();
            let haystack = Utf32Str::new(&text, &mut haystack_buf);
            indices.clear();
            if let Some(score) = pattern.indices(haystack, &mut self.matcher, &mut indices) {
                out.push(MatchResult {
                    item: item.clone(),
                    score: score as u32,
                    indices: indices.clone(),
                });
            }
        }

        out.sort_by(|a, b| b.score.cmp(&a.score));
        out
    }
}

impl Default for FuzzyMatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AppRef, WindowRef};
    use std::sync::Arc;

    fn window(app: &str, title: &str) -> Item {
        Item::Window(Arc::new(WindowRef {
            id: 0,
            pid: 0,
            title: title.into(),
            app_name: app.into(),
            bundle_id: None,
            icon_path: None,
            minimized: false,
        }))
    }

    #[test]
    fn empty_query_returns_all_in_order() {
        let mut m = FuzzyMatcher::new();
        let items = vec![window("Safari", "Hacker News"), window("Mail", "Inbox")];
        let out = m.rank("", &items);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].item.primary(), "Hacker News");
    }

    #[test]
    fn fuzzy_matches_by_subsequence() {
        let mut m = FuzzyMatcher::new();
        let items = vec![
            window("Safari", "Hacker News"),
            window("Mail", "Inbox — gmail"),
            window("Code", "leswitcheur — src"),
        ];
        let out = m.rank("lesw", &items);
        assert!(!out.is_empty());
        assert_eq!(out[0].item.primary(), "leswitcheur — src");
        assert!(!out[0].indices.is_empty());
    }

    #[test]
    fn apps_are_searchable() {
        let mut m = FuzzyMatcher::new();
        let items = vec![
            Item::App(Arc::new(AppRef {
                pid: 1,
                name: "Calculator".into(),
                bundle_id: None,
                icon_path: None,
            })),
            window("Mail", "Inbox"),
        ];
        let out = m.rank("calc", &items);
        assert_eq!(out[0].item.primary(), "Calculator");
    }

    #[test]
    fn non_matching_items_are_filtered() {
        let mut m = FuzzyMatcher::new();
        let items = vec![window("Safari", "Hacker News"), window("Mail", "Inbox")];
        let out = m.rank("xyz123nomatch", &items);
        assert!(out.is_empty());
    }
}
