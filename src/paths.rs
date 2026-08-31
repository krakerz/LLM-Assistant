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
