//! Spotlight-style inline math evaluation of the query.
//!
//! `meval`'s parser returns `Err` for anything that isn't a valid arithmetic
//! expression — we use that as our "is it math?" signal, with a small
//! pre-filter so bare identifiers like `pi` or `e` don't accidentally count.

pub fn try_eval(query: &str) -> Option<String> {
    let q = query.trim();
    if q.len() < 2 {
        return None;
    }
    // Require at least one arithmetic sigil. `(` counts so function calls like
    // `sin(pi)` qualify without a literal digit. Bare identifiers (`pi`, `e`)
    // fail this check and are rejected, so the switcher doesn't hijack words.
    let has_op = q
        .chars()
        .any(|c| matches!(c, '+' | '-' | '*' | '/' | '^' | '%' | '('));
    if !has_op {
        return None;
    }
    let value = meval::eval_str(q).ok()?;
    if !value.is_finite() {
        return None;
    }
    Some(format_number(value))
}

fn format_number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{:.10}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_sum() {
        assert_eq!(try_eval("2+2"), Some("4".into()));
    }

    #[test]
    fn precedence() {
        assert_eq!(try_eval("2+2*3"), Some("8".into()));
    }

    #[test]
    fn parens() {
        assert_eq!(try_eval("(10+5)*2"), Some("30".into()));
    }

    #[test]
    fn fractional() {
        assert_eq!(try_eval("10/4"), Some("2.5".into()));
    }

    #[test]
    fn sqrt_fn() {
        assert_eq!(try_eval("sqrt(81)"), Some("9".into()));
    }

    #[test]
    fn plain_text_rejected() {
        assert_eq!(try_eval("hello"), None);
    }

    #[test]
    fn bare_constant_rejected() {
        assert_eq!(try_eval("pi"), None);
        assert_eq!(try_eval("e"), None);
    }

    #[test]
    fn constant_in_expression_accepted() {
        assert!(try_eval("2*pi").is_some());
    }

    #[test]
    fn sin_of_pi() {
        // sin(π) ≈ 0 → formatter collapses to "0"
        assert_eq!(try_eval("sin(pi)"), Some("0".into()));
    }

    #[test]
    fn fn_call_without_digit_accepted() {
        assert!(try_eval("sqrt(pi)").is_some());
    }

    #[test]
    fn division_by_zero_rejected() {
        assert_eq!(try_eval("1/0"), None);
    }

    #[test]
    fn single_digit_too_short() {
        assert_eq!(try_eval("5"), None);
    }

    #[test]
    fn empty() {
        assert_eq!(try_eval(""), None);
        assert_eq!(try_eval("   "), None);
    }

    #[test]
    fn negative_result() {
        assert_eq!(try_eval("3-10"), Some("-7".into()));
    }
}
