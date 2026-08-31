use crate::paths::app_log_dir;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// How many rotated chat logs to keep around, oldest deleted first. Hardcoded
/// rather than configurable -- these are debug artifacts, not something
/// worth a settings entry for.
const MAX_LOG_FILES: usize = 5;

/// The path chosen for *this* run's log, fixed once at startup via `init()`
/// so every `append()` call (each one a separate Tauri command invocation)
/// keeps writing to the same file instead of fragmenting across several.
static CURRENT_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

fn is_chat_log(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with("chat-") && n.ends_with(".log"))
        .unwrap_or(false)
}

/// Starts a fresh timestamped log for this session, pruning older ones down
/// to `MAX_LOG_FILES - 1` first (so after adding the new one, at most
/// `MAX_LOG_FILES` remain). Filenames are timestamp-prefixed, so a plain
/// lexicographic sort is also chronological order -- no need to stat mtimes.
/// Past sessions survive this (unlike the old single-file `last-chat.log`
/// that got wiped on every launch), so a session that looped or misbehaved
/// stays inspectable after relaunching to try again.
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

    // Simulates 6 prior sessions' logs already sitting in the log dir (named
    // so they sort oldest-first, matching the real chat-<timestamp>.log
    // shape) and then calls init() once, as a new launch would. Only
    // directory state is asserted -- CURRENT_LOG_PATH is a process-global
    // OnceLock that only accepts the very first init() call across the whole
    // test binary, so it isn't a reliable thing to assert on here.
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
        // The two oldest of the six pre-existing files must be gone.
        assert!(!remaining.contains(&names[0]));
        assert!(!remaining.contains(&names[1]));
        // The four newest pre-existing ones, plus the freshly created one,
        // must all still be present.
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
