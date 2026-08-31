use crate::paths::app_log_dir;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Oldest deleted first. Hardcoded: these are debug artifacts.
const MAX_LOG_FILES: usize = 5;

/// Fixed at startup so every `append()` lands in the same file.
static CURRENT_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

fn is_chat_log(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with("chat-") && n.ends_with(".log"))
        .unwrap_or(false)
}

/// Fresh timestamped log, pruning older ones first. Timestamp-prefixed, so
/// lexicographic order is chronological.
pub fn init() -> anyhow::Result<PathBuf> {
    let dir = app_log_dir();
    fs::create_dir_all(&dir)?;

    let mut existing: Vec<PathBuf> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_chat_log(p))
        .collect();
    existing.sort();
    while existing.len() >= MAX_LOG_FILES {
        let oldest = existing.remove(0);
        let _ = fs::remove_file(&oldest);
    }

    let filename = format!("chat-{}.log", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let path = dir.join(filename);
    fs::write(&path, "")?;
    let _ = CURRENT_LOG_PATH.set(path.clone());
    Ok(path)
}

pub fn append(text: &str) -> anyhow::Result<()> {
    let path = CURRENT_LOG_PATH
        .get()
        .cloned()
        .unwrap_or_else(|| app_log_dir().join("chat-fallback.log"));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{text}\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Only directory state is asserted: CURRENT_LOG_PATH is a process-global
    // OnceLock that only the first init() in the test binary can set.
    #[test]
    fn init_prunes_down_to_max_log_files() {
        let dir =
            std::env::temp_dir().join(format!("llm-assistant-chatlog-test-{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let log_dir = app_log_dir();
        fs::create_dir_all(&log_dir).unwrap();

        let names: Vec<String> = (0..6)
            .map(|i| format!("chat-2026010{i}-000000.log"))
            .collect();
        for name in &names {
            fs::write(log_dir.join(name), "old session").unwrap();
        }

        let new_path = init().unwrap();

        let mut remaining: Vec<String> = fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("chat-") && n.ends_with(".log"))
            .collect();
        remaining.sort();

        assert_eq!(remaining.len(), MAX_LOG_FILES, "{remaining:?}");
        assert!(!remaining.contains(&names[0]));
        assert!(!remaining.contains(&names[1]));
        for name in &names[2..] {
            assert!(
                remaining.contains(name),
                "expected {name} to survive: {remaining:?}"
            );
        }
        assert!(remaining.contains(&new_path.file_name().unwrap().to_string_lossy().into_owned()));

        let _ = fs::remove_dir_all(&dir);
    }
}
