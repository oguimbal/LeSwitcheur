//! URL detection for the launcher. The query is classified as a URL when it
//! has an `http://` or `https://` scheme and contains no whitespace — that's
//! enough to cover the clipboard-paste flow the launcher is meant to catch
//! without swallowing short typed queries that happen to share a prefix.

pub fn detect(query: &str) -> Option<String> {
    let s = query.trim();
    if s.is_empty() {
        return None;
    }
    if !(s.starts_with("http://") || s.starts_with("https://")) {
        return None;
    }
    if s.chars().any(char::is_whitespace) {
        return None;
    }
    Some(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_url_detected() {
        assert_eq!(
            detect("https://example.com/path"),
            Some("https://example.com/path".into())
        );
    }

    #[test]
    fn http_url_detected() {
        assert_eq!(detect("http://foo.bar"), Some("http://foo.bar".into()));
    }

    #[test]
    fn trimmed_before_check() {
        assert_eq!(
            detect("  https://x.y  "),
            Some("https://x.y".into())
        );
    }

    #[test]
    fn plain_text_rejected() {
        assert!(detect("hello world").is_none());
        assert!(detect("safari").is_none());
    }

    #[test]
    fn embedded_whitespace_rejected() {
        assert!(detect("https://foo bar").is_none());
    }

    #[test]
    fn empty_rejected() {
        assert!(detect("").is_none());
        assert!(detect("   ").is_none());
    }
}
