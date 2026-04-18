//! Shared "this app is X" matcher, reused by features that need to enable or
//! disable themselves based on the frontmost application.
//!
//! An [`AppMatch`] is a single string that compares case-insensitively against
//! either the app's localized name (e.g. `Safari`) or its bundle identifier
//! (e.g. `com.apple.Safari`). [`AppMatchSet`] pre-lowercases the list once and
//! offers a fast `any_match` lookup for use in hot paths (HID event taps,
//! hotkey callbacks).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(transparent)]
pub struct AppMatch(pub String);

impl AppMatch {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Case-insensitive match against either `name` or `bundle_id`. An empty
    /// matcher string never matches — distinct from the wildcard semantics in
    /// [`crate::ExclusionRule`], because these exclusions are always app-scoped.
    pub fn matches(&self, name: &str, bundle_id: Option<&str>) -> bool {
        if self.0.is_empty() {
            return false;
        }
        let needle = self.0.to_lowercase();
        if name.eq_ignore_ascii_case(&needle) {
            return true;
        }
        if let Some(b) = bundle_id {
            if b.eq_ignore_ascii_case(&needle) {
                return true;
            }
        }
        false
    }
}

impl From<String> for AppMatch {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for AppMatch {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Pre-lowercased snapshot of an [`AppMatch`] list, cheap to check per event.
#[derive(Debug, Clone, Default)]
pub struct AppMatchSet {
    lowered: Vec<String>,
}

impl AppMatchSet {
    pub fn compile(items: &[AppMatch]) -> Self {
        let lowered = items
            .iter()
            .filter(|m| !m.0.is_empty())
            .map(|m| m.0.to_lowercase())
            .collect();
        Self { lowered }
    }

    pub fn is_empty(&self) -> bool {
        self.lowered.is_empty()
    }

    pub fn any_match(&self, name: &str, bundle_id: Option<&str>) -> bool {
        if self.lowered.is_empty() {
            return false;
        }
        for needle in &self.lowered {
            if name.eq_ignore_ascii_case(needle) {
                return true;
            }
            if let Some(b) = bundle_id {
                if b.eq_ignore_ascii_case(needle) {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_by_name_case_insensitive() {
        let m = AppMatch::new("Safari");
        assert!(m.matches("safari", None));
        assert!(m.matches("SAFARI", None));
        assert!(!m.matches("Chrome", None));
    }

    #[test]
    fn matches_by_bundle_id() {
        let m = AppMatch::new("com.apple.Safari");
        assert!(m.matches("Anything", Some("com.apple.safari")));
        assert!(!m.matches("Anything", Some("com.apple.Foo")));
        assert!(!m.matches("Anything", None));
    }

    #[test]
    fn empty_never_matches() {
        let m = AppMatch::new("");
        assert!(!m.matches("Safari", Some("com.apple.Safari")));
    }

    #[test]
    fn set_any_match() {
        let set = AppMatchSet::compile(&[
            AppMatch::new("Safari"),
            AppMatch::new("com.mitchellh.ghostty"),
        ]);
        assert!(set.any_match("Safari", None));
        assert!(set.any_match("Ghostty", Some("com.mitchellh.ghostty")));
        assert!(!set.any_match("Chrome", Some("com.google.Chrome")));
    }

    #[test]
    fn set_empty_short_circuits() {
        let set = AppMatchSet::compile(&[]);
        assert!(set.is_empty());
        assert!(!set.any_match("Safari", None));
    }

    #[test]
    fn empty_strings_filtered_from_set() {
        let set = AppMatchSet::compile(&[AppMatch::new(""), AppMatch::new("Safari")]);
        assert!(!set.is_empty());
        assert!(set.any_match("Safari", None));
    }

    #[test]
    fn serde_transparent_roundtrip() {
        let list = vec![AppMatch::new("Safari"), AppMatch::new("com.apple.Finder")];
        let text = toml::to_string(&TomlWrapper { items: list.clone() }).unwrap();
        assert!(text.contains("\"Safari\""));
        assert!(text.contains("\"com.apple.Finder\""));
        let back: TomlWrapper = toml::from_str(&text).unwrap();
        assert_eq!(back.items, list);
    }

    #[derive(Serialize, Deserialize)]
    struct TomlWrapper {
        items: Vec<AppMatch>,
    }
}
