//! Asset bundle for the switcher UI. GPUI's `svg()` element resolves path
//! strings (e.g. `"llm_icons/claude.svg"`) through an [`AssetSource`]
//! registered on the [`gpui::Application`]. We keep all visual assets
//! compiled into the binary via `include_bytes!` so the shipped .app is
//! self-contained.

use std::borrow::Cow;

use anyhow::{anyhow, Result};
use gpui::{AssetSource, SharedString};

const CLAUDE: &[u8] = include_bytes!("../assets/llm_icons/claude.svg");
const CHATGPT: &[u8] = include_bytes!("../assets/llm_icons/chatgpt.svg");
const MISTRAL: &[u8] = include_bytes!("../assets/llm_icons/mistral.svg");
const PERPLEXITY: &[u8] = include_bytes!("../assets/llm_icons/perplexity.svg");
const TAB_OVERLAY: &[u8] = include_bytes!("../assets/browser_icons/tab_overlay.svg");
const SPINNER: &[u8] = include_bytes!("../assets/browser_icons/spinner.svg");
const BRAND_LOGO: &[u8] = include_bytes!("../../../brand/logo-256.png");

/// Unit struct that the host registers via `application().with_assets(Assets)`.
/// Stateless — every lookup routes through the `match` below.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        match path {
            "llm_icons/claude.svg" => Ok(Some(Cow::Borrowed(CLAUDE))),
            "llm_icons/chatgpt.svg" => Ok(Some(Cow::Borrowed(CHATGPT))),
            "llm_icons/mistral.svg" => Ok(Some(Cow::Borrowed(MISTRAL))),
            "llm_icons/perplexity.svg" => Ok(Some(Cow::Borrowed(PERPLEXITY))),
            "browser_icons/tab_overlay.svg" => Ok(Some(Cow::Borrowed(TAB_OVERLAY))),
            "browser_icons/spinner.svg" => Ok(Some(Cow::Borrowed(SPINNER))),
            "brand/logo.png" => Ok(Some(Cow::Borrowed(BRAND_LOGO))),
            _ => Err(anyhow!("unknown asset: {path}")),
        }
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(vec![
            SharedString::from("claude.svg"),
            SharedString::from("chatgpt.svg"),
            SharedString::from("mistral.svg"),
            SharedString::from("perplexity.svg"),
            SharedString::from("tab_overlay.svg"),
            SharedString::from("spinner.svg"),
            SharedString::from("logo.png"),
        ])
    }
}
