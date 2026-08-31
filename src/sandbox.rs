//! Linux-only: confines shell commands proposed by the LLM to the selected
//! working directory (plus explicitly granted paths) using `bwrap`, and
//! shims `rm` inside the jail to soft-delete into `.temp-trash/` instead of
//! actually removing files. See project brainstorm notes for the rationale:
//! we don't try to classify "is this command safe" from its text (that's
//! unreliable) -- the sandbox makes "outside the folder" structurally
//! impossible, and the trash shim makes "destructive inside the folder"
//! recoverable. The only thing we *do* classify from text is "can this run
//! without asking the user first", which only needs to be conservative in
//! one direction (never wrongly say yes).

use crate::config::GrantedPath;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const READ_ONLY_BINARIES: &[&str] = &[
    "ls", "cat", "grep", "head", "tail", "wc", "file", "stat", "tree", "pwd", "echo", "du", "df",
    "find", "uname", "whoami", "id", "hostname",
];

/// Any of these appearing in the raw command text means we can no longer
/// reason about it as "just this one program" -- redirects, pipes, and
/// command substitution can turn a harmless-looking binary into a write.
const SHELL_METACHARACTERS: &[&str] = &["|", ">", "<", "&", ";", "`", "$(", "\n"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Classification {
    ReadOnly,
    NeedsConfirmation,
}

fn has_metacharacters(cmd: &str) -> bool {
    SHELL_METACHARACTERS.iter().any(|m| cmd.contains(m))
}

fn first_binary(cmd: &str) -> Option<String> {
    let tokens = shell_words::split(cmd).ok()?;
    let first = tokens.first()?;
    Some(
        Path::new(first)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(first)
            .to_string(),
    )
}

pub fn classify_command(cmd: &str) -> Classification {
    let trimmed = cmd.trim();
    if trimmed.is_empty() || has_metacharacters(trimmed) {
        return Classification::NeedsConfirmation;
    }
    // `find` is read-only-shaped but can delete/exec; don't trust it blindly.
    if trimmed.contains("-delete") || trimmed.contains("-exec") {
        return Classification::NeedsConfirmation;
    }
    match first_binary(trimmed) {
        Some(bin) if READ_ONLY_BINARIES.contains(&bin.as_str()) => Classification::ReadOnly,
        _ => Classification::NeedsConfirmation,
    }
}

/// A user can allow a specific program to always run without prompting.
/// This still requires the command to have no shell metacharacters --
/// otherwise "always allow cat" could be smuggled into "cat x > /etc/passwd".
pub fn is_auto_approved(cmd: &str, auto_approve: &[String]) -> bool {
    let trimmed = cmd.trim();
    if trimmed.is_empty() || has_metacharacters(trimmed) {
        return false;
    }
    match first_binary(trimmed) {
        Some(bin) => auto_approve.iter().any(|b| b == &bin),
        None => false,
    }
}

pub struct RunOutcome {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

const RM_SHIM_SCRIPT: &str = r#"#!/bin/sh
# Soft-delete shim: moves rm's targets into .temp-trash instead of unlinking
# them, preserving their path relative to the sandbox root so a manual
# restore just means moving them back. Every invocation gets its own
# timestamped subfolder (all targets of one rm call share it) so deleting
# the same path twice can never silently clobber the earlier trashed copy --
# mv -f below would otherwise overwrite it without a trace.
trash_root="${TRASH_ROOT:-$PWD/.temp-trash}/$(date +%Y%m%d-%H%M%S-%N)"
for arg in "$@"; do
  case "$arg" in
    -*) continue ;;
  esac
  case "$arg" in
    /*) rel="${arg#/}" ;;
    *) rel="$arg" ;;
  esac
  dest="$trash_root/$rel"
  mkdir -p "$(dirname "$dest")"
  mv -f -- "$arg" "$dest" 2>/dev/null
done
"#;

pub fn ensure_shims(shim_dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(shim_dir)?;
    let rm_shim = shim_dir.join("rm");
    fs::write(&rm_shim, RM_SHIM_SCRIPT)?;
    fs::set_permissions(&rm_shim, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

pub fn run_sandboxed(
    root: &Path,
    shim_dir: &Path,
    granted: &[GrantedPath],
    cmd: &str,
) -> anyhow::Result<RunOutcome> {
    let mut c = Command::new("bwrap");
    // Belt and suspenders: env_clear() stops our own process's environment
    // from reaching bwrap at all, and --clearenv stops bwrap from passing
    // anything through to the sandboxed shell either. Only PATH and
    // TRASH_ROOT (set explicitly below) end up visible inside.
    c.env_clear();
    c.arg("--die-with-parent")
        .arg("--clearenv")
        .arg("--unshare-all")
        .arg("--proc")
        .arg("/proc")
        .arg("--dev")
        .arg("/dev")
        .arg("--tmpfs")
        .arg("/tmp");

    for base in ["/usr", "/bin", "/lib", "/lib64", "/etc"] {
        if Path::new(base).exists() {
            c.arg("--ro-bind").arg(base).arg(base);
        }
    }

    c.arg("--ro-bind").arg(shim_dir).arg(shim_dir);

    for g in granted {
        let flag = if g.read_write { "--bind" } else { "--ro-bind" };
        let path = Path::new(&g.path);
        if !path.exists() {
            continue; // a stale grant shouldn't break every command
        }
        if g.recursive {
            c.arg(flag).arg(path).arg(path);
        } else {
            // A bind mount is inherently recursive, so "just this directory"
            // means binding each top-level file individually instead of the
            // whole tree -- subfolders simply aren't bound, so they don't
            // show up inside the sandbox at all.
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    let entry_path = entry.path();
                    if entry_path.is_file() {
                        c.arg(flag).arg(&entry_path).arg(&entry_path);
                    }
                }
            }
        }
    }

    // Bound last, deliberately: bwrap applies bind mounts in argument order,
    // and a later mount on top of (or covering) an earlier one wins. If a
    // granted path happens to be an ancestor of the working folder (e.g.
    // granting ~/src while the folder open is ~/src/playground), binding it
    // before root would silently make root read-only too. Binding root last
    // means it always wins back its own subtree, regardless of what else
    // was granted.
    c.arg("--bind").arg(root).arg(root);

    let path_env = format!("{}:/usr/bin:/bin", shim_dir.display());
    let trash_root = root.join(".temp-trash");

    c.arg("--chdir")
        .arg(root)
        .arg("--setenv")
        .arg("PATH")
        .arg(&path_env)
        .arg("--setenv")
        .arg("TRASH_ROOT")
        .arg(&trash_root)
        .arg("sh")
        .arg("-c")
        .arg(cmd);

    let output = c.output()?;
    Ok(RunOutcome {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_code: output.status.code().unwrap_or(-1),
    })
}

pub fn default_shim_dir() -> PathBuf {
    std::env::temp_dir().join("llm-assistant-shims")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread::sleep, time::Duration};

    // Proves the timestamped-subfolder fix for real, inside the actual
    // bwrap sandbox: deleting the same relative path twice must land in two
    // distinct .temp-trash subfolders, not silently overwrite the first
    // trashed copy via mv -f. Requires bwrap and coreutils' `date`, same as
    // the app itself does at runtime.
    #[test]
    fn repeated_rm_of_same_path_does_not_clobber_earlier_trash() {
        let root =
            std::env::temp_dir().join(format!("llm-assistant-sandbox-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/note.txt"), "version1").unwrap();

        let shim_dir = root.join("shims");
        ensure_shims(&shim_dir).unwrap();

        let outcome = run_sandboxed(&root, &shim_dir, &[], "rm sub/note.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        // Force a distinct nanosecond-precision timestamp for the second rm.
        sleep(Duration::from_millis(20));
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/note.txt"), "version2").unwrap();
        let outcome = run_sandboxed(&root, &shim_dir, &[], "rm sub/note.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        let trash_root = root.join(".temp-trash");
        let mut timestamp_dirs: Vec<PathBuf> = fs::read_dir(&trash_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        timestamp_dirs.sort();
        assert_eq!(
            timestamp_dirs.len(),
            2,
            "expected two separate trash batches, got {timestamp_dirs:?}"
        );

        let contents: Vec<String> = timestamp_dirs
            .iter()
            .map(|d| fs::read_to_string(d.join("sub/note.txt")).unwrap())
            .collect();
        assert!(
            contents.contains(&"version1".to_string()),
            "first trashed copy was lost: {contents:?}"
        );
        assert!(
            contents.contains(&"version2".to_string()),
            "second trashed copy missing: {contents:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
