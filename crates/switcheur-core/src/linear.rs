//! Linear issue identifiers (`ENG-123`), recognised in the query so the host
//! can look the issue up.

/// `"eng-42 "` → `Some(("ENG", 42))`. The team key is a letter followed by
/// letters/digits, as Linear allows; the number is positive.
pub fn parse_identifier(query: &str) -> Option<(String, u64)> {
    let (key, number) = query.trim().split_once('-')?;
    let mut chars = key.chars();
    if !chars.next()?.is_ascii_alphabetic() || !chars.all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    if key.len() > 10 || number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number: u64 = number.parse().ok().filter(|n| *n > 0)?;
    Some((key.to_ascii_uppercase(), number))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_parse_case_insensitively() {
        assert_eq!(parse_identifier("ENG-123"), Some(("ENG".into(), 123)));
        assert_eq!(parse_identifier(" perso-7 "), Some(("PERSO".into(), 7)));
        assert_eq!(parse_identifier("hg2-15"), Some(("HG2".into(), 15)));
    }

    #[test]
    fn non_identifiers_are_rejected() {
        for q in ["ENG-", "-12", "ENG", "1234", "2ENG-1", "A-B-1", "ENG-12a", "ENG-0", "fix the bug"] {
            assert_eq!(parse_identifier(q), None, "{q}");
        }
    }
}
