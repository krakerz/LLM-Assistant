use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantedPath {
    pub path: String,
    pub note: String,
    pub read_write: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub endpoint: String,
    pub model: String,
    pub system_prompt: String,
    pub temperature: f32,
    #[serde(default)]
    pub granted_paths: Vec<GrantedPath>,
    #[serde(default)]
    pub auto_approve: Vec<String>,
}

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a local file assistant working inside a single \
folder that the user opened for you. You cannot see or change anything outside that folder unless \
the user has explicitly granted you a path.\n\nWhen you want to take an action (list, search, move, \
copy, rename, edit, or delete files), first give a one-line explanation of what it will do, then put \
exactly one shell command in a single fenced code block, for example:\n\nMove all PNGs into an images \
folder.\n```sh\nmkdir -p images && mv -- *.png images/\n```\n\nOnly one command per reply. Wait for the \
command's output (given back to you as the next message) before proposing another step. Read-only \
commands (ls, cat, grep, find, ...) run immediately; anything else waits for the user to approve it, \
so don't be afraid to propose it — just don't chain unrelated destructive steps together.";

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:11434/v1/chat/completions".into(),
            model: "llama3.1".into(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            temperature: 0.2,
            granted_paths: vec![],
            auto_approve: vec![],
        }
    }
}

fn config_path(root: &Path) -> PathBuf {
    root.join(".config").join("config.toml")
}

pub fn load_or_init(root: &Path) -> anyhow::Result<AppConfig> {
    let path = config_path(root);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(toml::from_str(&text)?),
        Err(_) => {
            let cfg = AppConfig::default();
            save(root, &cfg)?;
            Ok(cfg)
        }
    }
}

pub fn save(root: &Path, cfg: &AppConfig) -> anyhow::Result<()> {
    let path = config_path(root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, toml::to_string_pretty(cfg)?)?;
    Ok(())
}
