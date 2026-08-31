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
    "find",
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
# restore just means moving them back.
trash_root="${TRASH_ROOT:-$PWD/.temp-trash}"
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
    c.arg("--die-with-parent")
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

    c.arg("--bind").arg(root).arg(root);
    c.arg("--ro-bind").arg(shim_dir).arg(shim_dir);

    for g in granted {
        let flag = if g.read_write { "--bind" } else { "--ro-bind" };
        c.arg(flag).arg(&g.path).arg(&g.path);
    }

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
