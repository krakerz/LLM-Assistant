//! Real web search via a self-hosted SearXNG instance, the same
//! `` ```web-search``` `` pseudo-tool pattern the ComfyUI integration
//! established for `` ```image-prompt``` ``: the dispatch pass (see
//! `chat_turn`'s module doc comment) fires the fence, `search` here does
//! the actual HTTP round-trip, and `chat_turn::run_search_answer_turn`
//! feeds the real results back to the model for a real answer -- turn 1
//! never has real information, only "let me check" (same reasoning as
//! image generation's reaction turn).
//!
//! Stored separately from `config.toml` (`<app-config-dir>/searxng.json`),
//! same reasoning as `comfyui::ComfyUiConfig`.
//!
//! The `web-search` ruleset (renamed from `other-tools` once its role
//! narrowed to specifically this) holds *content-policy* guidance only
//! (what NOT to search for or return -- NSFW, politics, whatever the user
//! wants filtered) -- the mechanical fence syntax and the actual request
//! are both app-controlled, same reasoning as `comfyui::IMAGE_PROMPT_PROTOCOL`.

use crate::paths::app_config_dir;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearxngConfig {
    #[serde(default)]
    pub base_url: String,
    /// Sent as `Authorization: Bearer <key>` if non-empty -- most
    /// self-hosted instances (including the one this was built against)
    /// need no key at all, so this stays optional and unused by default.
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_max_results")]
    pub max_results: u32,
}

fn default_max_results() -> u32 {
    5
}

impl Default for SearxngConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            max_results: default_max_results(),
        }
    }
}

fn config_path() -> PathBuf {
    app_config_dir().join("searxng.json")
}

pub fn load_or_init() -> anyhow::Result<SearxngConfig> {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(_) => {
            let cfg = SearxngConfig::default();
            save(&cfg)?;
            Ok(cfg)
        }
    }
}

pub fn save(cfg: &SearxngConfig) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    /// SearXNG's own "content" field -- a short snippet, not the full page.
    pub content: String,
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// One query, `cfg.max_results` results back -- SearXNG's own ranking
/// decides which ones, this only truncates the list, never re-sorts it.
pub async fn search(cfg: &SearxngConfig, query: &str) -> anyhow::Result<Vec<SearchResult>> {
    if cfg.base_url.trim().is_empty() {
        anyhow::bail!(
            "web search isn't configured yet -- set a SearXNG URL in Settings' Web Search tab"
        );
    }
    let base_url = cfg.base_url.trim().trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()?;
    let mut request = client
        .get(format!("{base_url}/search"))
        .query(&[("q", query), ("format", "json")]);
    if !cfg.api_key.trim().is_empty() {
        request = request.bearer_auth(cfg.api_key.trim());
    }
    let body: serde_json::Value = request.send().await?.error_for_status()?.json().await?;

    let results = body
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    Ok(results
        .into_iter()
        .filter_map(|r| {
            Some(SearchResult {
                title: r.get("title")?.as_str()?.to_string(),
                url: r.get("url")?.as_str()?.to_string(),
                content: r
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .take(cfg.max_results.max(1) as usize)
        .collect())
}

/// The mechanical fence explanation, injected by
/// `rules::build_dispatch_system_content` whenever `ruleset::WEB_SEARCH_RULESET_NAME`
/// is loaded, regardless of that file's own content -- same reasoning as
/// `comfyui::IMAGE_PROMPT_PROTOCOL`.
pub const WEB_SEARCH_PROTOCOL: &str = "\
Request a real web search with a fenced block on its own:\n\n\
```web-search\nquery: what to search for\n```\n\n\
This actually runs a search and the real results come back afterward, as a follow-up message -- \
you have no way to know what it'll find from here, so don't guess or make up results yourself. If \
nothing is configured yet, the app will say so plainly rather than silently doing nothing.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_no_base_url_and_five_max_results() {
        let cfg = SearxngConfig::default();
        assert_eq!(cfg.base_url, "");
        assert_eq!(cfg.max_results, 5);
    }
}
