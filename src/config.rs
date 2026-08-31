use crate::paths::app_config_dir;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantedPath {
    pub path: String,
    pub note: String,
    pub read_write: bool,
    /// Whether subfolders are included. A bind mount is inherently
    /// recursive, so `false` is handled specially in `sandbox.rs` by
    /// binding only the top-level files instead of the whole directory.
    /// Defaults to `true` for configs written before this field existed,
    /// matching their actual (always-recursive) behavior at the time.
    #[serde(default = "default_recursive")]
    pub recursive: bool,
}

fn default_recursive() -> bool {
    true
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
    /// How many propose-command -> run -> respond cycles the assistant can
    /// chain automatically (without the user sending another message)
    /// before it stops and waits. 0 means no limit. Kept fairly generous by
    /// default since intermediate steps are hidden behind a collapsed
    /// "Thinking" section in the UI, not shown inline.
    #[serde(default = "default_max_auto_steps")]
    pub max_auto_steps: u32,
    /// When true, `rules.md`/`command-rules.md` (the editable, "additional"
    /// advisory rules) are not sent at all -- only the hardcoded protocol
    /// prompt (command format, one-command-per-reply, sudo handling, see
    /// `rules::PROTOCOL_PROMPT`) plus the user's own system prompt. Lets
    /// someone who wants full control over behavior discard the app's
    /// baked-in advice without it competing with their own instructions.
    /// Defaults to false (existing behavior: both files are sent).
    #[serde(default)]
    pub disable_builtin_rules: bool,
}

fn default_max_auto_steps() -> u32 {
    12
}

/// Deliberately short -- this is the part meant to be customized per-user
/// (persona, focus, tone). The mechanical/protocol rules (command format,
/// quoting, confirmation behavior, etc.) live separately in `rules.rs` /
/// `rules.md`, read first, so editing this can't accidentally break them.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a local file assistant. The user has opened a \
single folder for you to work in; you cannot see or change anything outside it unless they've \
explicitly granted you another path. Follow the working rules provided separately from this prompt.";

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
            max_auto_steps: default_max_auto_steps(),
            disable_builtin_rules: false,
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

/// Per-turn note appended after the rules/system prompt telling the model
/// whether a folder is open and, if so, what it can propose commands
/// against and what else it's been granted read (or read-write) access to
/// -- and why, so it can actually use a granted path proactively instead of
/// only when the user spells out the absolute path themselves. Shared by
/// the GUI's `send_message` and headless mode so both stay consistent.
pub fn build_root_note(root: Option<&Path>, granted_paths: &[GrantedPath]) -> String {
    let Some(root) = root else {
        return "No folder is open right now, so don't propose shell commands -- just chat \
                normally, and if the user wants file operations, tell them to select a folder \
                first."
            .to_string();
    };

    let mut note = format!(
        "You currently have this folder open and can propose shell commands confined to it: {}. \
         This folder is the \"root\"/home context for this session -- if the user says \"root\", \
         \"root folder\", or similar without naming one of the granted paths below, they mean the \
         top level of THIS folder, not a granted path.",
        root.display()
    );
    if !granted_paths.is_empty() {
        note.push_str(
            "\n\nYou also have access to these additional paths outside the working folder \
             (these are not the \"root\" unless the user names one specifically):",
        );
        for g in granted_paths {
            let access = if g.read_write {
                "read-write"
            } else {
                "read-only"
            };
            let scope = if g.recursive {
                "includes subfolders"
            } else {
                "top-level files only, no subfolders"
            };
            let why = if g.note.trim().is_empty() {
                String::new()
            } else {
                format!(" -- {}", g.note.trim())
            };
            note.push_str(&format!("\n- {} ({access}, {scope}){why}", g.path));
        }
    }
    note
}
