use std::path::PathBuf;

/// `$XDG_CONFIG_HOME/llm-assistant`, falling back to `~/.config/llm-assistant`.
/// Deliberately not tied to the selected working directory -- config and logs
/// need to exist before any folder has been picked.
pub fn app_config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg).join("llm-assistant");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("llm-assistant")
}

pub fn app_log_dir() -> PathBuf {
    app_config_dir().join("logs")
}

/// Keeps a user-supplied name safe as a bare filename: no path separators,
/// no leading dot (would make it a hidden file, and `..` a traversal),
/// never empty. Shared by `persona.rs` and `ruleset.rs`, which both store
/// freeform `.md` files named directly after a user-typed or imported name.
pub fn sanitize_filename(name: &str, fallback: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').trim();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.to_string()
    }
}
