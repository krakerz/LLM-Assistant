//! Linux-only: confines shell commands proposed by the LLM to the selected
//! working directory (plus explicitly granted paths) using `bwrap`, and
//! shims `rm`/`rmdir` inside the jail to soft-delete into `.temp-trash/`
//! instead of actually removing files. See project brainstorm notes for the
//! rationale:
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

// Shared by both the `rm` and `rmdir` shims below -- moves whatever's named
// into .temp-trash instead of unlinking/removing it, preserving each
// target's path relative to the sandbox root so a manual restore just means
// moving it back. Every invocation gets its own timestamped subfolder (all
// targets of one call share it) so deleting the same path twice can never
// silently clobber the earlier trashed copy -- mv -f below would otherwise
// overwrite it without a trace. Note this means `rmdir` no longer fails on
// a non-empty directory the way real rmdir would -- it always succeeds and
// moves the whole thing to trash regardless of contents, trading that
// specific signal for the same "nothing is ever truly gone" guarantee `rm`
// already gets.
// Dropping the shim directory from PATH is the first thing every shim does.
// Without it a shim's own internal `mv`/`cp`/`mkdir` would resolve straight
// back into the shim directory -- harmless while only rm/rmdir were shimmed,
// but an infinite loop the moment `mv` and `cp` are too. The reduced PATH is
// inherited by the real tool we hand off to, which is fine: none of these
// shell out to anything.
const SHIM_PATH_RESET: &str = "PATH=/usr/bin:/bin:/usr/local/bin\nexport PATH\n";

const TRASH_SHIM_SCRIPT: &str = r#"trash_root="${TRASH_ROOT:-$PWD/.temp-trash}/$(date +%Y%m%d-%H%M%S-%N)"
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

/// The other half of the soft-delete guarantee: `rm` isn't the only way to
/// destroy a file. `mv a b` and `cp a b` silently overwrite `b`, and
/// `truncate -s 0 b` empties it in place -- all unrecoverable, and all
/// previously gated only by the confirmation dialog. These shims copy
/// whatever is about to be overwritten into `.temp-trash` first and then hand
/// off to the real tool, so approving one of those still leaves a way back.
///
/// The operand scan is deliberately simple: skip anything starting with `-`
/// (until a bare `--`), then treat the last remaining operand as the
/// destination -- a directory means the victims are `dest/basename(src)`,
/// otherwise the destination itself is the victim. `@ALL@` mode instead
/// treats every operand as a victim, which is what `truncate` needs.
///
/// That scan gets option *arguments* wrong (`cp -t dir a b` puts the target
/// in a place this doesn't understand), and that's an accepted limitation
/// rather than an oversight: the consequence is copying a source file into
/// the trash that didn't need saving. Wasted space, never a lost file, and
/// never a changed command -- the real tool always receives `"$@"` untouched,
/// so what the user approved is exactly what runs.
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
        write_shim(name, TRASH_SHIM_SCRIPT.to_string())?;
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

/// Only the files a trash batch actually holds, relative to the batch dir --
/// used by the tests to say what was preserved without caring which
/// timestamped subfolder it landed in.
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

    /// Whether this machine will actually let `bwrap` build a sandbox right
    /// now. Not every environment does: Ubuntu 24.04 restricts unprivileged
    /// user namespaces through AppArmor, and container/CI environments often
    /// refuse at the loopback-setup step that `--unshare-net` triggers
    /// (`bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted`).
    ///
    /// The tests below use this to skip rather than fail, because a red test
    /// there says "this machine won't run bwrap", not "the shim is broken" --
    /// and the second message is the one this suite exists to deliver. The
    /// skip is printed loudly on purpose: a silent skip of the sandbox tests
    /// is indistinguishable from them passing, so CI enables unprivileged
    /// user namespaces up front (see `.github/workflows/autobuild.yml`) and
    /// the skip line is how you find out that didn't work.
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

    // Proves the timestamped-subfolder fix for real, inside the actual
    // bwrap sandbox: deleting the same relative path twice must land in two
    // distinct .temp-trash subfolders, not silently overwrite the first
    // trashed copy via mv -f. Requires bwrap and coreutils' `date`, same as
    // the app itself does at runtime.
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

    /// Fresh working folder + shims for one test, named after the test so
    /// concurrent runs don't share a directory.
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

    // `rm` was never the only way to lose a file: `mv` over an existing
    // target destroys it just as permanently, and unlike `rm` it looks
    // routine enough that it's a common thing to auto-approve.
    #[test]
    fn mv_over_an_existing_file_keeps_the_overwritten_copy() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir) = scratch("mv-overwrite");
        fs::write(root.join("new.txt"), "incoming").unwrap();
        fs::write(root.join("old.txt"), "about to be destroyed").unwrap();

        let outcome = run_sandboxed(&root, &shim_dir, &[], "mv new.txt old.txt").unwrap();
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

        let outcome = run_sandboxed(&root, &shim_dir, &[], "mv note.txt dest").unwrap();
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

        let outcome = run_sandboxed(&root, &shim_dir, &[], "cp src.txt dst.txt").unwrap();
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

    // truncate destroys content without deleting or replacing anything, so
    // neither the rm shim nor the destination logic above would catch it.
    #[test]
    fn truncate_keeps_the_contents_it_discards() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir) = scratch("truncate");
        fs::write(root.join("log.txt"), "important history").unwrap();

        let outcome = run_sandboxed(&root, &shim_dir, &[], "truncate -s 0 log.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(fs::read_to_string(root.join("log.txt")).unwrap(), "");

        let trashed = trashed_files(&root.join(".temp-trash"));
        assert!(
            trashed.iter().any(|(_, text)| text == "important history"),
            "expected the pre-truncation contents in .temp-trash: {trashed:?}"
        );

        let _ = fs::remove_dir_all(&root);
    }

    // The shims call mv/cp/mkdir internally. Now that mv and cp are
    // themselves shimmed, an unqualified call would re-enter the shim
    // directory -- this is the regression test for the PATH reset that stops
    // that, and it fails by hanging or blowing the stack rather than
    // returning a wrong answer.
    #[test]
    fn shims_do_not_recurse_into_each_other() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir) = scratch("shim-recursion");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/a.txt"), "content").unwrap();

        // rm's internal `mv`, then mv's internal `cp`, then cp's internal
        // `cp` -- each one a chance to land back in the shim directory.
        let outcome = run_sandboxed(&root, &shim_dir, &[], "rm sub/a.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        fs::write(root.join("x.txt"), "x").unwrap();
        fs::write(root.join("y.txt"), "y").unwrap();
        let outcome = run_sandboxed(&root, &shim_dir, &[], "mv x.txt y.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(fs::read_to_string(root.join("y.txt")).unwrap(), "x");

        let _ = fs::remove_dir_all(&root);
    }

    // rmdir isn't just gated behind confirmation like an unshimmed
    // destructive command -- it needs to be structurally redirected the
    // same way rm is, since a real rmdir permanently (if harmlessly, for an
    // empty dir) removes its target with no recovery path.
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

        let outcome = run_sandboxed(&root, &shim_dir, &[], "rmdir empty_folder").unwrap();
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
