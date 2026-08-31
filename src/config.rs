use crate::paths::app_config_dir;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

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
    #[serde(default)]
    pub api_key: String,
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
folder.\n```sh\nmkdir -p images && mv -- *.png images/\n```\n\nOnly one command per reply. A command's \
output is automatically given back to you as the next message, so after that happens, actually use it: \
answer the user's original question, summarize the content, or explain what you found, in plain text \
with no code block. Only propose another command if a further action is genuinely needed -- don't run \
a command just to immediately run another one.\n\nWhen the user refers to a file by topic or description \
rather than an exact filename (e.g. \"my shopping list\"), don't guess a name or extension -- first \
search for likely matches (e.g. `find . -iname '*shopping*'`). If more than one file could reasonably \
match, list the candidates in plain text and ask the user which one they mean before reading any of \
them.\n\nRead-only commands (ls, cat, grep, find, ...) run immediately; anything else waits for the user \
to approve it, so don't be afraid to propose it -- just don't chain unrelated destructive steps together. \
Deletions are not permanent: anything removed is moved into a `.temp-trash` folder that mirrors the \
original layout, so proposing a delete when it's genuinely the right step is fine.\n\nOnly ever put text \
inside a ```sh fence when it is a literal command to run -- never use a ```sh (or ```bash/```shell) fence \
to show file contents, a list, or any other text; use a plain fence with no language tag (or no fence at \
all) for that. The folder's contents can also change between turns, including from outside this app -- \
don't assume an earlier `ls`/`find` output in this conversation is still accurate; if it matters, \
re-check with a fresh listing rather than answering from memory.";

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:11434/v1/chat/completions".into(),
            model: "llama3.1".into(),
            api_key: String::new(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            temperature: 0.2,
            granted_paths: vec![],
            auto_approve: vec![],
        }
    }
}

fn config_path() -> PathBuf {
    app_config_dir().join("config.toml")
}

/// Config is global (one file under `~/.config/llm-assistant/`), not scoped
/// to the currently selected folder -- it needs to exist before any folder
/// has been picked, and it's the same assistant settings across projects.
pub fn load_or_init() -> anyhow::Result<AppConfig> {
    let path = config_path();
    match fs::read_to_string(&path) {
        Ok(text) => Ok(toml::from_str(&text)?),
        Err(_) => {
            let cfg = AppConfig::default();
            save(&cfg)?;
            Ok(cfg)
        }
    }
}

pub fn save(cfg: &AppConfig) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, toml::to_string_pretty(cfg)?)?;
    Ok(())
}
