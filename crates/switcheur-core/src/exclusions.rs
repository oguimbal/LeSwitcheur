//! Exclusion rules for hiding specific apps/windows from the switcher.
//!
//! Rule shape (see `ExclusionRule`): optional app (matched exact, case-insensitive,
//! against `app_name` OR `bundle_id`) combined AND-style with an optional regex on
//! the window title. Empty fields are wildcards. Multiple rules OR together.
//!
//! Invalid regexes are reported by `compile` and silently skipped at match time —
//! the UI surfaces the error inline so the user can fix it.

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::model::{AppRef, WindowRef};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ExclusionRule {
    /// Exact, case-insensitive match against `WindowRef::app_name` OR
    /// `WindowRef::bundle_id`. Empty string = wildcard (any app).
    #[serde(default)]
    pub app: String,
    /// Regex applied to `WindowRef::title`. Substring by default (no anchors).
    /// Empty string = wildcard (any title). Case-insensitive by default via an
    /// injected `(?i)` prefix; defeat with `(?-i)` in the pattern.
    #[serde(default)]
    pub title_pattern: String,
}

pub struct ExclusionFilter {
    compiled: Vec<CompiledRule>,
}

struct CompiledRule {
    app: String,
    title_re: Option<Regex>,
}

impl ExclusionFilter {
    pub fn empty() -> Self {
        Self {
            compiled: Vec::new(),
        }
    }

    /// Returns the filter plus a list of `(rule_index, regex_error)` for any
    /// rule whose `title_pattern` failed to compile. Broken rules are dropped
    /// from the filter; everything else keeps working.
    pub fn compile(rules: &[ExclusionRule]) -> (Self, Vec<(usize, regex::Error)>) {
        let mut compiled = Vec::with_capacity(rules.len());
        let mut errors = Vec::new();
        for (idx, r) in rules.iter().enumerate() {
            let title_re = if r.title_pattern.is_empty() {
                None
            } else {
                let pat = format!("(?i){}", r.title_pattern);
                match Regex::new(&pat) {
                    Ok(re) => Some(re),
                    Err(e) => {
                        errors.push((idx, e));
                        continue;
                    }
                }
            };
            compiled.push(CompiledRule {
                app: r.app.to_lowercase(),
                title_re,
            });
        }
        (Self { compiled }, errors)
    }

    pub fn is_excluded_window(&self, w: &WindowRef) -> bool {
        self.compiled.iter().any(|r| window_matches(r, w))
    }

    /// App-level entries (when `include_apps = true`) are only excluded by rules
    /// that have NO title pattern — those explicitly target windows.
    pub fn is_excluded_app(&self, a: &AppRef) -> bool {
        self.compiled
            .iter()
            .any(|r| r.title_re.is_none() && app_field_matches_app(r, a))
    }
}

fn window_matches(r: &CompiledRule, w: &WindowRef) -> bool {
    if !r.app.is_empty() {
        let name_hit = w.app_name.eq_ignore_ascii_case(&r.app);
        let bid_hit = w
            .bundle_id
            .as_deref()
            .map(|b| b.eq_ignore_ascii_case(&r.app))
            .unwrap_or(false);
        if !name_hit && !bid_hit {
            return false;
        }
    }
    match &r.title_re {
        Some(re) => re.is_match(&w.title),
        None => true,
    }
}

