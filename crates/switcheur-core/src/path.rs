//! Absolute paths typed in the query (`/a/b`, `~/a`, `C:\a`, `C:/a`), offered
//! as openable file/folder rows.

use std::path::{Path, PathBuf};

/// `" '/a/my\ dir' "` → `Some("/a/my dir")`. Accepts what a terminal or a
/// Finder drag pastes: surrounding quotes, backslash-escaped spaces.
pub fn detect(query: &str) -> Option<String> {
    let mut s = query.trim();
    for quote in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(quote) && s.ends_with(quote) {
            s = &s[1..s.len() - 1];
        }
    }
    if s.contains('\n') {
        return None;
    }
    if s.starts_with('/') || s == "~" || s.starts_with("~/") {
        return Some(s.replace("\\ ", " "));
    }
    let b = s.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
    {
        return Some(s.to_string());
    }
    None
}

/// What exists for a detected path.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Hits {
    /// The path itself (`~` expanded) and its `is_dir`, when it exists.
    pub exact: Option<(PathBuf, bool)>,
    /// Entries of its folder whose name starts with the last component, or
    /// its children when the path ends with a separator. Sorted, hidden
    /// entries only for a `.` prefix. `(path, is_dir)`.
    pub entries: Vec<(PathBuf, bool)>,
}

impl Hits {
    /// Rows to list: the path itself, then the entries.
    pub fn rows(&self) -> impl Iterator<Item = &(PathBuf, bool)> {
        self.exact.iter().chain(&self.entries)
    }
}

/// Look `target` up on disk, keeping at most `limit` entries.
pub fn hits(target: &str, home: Option<&Path>, limit: usize) -> Hits {
    let expanded = match (target.strip_prefix('~'), home) {
        (Some(rest), Some(home)) => format!("{}{rest}", home.display()),
        (Some(_), None) => return Hits::default(),
        (None, _) => target.to_string(),
    };
    let path = PathBuf::from(&expanded);
    let mut out = Hits {
        exact: std::fs::metadata(&path).ok().map(|m| (path.clone(), m.is_dir())),
        entries: Vec::new(),
    };
    let ends_with_sep = expanded.ends_with('/') || expanded.ends_with('\\');
    let (dir, prefix) = if ends_with_sep {
        if !matches!(out.exact, Some((_, true))) {
            return out;
        }
        (path.clone(), String::new())
    } else if target == "~" {
        return out;
    } else {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return out;
        };
        (parent.to_path_buf(), name.to_string_lossy().to_lowercase())
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut siblings: Vec<(String, PathBuf, bool)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let lower = name.to_lowercase();
            if !lower.starts_with(&prefix) || lower == prefix {
                return None;
            }
            if name.starts_with('.') && !prefix.starts_with('.') {
                return None;
            }
            let is_dir = std::fs::metadata(entry.path()).map(|m| m.is_dir()).ok()?;
            Some((lower, entry.path(), is_dir))
        })
        .collect();
    siblings.sort_by(|a, b| a.0.cmp(&b.0));
    siblings.truncate(limit);
    out.entries = siblings.into_iter().map(|(_, path, is_dir)| (path, is_dir)).collect();
    out
}

/// Queries Tab cycles through for `target`, shell style: the typed folder
/// part kept as typed, the last component replaced by each candidate's real
/// name. No separator is appended to folders: typing it is how the user
/// steps into the chosen one.
pub fn completions(target: &str, hits: &Hits) -> Vec<String> {
    let Some(sep_at) = target.rfind(['/', '\\']) else {
        return Vec::new();
    };
    let base = &target[..=sep_at];
    hits.entries
        .iter()
        .filter_map(|(path, _)| Some(format!("{base}{}", path.file_name()?.to_string_lossy())))
        .collect()
}

