//! Real web search via a self-hosted SearXNG instance. `chat_director` decides
//! when a search is needed, `search` here does the HTTP round-trip, and
//! `chat_turn::web_search` feeds the real results back to the model for an
//! answer grounded in what was found rather than a guess.
//!
//! Stored separately from `config.toml` (`<app-config-dir>/searxng.json`),
//! same reasoning as `comfyui::ComfyUiConfig`. The `web-search` ruleset file
//! holds only content-policy guidance for the answer (what not to return).

use crate::paths::{app_config_dir, write_atomic};
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
        Ok(text) => {
            serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cfg = SearxngConfig::default();
            save(&cfg)?;
            Ok(cfg)
        }
        Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
    }
}

pub fn save(cfg: &SearxngConfig) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 0600: `api_key` lives in here.
    write_atomic(
        &path,
        serde_json::to_string_pretty(cfg)?.as_bytes(),
        Some(0o600),
    )?;
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

/// `reqwest` sends none of these by default -- fine for the app's other
/// HTTP clients (a local LLM endpoint, a local ComfyUI instance, neither of
/// which cares), but SearXNG's own bot detection (`searx.botdetection`)
/// checks each of these independently and blocks on the first one it finds
/// missing, one at a time -- `http_user_agent`, then (without the crate's
/// `gzip`/`deflate` features) `http_accept_encoding`, then
/// `http_accept_language`. A real browser always sends all of these
/// together, so they're set together here rather than getting fixed one
/// discovered check at a time.
const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
const ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

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
        .user_agent(USER_AGENT)
        .build()?;
    let mut request = client
        .get(format!("{base_url}/search"))
        .header(reqwest::header::ACCEPT, ACCEPT)
        .header(reqwest::header::ACCEPT_LANGUAGE, ACCEPT_LANGUAGE)
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
