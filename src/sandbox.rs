//! Linux-only. Confines proposed commands to the working folder (plus
//! granted paths) with `bwrap`, and shims destructive tools to soft-delete
//! into `.temp-trash/`.
//!
//! We never try to judge "is this command safe" from its text. The sandbox
//! makes "outside the folder" impossible and the shims make "destructive
//! inside it" recoverable. The only text-based judgment is "can this run
//! without asking", which only has to be conservative in one direction.

use crate::config::GrantedPath;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const READ_ONLY_BINARIES: &[&str] = &[
    "ls", "cat", "grep", "head", "tail", "wc", "file", "stat", "tree", "pwd", "echo", "du", "df",
    "find", "uname", "whoami", "id", "hostname",
];

/// Any of these and it's no longer "just this one program".
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
    // `find` looks read-only but can delete/exec.
    if trimmed.contains("-delete") || trimmed.contains("-exec") {
        return Classification::NeedsConfirmation;
    }
    match first_binary(trimmed) {
        Some(bin) if READ_ONLY_BINARIES.contains(&bin.as_str()) => Classification::ReadOnly,
        _ => Classification::NeedsConfirmation,
    }
}

/// Metacharacters are still refused -- otherwise "always allow cat" becomes
/// "cat x > /etc/passwd".
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

// Every shim drops the shim dir from PATH first, or its own internal
// `mv`/`cp`/`mkdir` would re-enter the shim directory and loop forever.
const SHIM_PATH_RESET: &str = "PATH=/usr/bin:/bin:/usr/local/bin\nexport PATH\n";

