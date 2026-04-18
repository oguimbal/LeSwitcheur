//! Domain types describing what the switcher can switch to.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// A concrete window owned by some running application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRef {
    /// CoreGraphics window number (`kCGWindowNumber`). Unique per window for its lifetime.
    pub id: u64,
    /// Owning process id.
    pub pid: i32,
    /// Window title, as reported by `kCGWindowName`. May be empty for some windows.
    pub title: String,
    /// Application display name (e.g. "Safari").
    pub app_name: String,
    /// Bundle identifier when resolvable, e.g. `com.apple.Safari`.
    pub bundle_id: Option<String>,
    /// Filesystem path to a cached PNG of the app icon (resolved by the platform crate).
    pub icon_path: Option<PathBuf>,
    /// True if the window is currently minimized to the Dock.
    pub minimized: bool,
}

/// A running application, used when the user enables the "apps" scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRef {
    pub pid: i32,
    pub name: String,
    pub bundle_id: Option<String>,
    pub icon_path: Option<PathBuf>,
}

/// An installed (but not necessarily running) application the user can launch.
/// Populated by the platform's program source (Spotlight on macOS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramRef {
    pub name: String,
    pub bundle_id: Option<String>,
    pub bundle_path: PathBuf,
    pub icon_path: Option<PathBuf>,
}

impl WindowRef {
    /// Title shown as the primary line in the UI — falls back to the app name
    /// when the window has no title of its own.
    pub fn display_title(&self) -> &str {
        if self.title.is_empty() {
            &self.app_name
        } else {
            &self.title
        }
    }

    /// Subtitle, if any — the app name when we have a distinct window title.
    pub fn display_subtitle(&self) -> Option<&str> {
        if self.title.is_empty() {
            None
        } else {
            Some(&self.app_name)
        }
    }
}

/// One of the well-known LLM providers the switcher can hand a query off to
/// when no window/app/program matches. Serialized in the user config to
/// persist the MRU order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmProvider {
    Mistral,
    Claude,
    ChatGpt,
    Perplexity,
}

impl LlmProvider {
    /// Canonical default ordering — Mistral first for EU-first positioning,
    /// then the others in rough usage order. Used when the config has no
    /// prior preference stored yet.
    pub fn default_order() -> Vec<LlmProvider> {
        vec![
            LlmProvider::Mistral,
            LlmProvider::Claude,
            LlmProvider::ChatGpt,
            LlmProvider::Perplexity,
        ]
    }

    /// Display name in the row, e.g. "Le Chat" / "Claude" / …
    pub fn display_name(self) -> &'static str {
        match self {
            LlmProvider::Mistral => "Le Chat",
            LlmProvider::Claude => "Claude",
            LlmProvider::ChatGpt => "ChatGPT",
            LlmProvider::Perplexity => "Perplexity",
        }
    }

    /// i18n key for the "Ask <provider>" row primary text.
    pub fn i18n_key(self) -> &'static str {
        match self {
            LlmProvider::Mistral => "llm.ask_mistral",
            LlmProvider::Claude => "llm.ask_claude",
            LlmProvider::ChatGpt => "llm.ask_chatgpt",
            LlmProvider::Perplexity => "llm.ask_perplexity",
        }
    }

    /// Single-character seed for the placeholder icon.
    pub fn icon_initial(self) -> char {
        match self {
            LlmProvider::Mistral => 'M',
            LlmProvider::Claude => 'C',
            LlmProvider::ChatGpt => 'G',
            LlmProvider::Perplexity => 'P',
        }
    }
}

/// Anything selectable in the switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Window(Arc<WindowRef>),
    App(Arc<AppRef>),
    Program(Arc<ProgramRef>),
    /// Fallback row shown when the query matches no window/app/program and
    /// isn't a math/JS expression: "Ask <Provider>". Activating it sends the
    /// query to that LLM (native app if installed, web URL otherwise).
    AskLlm {
        provider: LlmProvider,
        query: Arc<str>,
    },
    /// Row shown when the query is a URL (http/https). Activating it opens
    /// the URL in the user's default browser.
    OpenUrl(Arc<str>),
}

impl Item {
    /// The text used as a haystack for fuzzy matching.
    pub fn search_text(&self) -> String {
        match self {
            Item::Window(w) => {
                if w.title.is_empty() {
                    w.app_name.clone()
                } else {
                    format!("{} — {}", w.app_name, w.title)
                }
            }
            Item::App(a) => a.name.clone(),
            Item::Program(p) => p.name.clone(),
            Item::AskLlm { provider, .. } => provider.display_name().to_string(),
            Item::OpenUrl(url) => url.to_string(),
        }
    }

    pub fn primary(&self) -> &str {
        match self {
            Item::Window(w) => w.display_title(),
            Item::App(a) => &a.name,
            Item::Program(p) => &p.name,
            // AskLlm rows resolve their primary text through i18n at render
            // time — the row renderer calls `tr(provider.i18n_key())`. This
            // accessor only returns the bare provider name as a fallback.
            Item::AskLlm { provider, .. } => provider.display_name(),
            // OpenUrl rows also resolve their primary label via i18n at render
            // time. Fall back to the URL itself.
            Item::OpenUrl(url) => url,
        }
    }

    pub fn secondary(&self) -> Option<&str> {
        match self {
            Item::Window(w) => w.display_subtitle(),
            Item::OpenUrl(url) => Some(url),
            _ => None,
        }
    }

    /// Stable short code used to color the placeholder icon.
    pub fn icon_seed(&self) -> &str {
        match self {
            Item::Window(w) => w.bundle_id.as_deref().unwrap_or(&w.app_name),
            Item::App(a) => a.bundle_id.as_deref().unwrap_or(&a.name),
            Item::Program(p) => p.bundle_id.as_deref().unwrap_or(&p.name),
            Item::AskLlm { provider, .. } => provider.display_name(),
            Item::OpenUrl(_) => "open_url",
        }
    }

    /// First visible character of the app name, for placeholder icons.
    pub fn icon_initial(&self) -> char {
        let name = match self {
            Item::Window(w) => w.app_name.as_str(),
            Item::App(a) => a.name.as_str(),
            Item::Program(p) => p.name.as_str(),
            Item::AskLlm { provider, .. } => return provider.icon_initial(),
            Item::OpenUrl(_) => return '↗',
        };
        name.chars().next().unwrap_or('?').to_ascii_uppercase()
    }

    /// Path to a cached PNG of the icon, if the platform resolved one.
    pub fn icon_path(&self) -> Option<&std::path::Path> {
        match self {
            Item::Window(w) => w.icon_path.as_deref(),
            Item::App(a) => a.icon_path.as_deref(),
            Item::Program(p) => p.icon_path.as_deref(),
            Item::AskLlm { .. } | Item::OpenUrl(_) => None,
        }
    }

    /// Whether this item is a minimized window (always false for apps).
    pub fn is_minimized(&self) -> bool {
        matches!(self, Item::Window(w) if w.minimized)
    }
}

impl From<WindowRef> for Item {
    fn from(w: WindowRef) -> Self {
        Item::Window(Arc::new(w))
    }
}

impl From<AppRef> for Item {
    fn from(a: AppRef) -> Self {
        Item::App(Arc::new(a))
    }
}

impl From<ProgramRef> for Item {
    fn from(p: ProgramRef) -> Self {
        Item::Program(Arc::new(p))
    }
}
