//! "Ask LLM" fallback launcher. Each provider goes through whichever entry
//! point reliably pre-fills the prompt:
//!
//! * **Claude** — web only: `claude.ai/new?q=...` opens a fresh chat with
//!   the query already in the composer, whether or not the desktop app is
//!   installed.
//! * **ChatGPT** — `chatgpt.com/?q=...&hints=search`. When the desktop app
//!   is present we force-route the URL through it via
//!   `open -b com.openai.chat`, otherwise it opens in the default browser.
//! * **Mistral Le Chat** — web only: `chat.mistral.ai/chat?q=...` pre-fills
//!   the composer, same pattern as Claude.
//! * **Perplexity** — native URL scheme `perplexity://search?q=...` when
//!   the app is installed, otherwise `perplexity.ai/search?q=...` on web.

use std::process::Command;

use anyhow::{anyhow, Result};
use objc2_app_kit::NSWorkspace;
use objc2_foundation::NSString;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use switcheur_core::LlmProvider;

/// Dispatch the query to the provider's preferred entry point.
pub fn open_llm(provider: LlmProvider, prompt: &str) -> Result<()> {
    tracing::info!(?provider, prompt_len = prompt.len(), "open_llm invoked");
    let encoded = utf8_percent_encode(prompt, NON_ALPHANUMERIC).to_string();
    match provider {
        LlmProvider::Claude => open_url(&format!("https://claude.ai/new?q={encoded}")),
        LlmProvider::ChatGpt => {
            let url = format!("https://chatgpt.com/?q={encoded}&hints=search");
            if app_path_for_bundle_id("com.openai.chat").is_some() {
                open_url_in_app("com.openai.chat", &url)
            } else {
                open_url(&url)
            }
        }
        LlmProvider::Mistral => open_url(&format!("https://chat.mistral.ai/chat?q={encoded}")),
        LlmProvider::Perplexity => {
            if app_path_for_bundle_id("ai.perplexity.mac").is_some() {
                open_url(&format!("perplexity://search?q={encoded}"))
            } else {
                open_url(&format!("https://www.perplexity.ai/search?q={encoded}"))
            }
        }
    }
}

/// Does the app with this bundle id resolve via Launch Services? Used to
/// pick between native-app and web paths for providers where the former
/// has a usable deep link.
fn app_path_for_bundle_id(bundle_id: &str) -> Option<std::path::PathBuf> {
    let workspace = NSWorkspace::sharedWorkspace();
    let id_ns = NSString::from_str(bundle_id);
    let url = workspace.URLForApplicationWithBundleIdentifier(&id_ns)?;
    let path = url.path()?.to_string();
    Some(std::path::PathBuf::from(path))
}

pub(crate) fn open_url(url: &str) -> Result<()> {
    open::that(url).map_err(|e| anyhow!("open {url}: {e}"))
}

/// Force macOS to open `url` with the app identified by `bundle_id`, even
/// if the user's default handler is different. Equivalent to
/// `open -b <bundle_id> <url>` in the shell.
fn open_url_in_app(bundle_id: &str, url: &str) -> Result<()> {
    let status = Command::new("/usr/bin/open")
        .args(["-b", bundle_id, url])
        .status()
        .map_err(|e| anyhow!("spawn open: {e}"))?;
    if !status.success() {
        anyhow::bail!("open -b {bundle_id} {url} exited with {status}");
    }
    Ok(())
}