// rm/rmdir: move the target into .temp-trash keeping its relative path, so a
// restore is just moving it back. Each invocation gets its own timestamped
// subfolder, or deleting the same path twice would have `mv -f` clobber the
// first copy.
//
// `rmdir` still refuses a non-empty directory, exactly as the real one does.
// That refusal is a signal the model actively relies on -- observed: asked to
// "clean up the leftover folders", it passed a directory full of the user's
// files alongside five empty ones, and a shim that silently trashed the lot
// turned a command that would have failed safely into one that reported
// success. Recoverable from the trash is not the same as not having happened.
const TRASH_SHIM_SCRIPT: &str = r#"tool="@TOOL@"
status=0
trash_root="${TRASH_ROOT:-$PWD/.temp-trash}/$(date +%Y%m%d-%H%M%S-%N)"
for arg in "$@"; do
  case "$arg" in
    -*) continue ;;
  esac
  if [ "$tool" = "rmdir" ] && [ -d "$arg" ] && [ -n "$(ls -A -- "$arg" 2>/dev/null)" ]; then
    echo "rmdir: failed to remove '$arg': Directory not empty" >&2
    status=1
    continue
  fi
  case "$arg" in
    /*) rel="${arg#/}" ;;
    *) rel="$arg" ;;
  esac
  dest="$trash_root/$rel"
  mkdir -p "$(dirname "$dest")"
  if ! mv -f -- "$arg" "$dest" 2>/dev/null; then
    echo "$tool: cannot remove '$arg'" >&2
    status=1
  fi
done
exit $status
"#;

/// mv/cp/truncate: copy what's about to be overwritten into `.temp-trash`,
/// then `exec` the real tool with `"$@"` untouched.
///
/// Operand scan: skip `-*` until a bare `--`, treat the last operand as the
/// destination (a directory means victims are `dest/basename(src)`). `@ALL@`
/// instead treats every operand as a victim, which is what `truncate` needs.
///
/// It mis-reads option *arguments* (`cp -t dir a b`) -- accepted, because the
/// cost is a needless trash copy, never a lost file or a changed command.
const PRESERVE_SHIM_SCRIPT: &str = r#"tool="@TOOL@"
real="$(command -v "$tool" 2>/dev/null)"
if [ -z "$real" ]; then
  echo "$tool: not available in this sandbox" >&2
  exit 127
fi

trash_root="${TRASH_ROOT:-$PWD/.temp-trash}/$(date +%Y%m%d-%H%M%S-%N)"

keep() {
  [ -e "$1" ] || return 0
  case "$1" in
    /*) rel="${1#/}" ;;
    *) rel="$1" ;;
  esac
  dest="$trash_root/$rel"
  mkdir -p "$(dirname "$dest")" 2>/dev/null || return 0
  cp -a -- "$1" "$dest" 2>/dev/null || :
}

last=""
count=0
opts_done=0
for arg in "$@"; do
  if [ "$opts_done" -eq 0 ]; then
    case "$arg" in
      --) opts_done=1; continue ;;
      -?*) continue ;;
    esac
  fi
  last="$arg"
  count=$((count + 1))
  if [ "@ALL@" = "yes" ]; then
    keep "$arg"
  fi
done

if [ "@ALL@" != "yes" ] && [ "$count" -ge 2 ]; then
  if [ -d "$last" ]; then
    # Destination is a directory, so each source lands inside it under its
    # own basename -- those are what get overwritten, not the directory.
    seen=0
    opts_done=0
    for arg in "$@"; do
      if [ "$opts_done" -eq 0 ]; then
        case "$arg" in
          --) opts_done=1; continue ;;
          -?*) continue ;;
        esac
      fi
      seen=$((seen + 1))
      [ "$seen" -eq "$count" ] && break
      keep "$last/$(basename -- "$arg")"
    done
  else
    keep "$last"
  fi
fi

exec "$real" "$@"
"#;

pub fn ensure_shims(shim_dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(shim_dir)?;
    let write_shim = |name: &str, body: String| -> anyhow::Result<()> {
        let shim = shim_dir.join(name);
        fs::write(&shim, format!("#!/bin/sh\n{SHIM_PATH_RESET}{body}"))?;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755))?;
        Ok(())
    };

    for name in ["rm", "rmdir"] {
        write_shim(name, TRASH_SHIM_SCRIPT.replace("@TOOL@", name))?;
    }
    // `all` = every operand is a victim (truncate rewrites its arguments in
    // place); otherwise only the destination is.
    for (name, all) in [("mv", "no"), ("cp", "no"), ("truncate", "yes")] {
        write_shim(
            name,
            PRESERVE_SHIM_SCRIPT
                .replace("@TOOL@", name)
                .replace("@ALL@", all),
        )?;
    }
    Ok(())
}

/// `scratch`, when given, is bound read-write -- the model's only writable
/// path outside the working folder. Passed in rather than read from
/// `memory::`, so the security module doesn't depend on the feature that
/// happens to use it.
pub fn run_sandboxed(
    root: &Path,
    shim_dir: &Path,
    granted: &[GrantedPath],
    scratch: Option<&Path>,
    cmd: &str,
) -> anyhow::Result<RunOutcome> {
    let mut c = Command::new("bwrap");
    // env_clear() keeps our environment out of bwrap; --clearenv keeps
    // anything out of the shell. Only PATH and TRASH_ROOT get through.
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

    // Scratch only. The rest of session memory stays unbound: `progress.md`
    // is worth trusting only if nothing but this app can write it.
    if let Some(scratch) = scratch.filter(|p| p.is_dir()) {
        c.arg("--bind").arg(scratch).arg(scratch);
    }

    for g in granted {
        let flag = if g.read_write { "--bind" } else { "--ro-bind" };
        let path = Path::new(&g.path);
        if !path.exists() {
            continue; // a stale grant shouldn't break every command
        }
        if g.recursive {
            c.arg(flag).arg(path).arg(path);
        } else {
            // Binds are recursive, so "just this directory" means binding
            // each top-level file individually.
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

    // Last, so it wins: binds apply in order, and a granted path that is an
    // ancestor of root (grant ~/src, open ~/src/playground) would otherwise
    // make root read-only.
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

/// Trashed files across all batches, so tests needn't know which one.
#[cfg(test)]
fn trashed_files(trash_root: &Path) -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out);
            } else if let Ok(text) = fs::read_to_string(&path) {
                out.push((path.strip_prefix(base).unwrap_or(&path).to_path_buf(), text));
            }
        }
    }
    let mut out = Vec::new();
    for batch in fs::read_dir(trash_root).into_iter().flatten().flatten() {
        let batch = batch.path();
        if batch.is_dir() {
            walk(&batch, &batch, &mut out);
        }
    }
    out
}

pub fn default_shim_dir() -> PathBuf {
    std::env::temp_dir().join("llm-assistant-shims")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread::sleep, time::Duration};

    /// Ubuntu 24.04 and many CI runners refuse the loopback setup that
    /// `--unshare-net` triggers. Tests skip rather than fail on those, since
    /// red there means "this machine won't run bwrap", not "the shim broke".
    /// The skip prints loudly -- a silent one looks exactly like passing.
    fn sandbox_available() -> bool {
        match Command::new("bwrap")
            .args(["--unshare-all", "--ro-bind", "/", "/", "/bin/true"])
            .output()
        {
            Ok(out) if out.status.success() => true,
            Ok(out) => {
                eprintln!(
                    "SKIPPING sandbox tests -- bwrap cannot create a sandbox here: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                false
            }
            Err(e) => {
                eprintln!("SKIPPING sandbox tests -- could not run bwrap at all: {e}");
                false
            }
        }
    }

    // Deleting the same path twice must land in two distinct trash batches,
    // not have `mv -f` overwrite the first.
    #[test]
    fn repeated_rm_of_same_path_does_not_clobber_earlier_trash() {
        if !sandbox_available() {
            return;
        }
        let root =
            std::env::temp_dir().join(format!("llm-assistant-sandbox-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/note.txt"), "version1").unwrap();

        let shim_dir = root.join("shims");
        ensure_shims(&shim_dir).unwrap();

        let outcome = run_sandboxed(&root, &shim_dir, &[], None, "rm sub/note.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        // Force a distinct nanosecond-precision timestamp for the second rm.
        sleep(Duration::from_millis(20));
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/note.txt"), "version2").unwrap();
        let outcome = run_sandboxed(&root, &shim_dir, &[], None, "rm sub/note.txt").unwrap();
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

    /// Fresh folder + shims, named per test so parallel runs don't collide.
    fn scratch(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "llm-assistant-sandbox-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let shim_dir = root.join("shims");
        ensure_shims(&shim_dir).unwrap();
        (root, shim_dir)
    }

    // `mv` over an existing target destroys it as permanently as `rm`, and
    // looks routine enough to be commonly auto-approved.
    #[test]
    fn mv_over_an_existing_file_keeps_the_overwritten_copy() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir) = scratch("mv-overwrite");
        fs::write(root.join("new.txt"), "incoming").unwrap();
        fs::write(root.join("old.txt"), "about to be destroyed").unwrap();

        let outcome = run_sandboxed(&root, &shim_dir, &[], None, "mv new.txt old.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(
            fs::read_to_string(root.join("old.txt")).unwrap(),
            "incoming",
            "the move itself must still happen exactly as asked"
        );

        let trashed = trashed_files(&root.join(".temp-trash"));
        assert!(
            trashed
                .iter()
                .any(|(_, text)| text == "about to be destroyed"),
            "the overwritten file should be recoverable from .temp-trash: {trashed:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mv_into_a_directory_keeps_the_file_it_replaces() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir) = scratch("mv-into-dir");
        fs::create_dir_all(root.join("dest")).unwrap();
        fs::write(root.join("note.txt"), "new version").unwrap();
        fs::write(root.join("dest/note.txt"), "old version").unwrap();

        let outcome = run_sandboxed(&root, &shim_dir, &[], None, "mv note.txt dest").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(
            fs::read_to_string(root.join("dest/note.txt")).unwrap(),
            "new version"
        );

        let trashed = trashed_files(&root.join(".temp-trash"));
        assert!(
            trashed.iter().any(|(_, text)| text == "old version"),
            "dest/note.txt should have been preserved before being replaced: {trashed:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cp_over_an_existing_file_keeps_the_overwritten_copy() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir) = scratch("cp-overwrite");
        fs::write(root.join("src.txt"), "incoming").unwrap();
        fs::write(root.join("dst.txt"), "about to be destroyed").unwrap();

        let outcome = run_sandboxed(&root, &shim_dir, &[], None, "cp src.txt dst.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(
            fs::read_to_string(root.join("dst.txt")).unwrap(),
            "incoming"
        );

        let trashed = trashed_files(&root.join(".temp-trash"));
        assert!(
            trashed
                .iter()
                .any(|(_, text)| text == "about to be destroyed"),
            "expected the clobbered destination in .temp-trash: {trashed:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    // Destroys content without deleting or replacing, so neither the rm shim
    // nor the destination logic catches it.
    #[test]
    fn truncate_keeps_the_contents_it_discards() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir) = scratch("truncate");
        fs::write(root.join("log.txt"), "important history").unwrap();

        let outcome = run_sandboxed(&root, &shim_dir, &[], None, "truncate -s 0 log.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(fs::read_to_string(root.join("log.txt")).unwrap(), "");

        let trashed = trashed_files(&root.join(".temp-trash"));
        assert!(
            trashed.iter().any(|(_, text)| text == "important history"),
            "expected the pre-truncation contents in .temp-trash: {trashed:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    // Regression test for the PATH reset: shims call mv/cp/mkdir internally,
    // and those are now shimmed too. Fails by hanging, not by a wrong answer.
    #[test]
    fn shims_do_not_recurse_into_each_other() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir) = scratch("shim-recursion");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/a.txt"), "content").unwrap();

        let outcome = run_sandboxed(&root, &shim_dir, &[], None, "rm sub/a.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        fs::write(root.join("x.txt"), "x").unwrap();
        fs::write(root.join("y.txt"), "y").unwrap();
        let outcome = run_sandboxed(&root, &shim_dir, &[], None, "mv x.txt y.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(fs::read_to_string(root.join("y.txt")).unwrap(), "x");

        let _ = fs::remove_dir_all(&root);
    }

    // Observed in a real session: asked to clean up leftover folders, the
    // model passed a directory full of the user's files alongside five empty
    // ones. Real rmdir refuses that; a shim that trashes it anyway turns a
    // command that would have failed safely into one that reports success.
    #[test]
    fn rmdir_refuses_a_non_empty_directory() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir) = scratch("rmdir-non-empty");
        fs::create_dir_all(root.join("empty_one")).unwrap();
        fs::create_dir_all(root.join("archive")).unwrap();
        fs::write(root.join("archive/keep.txt"), "the user's file").unwrap();

        let outcome =
            run_sandboxed(&root, &shim_dir, &[], None, "rmdir empty_one archive").unwrap();
        assert_ne!(outcome.exit_code, 0, "must fail like the real rmdir");
        assert!(
            outcome.stderr.contains("Directory not empty"),
            "stderr: {}",
            outcome.stderr
        );
        assert!(
            root.join("archive/keep.txt").exists(),
            "a non-empty directory must be left completely alone"
        );
        assert!(
            !root.join("empty_one").exists(),
            "the empty one should still have been trashed"
        );

        let _ = fs::remove_dir_all(&root);
    }

    // rmdir needs the same structural redirect as rm: a real one removes its
    // target with no recovery path.
    #[test]
    fn rmdir_moves_target_to_trash_instead_of_removing_it() {
        if !sandbox_available() {
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "llm-assistant-sandbox-rmdir-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("empty_folder")).unwrap();

        let shim_dir = root.join("shims");
        ensure_shims(&shim_dir).unwrap();

        let outcome = run_sandboxed(&root, &shim_dir, &[], None, "rmdir empty_folder").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        assert!(
            !root.join("empty_folder").exists(),
            "empty_folder should no longer be at its original location"
        );
        let trash_root = root.join(".temp-trash");
        let moved: Vec<PathBuf> = fs::read_dir(&trash_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().join("empty_folder"))
            .filter(|p| p.is_dir())
            .collect();
        assert_eq!(
            moved.len(),
            1,
            "expected empty_folder to land in exactly one trash batch, got {moved:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
