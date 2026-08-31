//! A flat, always-current transcript of the GUI conversation (including
//! "thinking" steps, since this file doesn't have a collapse concept) for
//! quick debugging without driving the actual window -- `tail -f` or `cat`
//! it instead of expanding disclosures in the app. Cleared on every GUI
//! launch (not headless mode, which has its own stdout output already).

use crate::paths::app_config_dir;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

fn log_path() -> PathBuf {
    app_config_dir().join("last-chat.log")
}

pub fn clear() -> anyhow::Result<()> {
    let path = log_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, "")?;
    Ok(())
}

pub fn append(text: &str) -> anyhow::Result<()> {
    let path = log_path();
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
