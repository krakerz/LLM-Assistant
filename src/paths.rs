use std::io::Write;
use std::path::{Path, PathBuf};

/// `$XDG_CONFIG_HOME/llm-assistant`, falling back to `~/.config/llm-assistant`.
/// Deliberately not tied to the selected working directory -- config and logs
/// need to exist before any folder has been picked.
pub fn app_config_dir() -> PathBuf {
    resolve_config_dir(
        std::env::var("XDG_CONFIG_HOME").ok(),
        std::env::var("HOME").ok(),
    )
}

/// With neither variable set, a per-uid dir under the system temp dir beats
/// a CWD-relative `./.config` (which would scatter config, API keys and chat
/// history into whatever directory the app was launched from).
fn resolve_config_dir(xdg: Option<String>, home: Option<String>) -> PathBuf {
    if let Some(xdg) = xdg.filter(|x| !x.trim().is_empty()) {
        return PathBuf::from(xdg).join("llm-assistant");
    }
    if let Some(home) = home {
        return PathBuf::from(home).join(".config").join("llm-assistant");
    }
    static WARN: std::sync::Once = std::sync::Once::new();
    let uid = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self")
            .map(|m| m.uid())
            .unwrap_or(0)
    };
    let dir = std::env::temp_dir().join(format!("llm-assistant-{uid}"));
    WARN.call_once(|| {
        log::warn!(
            "neither XDG_CONFIG_HOME nor HOME is set -- using {} for config",
            dir.display()
        )
    });
    dir
}

/// Creates a brand-new file that never follows a symlink at `path` (O_EXCL
/// plus O_NOFOLLOW) and is born with `mode`, so a secret is never briefly
/// world-readable.
fn create_new_nofollow(path: &Path, mode: u32) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(mode)
        .open(path)
}

/// Crash-safe replace: writes a sibling temp file, fsyncs it, then renames
/// over `path`, so a reader (or a crash) never sees a truncated file. The temp
/// name is random and created exclusively: a symlink planted at a guessed name
/// can never redirect the write elsewhere. `mode: None` keeps the replaced
/// file's mode (0o666 minus umask for a new file).
pub fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?;
    let create_mode = mode.unwrap_or_else(|| {
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or(0o666)
    });
    let mut attempts = 0;
    let (tmp, mut file) = loop {
        let mut suffix = [0u8; 8];
        getrandom::getrandom(&mut suffix)
            .map_err(|e| std::io::Error::other(format!("OS RNG failed: {e}")))?;
        let mut tmp_name = name.to_os_string();
        tmp_name.push(format!(".tmp-{}", hex(&suffix)));
        let tmp = path.with_file_name(tmp_name);
        match create_new_nofollow(&tmp, create_mode) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < 4 => {
                attempts += 1;
            }
            Err(e) => return Err(e),
        }
    };
    let result = (|| {
        file.write_all(bytes)?;
        if let Some(mode) = mode {
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_dir_prefers_xdg_then_home_then_temp() {
        assert_eq!(
            resolve_config_dir(Some("/x".into()), Some("/h".into())),
            PathBuf::from("/x/llm-assistant")
        );
        assert_eq!(
            resolve_config_dir(Some("  ".into()), Some("/h".into())),
            PathBuf::from("/h/.config/llm-assistant")
        );
        let fallback = resolve_config_dir(None, None);
        assert!(fallback.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("llm-assistant-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.json");
        write_atomic(&file, b"one", Some(0o600)).unwrap();
        write_atomic(&file, b"two", None).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_creation_refuses_symlinks_and_existing_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("llm-assistant-nofollow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep").unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(create_new_nofollow(&link, 0o600).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        // A dangling link must not be created through either.
        let dangling = dir.join("dangling");
        std::os::unix::fs::symlink(dir.join("nowhere"), &dangling).unwrap();
        assert!(create_new_nofollow(&dangling, 0o600).is_err());
        assert!(!dir.join("nowhere").exists());
        // Mode is applied at creation.
        let fresh = dir.join("fresh");
        let f = create_new_nofollow(&fresh, 0o600).unwrap();
        drop(f);
        assert_eq!(
            std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_atomic_none_mode_keeps_existing_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("llm-assistant-keepmode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.json");
        write_atomic(&file, b"one", Some(0o600)).unwrap();
        write_atomic(&file, b"two", None).unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
