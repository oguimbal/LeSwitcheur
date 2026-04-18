//! Runtime i18n for LeSwitcheur.
//!
//! Locale is detected once at boot from the OS (sys-locale → BCP-47 tag), the
//! primary subtag is matched against the 7 supported languages, and
//! `rust_i18n::set_locale` is called. If nothing matches, English is used
//! silently. No runtime switch, no settings UI.
//!
//! Translation files live in `locales/*.yml` and are embedded at compile time.
//! Callers use the re-exported `t!` macro.

rust_i18n::i18n!("locales", fallback = "en");

/// Detect system language and set the current rust-i18n locale. Idempotent.
pub fn init() {
    let code = detect();
    rust_i18n::set_locale(code);
    tracing::info!(locale = code, "i18n initialized");
}

/// Look up a translation by key (e.g. `"settings.header"`). Returns the
/// English fallback if the key is missing from the active locale, and the
/// literal key string if it's missing from English too.
///
/// `rust_i18n::t!` can't be re-exported across crates because it expands to
/// `crate::_rust_i18n_t(...)` — the generated function lives only in the
/// crate that calls `i18n!`. This wrapper lets any crate do lookups without
/// needing its own `i18n!` invocation and duplicated locale files.
pub fn tr(key: &str) -> String {
    rust_i18n::t!(key).into_owned()
}

/// Like [`tr`], but substitutes `%{name}` placeholders from `subs` pairs.
/// Matches rust-i18n's default interpolation syntax.
pub fn tr_sub(key: &str, subs: &[(&str, &str)]) -> String {
    let mut out = tr(key);
    for (name, val) in subs {
        let placeholder = format!("%{{{name}}}");
        out = out.replace(&placeholder, val);
    }
    out
}

/// List of supported primary language subtags.
const SUPPORTED: &[&str] = &["en", "fr", "es", "zh", "de", "it", "pt"];

/// Extract the primary subtag from a BCP-47 / POSIX locale tag (e.g.
/// `fr-FR`, `zh_Hans_CN`, `pt-BR.UTF-8`) and map it to one of the supported
/// locales. Falls back to `"en"` when no match is found or when the OS gives
/// us nothing readable.
fn detect() -> &'static str {
    let Some(raw) = sys_locale::get_locale() else {
        return "en";
    };
    map_tag(&raw)
}

fn map_tag(raw: &str) -> &'static str {
    let primary = raw
        .split(['-', '_', '.'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    for code in SUPPORTED {
        if primary == *code {
            return code;
        }
    }
    "en"
}

/// Symbol used in the UI to represent a modifier key. Differs by OS so the
/// rendering stays idiomatic (⌘⌃⌥⇧ on macOS, word labels elsewhere).
///
/// The `m` argument uses the internal key names we persist in `HotkeySpec`:
/// `cmd`, `ctrl`, `alt`/`opt`, `shift`.
pub fn modifier_symbol(m: &str) -> &'static str {
    let key = m.to_ascii_lowercase();
    #[cfg(target_os = "macos")]
    {
        match key.as_str() {
            "cmd" | "command" | "super" | "meta" => "⌘",
            "ctrl" | "control" => "⌃",
            "alt" | "opt" | "option" => "⌥",
            "shift" => "⇧",
            _ => "?",
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        match key.as_str() {
            "cmd" | "command" | "super" | "meta" => "Win",
            "ctrl" | "control" => "Ctrl",
            "alt" | "opt" | "option" => "Alt",
            "shift" => "Shift",
            _ => "?",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_tag_matches_primary_subtag() {
        assert_eq!(map_tag("en-US"), "en");
        assert_eq!(map_tag("fr_FR"), "fr");
        assert_eq!(map_tag("fr-CA"), "fr");
        assert_eq!(map_tag("es-419"), "es");
        assert_eq!(map_tag("zh-Hans-CN"), "zh");
        assert_eq!(map_tag("zh_TW"), "zh");
        assert_eq!(map_tag("de-DE"), "de");
        assert_eq!(map_tag("it-IT"), "it");
        assert_eq!(map_tag("pt-BR"), "pt");
        assert_eq!(map_tag("pt-PT.UTF-8"), "pt");
    }

    #[test]
    fn map_tag_falls_back_to_english() {
        assert_eq!(map_tag("ja-JP"), "en");
        assert_eq!(map_tag("ko"), "en");
        assert_eq!(map_tag(""), "en");
        assert_eq!(map_tag("xx-YY"), "en");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn modifier_symbols_macos() {
        assert_eq!(modifier_symbol("cmd"), "⌘");
        assert_eq!(modifier_symbol("Ctrl"), "⌃");
        assert_eq!(modifier_symbol("opt"), "⌥");
        assert_eq!(modifier_symbol("alt"), "⌥");
        assert_eq!(modifier_symbol("shift"), "⇧");
    }

    // rust-i18n's current locale is a process-global, so locale-dependent
    // assertions must run in a single `#[test]` to avoid races when cargo
    // parallelizes tests across threads.
    #[test]
    fn tr_and_tr_sub_across_locales() {
        rust_i18n::set_locale("en");
        assert_eq!(tr("settings.header"), "Settings");
        assert_eq!(tr("switcher.no_results"), "No results");
        assert_eq!(
            tr_sub("exclusions.invalid_regex", &[("err", "unclosed group")]),
            "invalid regex: unclosed group",
        );

        rust_i18n::set_locale("fr");
        assert_eq!(tr("settings.header"), "Réglages");
        rust_i18n::set_locale("es");
        assert_eq!(tr("settings.header"), "Ajustes");
        rust_i18n::set_locale("zh");
        assert_eq!(tr("settings.header"), "设置");
        rust_i18n::set_locale("de");
        assert_eq!(tr("settings.header"), "Einstellungen");
        rust_i18n::set_locale("it");
        assert_eq!(tr("settings.header"), "Impostazioni");
        rust_i18n::set_locale("pt");
        assert_eq!(tr("settings.header"), "Ajustes");
        rust_i18n::set_locale("en");
    }
}