/// Grey suffix shown after `query` for the first completion, when that
/// completion extends what is typed (case-insensitively).
pub fn ghost<'a>(query: &str, completions: &'a [String]) -> Option<&'a str> {
    let first = completions.first()?;
    let head = first.get(..query.len())?;
    (first.len() > query.len() && head.eq_ignore_ascii_case(query)).then(|| &first[query.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_paths_detected() {
        assert_eq!(detect("/Users/me"), Some("/Users/me".into()));
        assert_eq!(detect(" ~/repos "), Some("~/repos".into()));
        assert_eq!(detect("~"), Some("~".into()));
        assert_eq!(detect("C:\\Users\\me"), Some("C:\\Users\\me".into()));
        assert_eq!(detect("d:/data"), Some("d:/data".into()));
        assert_eq!(detect("'/a/my dir'"), Some("/a/my dir".into()));
        assert_eq!(detect("/a/my\\ dir"), Some("/a/my dir".into()));
    }

    #[test]
    fn other_queries_rejected() {
        for q in ["", "safari", "~me", "a/b", "C:", "Cd:/x", "https://x.y", "1/2"] {
            assert_eq!(detect(q), None, "{q}");
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("switcheur-path-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Repos")).unwrap();
        std::fs::create_dir_all(dir.join("repos-my")).unwrap();
        std::fs::write(dir.join("readme.md"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        dir
    }

    #[test]
    fn exact_path_then_prefix_siblings() {
        let dir = scratch("prefix");
        let target = format!("{}/repos", dir.display());
        let hits = hits(&target, None, 8);
        let names: Vec<_> = hits
            .rows()
            .map(|(p, d)| (p.file_name().unwrap().to_string_lossy().into_owned(), *d))
            .collect();
        // Case-insensitive filesystems resolve `repos` to `Repos`.
        if names.first().map(|n| n.0.as_str()) == Some("repos") {
            assert_eq!(names[1..], [("repos-my".to_string(), true)]);
        } else {
            assert!(names.contains(&("repos-my".to_string(), true)));
            assert!(names.contains(&("Repos".to_string(), true)));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trailing_separator_lists_children_without_hidden() {
        let dir = scratch("children");
        let hits = hits(&format!("{}/", dir.display()), None, 8);
        assert!(matches!(hits.exact, Some((_, true))));
        assert_eq!(hits.entries.len(), 3, "{hits:?}");
        assert!(hits.entries.iter().all(|(p, _)| !p.ends_with(".hidden")));
        assert!(hits.entries.iter().any(|(p, d)| p.ends_with("readme.md") && !d));
        let hidden = super::hits(&format!("{}/.h", dir.display()), None, 8);
        assert_eq!(hidden.entries.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tilde_expands_and_missing_paths_are_empty() {
        let dir = scratch("tilde");
        let hits = hits("~/readme.md", Some(&dir), 8);
        assert_eq!(hits.rows().cloned().collect::<Vec<_>>(), vec![(dir.join("readme.md"), false)]);
        assert_eq!(super::hits("/definitely/not/here", None, 8), Hits::default());
        assert_eq!(super::hits("~/x", None, 8), Hits::default());
        assert_eq!(super::hits(&format!("{}/re", dir.display()), None, 1).entries.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn completions_follow_the_shell() {
        let dir = scratch("complete");
        let base = dir.display().to_string();
        let typed = format!("{base}/rE");
        let c = completions(&typed, &hits(&typed, None, 8));
        assert_eq!(c, vec![format!("{base}/readme.md"), format!("{base}/Repos"), format!("{base}/repos-my")]);
        assert_eq!(ghost(&typed, &c), Some("adme.md"));
        let typed = format!("{base}/repos-my");
        assert!(completions(&typed, &hits(&typed, None, 8)).is_empty());
        let c = completions(&format!("{base}/"), &hits(&format!("{base}/"), None, 8));
        assert_eq!(c.len(), 3);
        assert!(completions("~", &Hits { exact: Some((dir.clone(), true)), entries: vec![] }).is_empty());
        assert_eq!(ghost("/x/abc", &["/x/ab".to_string()]), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
