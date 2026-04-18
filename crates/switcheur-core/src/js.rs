//! Lazy JavaScript evaluator backed by QuickJS (rquickjs).
//!
//! The runtime is thread-local: QuickJS assumes single-threaded access, and
//! `try_eval` is always called from the UI thread in production. A
//! thread-local avoids a Mutex and any `Send` hazards around QuickJS state.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use rquickjs::{Context, Ctx, Function, Object, Runtime, Type, Value};

const EVAL_TIMEOUT: Duration = Duration::from_millis(100);
const MEMORY_LIMIT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESULT_LEN: usize = 200;

struct JsRt {
    runtime: Runtime,
    context: Context,
}

fn new_rt() -> JsRt {
    let runtime = Runtime::new().expect("init QuickJS runtime");
    runtime.set_memory_limit(MEMORY_LIMIT_BYTES);
    let context = Context::full(&runtime).expect("init QuickJS context");
    JsRt { runtime, context }
}

thread_local! {
    static RT: RefCell<Option<JsRt>> = const { RefCell::new(None) };
}

pub fn try_eval(query: &str) -> Option<String> {
    let q = query.trim();
    if !looks_like_js(q) {
        return None;
    }

    RT.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let rt = borrow.get_or_insert_with(new_rt);

        let start = Instant::now();
        rt.runtime
            .set_interrupt_handler(Some(Box::new(move || start.elapsed() > EVAL_TIMEOUT)));

        let result = rt.context.with(|ctx| -> Option<String> {
            // Wrap as `(expr)` so `{a:1}` parses as an object literal rather
            // than a block. Fall back to raw eval for statement forms.
            let wrapped = format!("({})", q);
            let val: Value = match ctx.eval(wrapped.as_str()) {
                Ok(v) => v,
                Err(_) => ctx.eval(q).ok()?,
            };
            format_value(&ctx, val)
        });

        rt.runtime.set_interrupt_handler(None);
        result
    })
}

fn looks_like_js(q: &str) -> bool {
    if q.len() < 2 {
        return false;
    }
    let has_code_char = q.chars().any(|c| {
        matches!(
            c,
            '.' | '[' | ']' | '"' | '\'' | '`' | '{' | '}' | '=' | '+' | '-' |
            '*' | '/' | '%' | '<' | '>' | '!' | '?' | '(' | ')'
        )
    });
    if !has_code_char {
        return false;
    }
    // Reject queries that are a single bare identifier/phrase of words
    // (e.g. "hello world" — whitespace alone doesn't count as code).
    q.chars().any(|c| !c.is_alphanumeric() && c != '_' && c != ' ')
}

fn format_value<'js>(ctx: &Ctx<'js>, val: Value<'js>) -> Option<String> {
    match val.type_of() {
        Type::Undefined | Type::Null | Type::Uninitialized => return None,
        _ => {}
    }
    if val.is_function() {
        return None;
    }

    let s: String = if let Some(js_str) = val.as_string() {
        format!("\"{}\"", js_str.to_string().ok()?)
    } else if val.is_number() {
        let n: f64 = val.as_number()?;
        if !n.is_finite() {
            return None;
        }
        format_number(n)
    } else if val.is_bool() {
        val.as_bool()?.to_string()
    } else if val.is_object() {
        let global = ctx.globals();
        let json: Object = global.get("JSON").ok()?;
        let stringify: Function = json.get("stringify").ok()?;
        stringify.call((val.clone(),)).ok()?
    } else {
        return None;
    };

    Some(truncate(s))
}

fn format_number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let s = format!("{:.10}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

fn truncate(s: String) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > MAX_RESULT_LEN {
        let cut: String = s.chars().take(MAX_RESULT_LEN).collect();
        format!("{}…", cut)
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_arithmetic() {
        assert_eq!(try_eval("1+1"), Some("2".into()));
    }

    #[test]
    fn string_method() {
        assert_eq!(try_eval("\"abc\".toUpperCase()"), Some("\"ABC\"".into()));
    }

    #[test]
    fn array_reduce() {
        assert_eq!(
            try_eval("[1,2,3].reduce((a,b)=>a+b)"),
            Some("6".into())
        );
    }

    #[test]
    fn math_log2() {
        assert_eq!(try_eval("Math.log2(1024)"), Some("10".into()));
    }

    #[test]
    fn object_literal() {
        assert_eq!(try_eval("{a:1,b:2}"), Some("{\"a\":1,\"b\":2}".into()));
    }

    #[test]
    fn bare_ident_rejected() {
        assert_eq!(try_eval("hello"), None);
        assert_eq!(try_eval("chrome"), None);
    }

    #[test]
    fn phrase_rejected() {
        assert_eq!(try_eval("hello world"), None);
    }

    #[test]
    fn undefined_rejected() {
        assert_eq!(try_eval("undefined"), None);
    }

    #[test]
    fn infinity_rejected() {
        assert_eq!(try_eval("1/0"), None);
    }

    #[test]
    fn infinite_loop_times_out() {
        assert_eq!(try_eval("while(1){}"), None);
    }

    #[test]
    fn too_short() {
        assert_eq!(try_eval(""), None);
        assert_eq!(try_eval("a"), None);
    }

    #[test]
    fn boolean_result() {
        assert_eq!(try_eval("1 < 2"), Some("true".into()));
    }
}