fn app_field_matches_app(r: &CompiledRule, a: &AppRef) -> bool {
    if r.app.is_empty() {
        return false;
    }
    let name_hit = a.name.eq_ignore_ascii_case(&r.app);
    let bid_hit = a
        .bundle_id
        .as_deref()
        .map(|b| b.eq_ignore_ascii_case(&r.app))
        .unwrap_or(false);
    name_hit || bid_hit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(app: &str, bundle: Option<&str>, title: &str) -> WindowRef {
        WindowRef {
            id: 1,
            pid: 1,
            title: title.to_string(),
            app_name: app.to_string(),
            bundle_id: bundle.map(str::to_string),
            icon_path: None,
            minimized: false,
        }
    }

    fn app_ref(name: &str, bundle: Option<&str>) -> AppRef {
        AppRef {
            pid: 1,
            name: name.to_string(),
            bundle_id: bundle.map(str::to_string),
            icon_path: None,
        }
    }

    fn filter(rules: &[ExclusionRule]) -> ExclusionFilter {
        let (f, errs) = ExclusionFilter::compile(rules);
        assert!(errs.is_empty(), "unexpected compile errors: {errs:?}");
        f
    }

    #[test]
    fn excludes_by_app_name_only() {
        let f = filter(&[ExclusionRule {
            app: "Ghostty".into(),
            title_pattern: String::new(),
        }]);
        assert!(f.is_excluded_window(&win("Ghostty", None, "~/code")));
        assert!(f.is_excluded_window(&win("Ghostty", None, "")));
        assert!(!f.is_excluded_window(&win("Safari", None, "GitHub")));
    }

    #[test]
    fn excludes_by_regex_substring() {
        let f = filter(&[ExclusionRule {
            app: "Safari".into(),
            title_pattern: "Private".into(),
        }]);
        assert!(f.is_excluded_window(&win("Safari", None, "Private Browsing — Safari")));
        assert!(!f.is_excluded_window(&win("Safari", None, "GitHub — Safari")));
    }

    #[test]
    fn regex_anchored_caret() {
        let f = filter(&[ExclusionRule {
            app: String::new(),
            title_pattern: "^Private".into(),
        }]);
        assert!(f.is_excluded_window(&win("Safari", None, "Private Browsing")));
        assert!(!f.is_excluded_window(&win("Safari", None, "Not Private")));
    }

    #[test]
    fn app_matches_bundle_id() {
        let f = filter(&[ExclusionRule {
            app: "com.apple.Safari".into(),
            title_pattern: String::new(),
        }]);
        assert!(f.is_excluded_window(&win("Safari", Some("com.apple.Safari"), "anything")));
        assert!(!f.is_excluded_window(&win("Safari", Some("com.apple.Foo"), "anything")));
    }

    #[test]
    fn empty_title_pattern_is_wildcard() {
        let f = filter(&[ExclusionRule {
            app: "Ghostty".into(),
            title_pattern: String::new(),
        }]);
        assert!(f.is_excluded_window(&win("Ghostty", None, "")));
        assert!(f.is_excluded_window(&win("Ghostty", None, "whatever")));
    }

    #[test]
    fn empty_app_with_title_regex() {
        let f = filter(&[ExclusionRule {
            app: String::new(),
            title_pattern: "zoom meeting".into(),
        }]);
        assert!(f.is_excluded_window(&win("zoom.us", None, "Zoom Meeting")));
        assert!(f.is_excluded_window(&win("Chrome", None, "join zoom meeting")));
        assert!(!f.is_excluded_window(&win("Safari", None, "GitHub")));
    }

    #[test]
    fn invalid_regex_is_reported_and_skipped() {
        // Rule 0 has an invalid regex and must be dropped; rule 1 must still
        // apply unaffected.
        let (f, errs) = ExclusionFilter::compile(&[
            ExclusionRule {
                app: "Cursed".into(),
                title_pattern: "[".into(),
            },
            ExclusionRule {
                app: "Safari".into(),
                title_pattern: String::new(),
            },
        ]);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].0, 0);
        // The broken rule would have targeted Cursed windows — they aren't excluded.
        assert!(!f.is_excluded_window(&win("Cursed", None, "[suspicious]")));
        // The other rule keeps working.
        assert!(f.is_excluded_window(&win("Safari", None, "anything")));
    }

    #[test]
    fn case_insensitive_by_default() {
        let f = filter(&[ExclusionRule {
            app: String::new(),
            title_pattern: "private".into(),
        }]);
        assert!(f.is_excluded_window(&win("Safari", None, "Private Browsing")));
    }

    #[test]
    fn app_entry_excluded_only_without_pattern() {
        let f_with_pattern = filter(&[ExclusionRule {
            app: "Safari".into(),
            title_pattern: "^X".into(),
        }]);
        assert!(!f_with_pattern.is_excluded_app(&app_ref("Safari", Some("com.apple.Safari"))));

        let f_no_pattern = filter(&[ExclusionRule {
            app: "Safari".into(),
            title_pattern: String::new(),
        }]);
        assert!(f_no_pattern.is_excluded_app(&app_ref("Safari", Some("com.apple.Safari"))));
        assert!(!f_no_pattern.is_excluded_app(&app_ref("Ghostty", None)));
    }

    #[test]
    fn multiple_rules_or_together() {
        let f = filter(&[
            ExclusionRule {
                app: "Ghostty".into(),
                title_pattern: String::new(),
            },
            ExclusionRule {
                app: "Safari".into(),
                title_pattern: "^Private".into(),
            },
        ]);
        assert!(f.is_excluded_window(&win("Ghostty", None, "any")));
        assert!(f.is_excluded_window(&win("Safari", None, "Private Browsing")));
        assert!(!f.is_excluded_window(&win("Safari", None, "GitHub")));
    }
}
