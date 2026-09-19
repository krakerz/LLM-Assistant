//! Linux-only. Confines proposed commands to the working folder (plus
//! granted paths) with `bwrap`, and shims destructive tools to soft-delete
//! into an app-managed trash directory mounted at `/trash` inside the
//! sandbox -- deliberately not a child of the working folder itself (see
//! `resolve_trash_dir`/`run_sandboxed`'s doc comments for why).
//!
//! We never try to judge "is this command safe" from its text. The sandbox
//! makes "outside the folder" impossible and the shims make "destructive
//! inside it" recoverable. The only text-based judgment is "can this run
//! without asking", which only has to be conservative in one direction.

use crate::config::{AppConfig, GrantedPath};
use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hasher};
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

// rm/rmdir: move the target into the trash directory `run_sandboxed` bound
// in at `TRASH_ROOT`, keeping its relative path, so a restore is just moving
// it back. `TRASH_ROOT` is already unique per command execution (see
// `run_sandboxed`), so this shim doesn't mint its own subfolder the way it
// used to -- but it still has to guard against trashing the *same* relative
// path twice within one execution (e.g. `rm a; touch a; rm a`), which would
// otherwise have the second `mv -f` clobber the first copy: on a collision,
// append `.2`, `.3`, ... until a free name turns up.
//
// `rmdir` still refuses a non-empty directory, exactly as the real one does.
// That refusal is a signal the model actively relies on -- observed: asked to
// "clean up the leftover folders", it passed a directory full of the user's
// files alongside five empty ones, and a shim that silently trashed the lot
// turned a command that would have failed safely into one that reported
// success. Recoverable from the trash is not the same as not having happened.
const TRASH_SHIM_SCRIPT: &str = r#"tool="@TOOL@"
status=0
trash_root="${TRASH_ROOT:-$PWD/.temp-trash}"
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
  if [ -e "$dest" ]; then
    n=2
    while [ -e "$dest.$n" ]; do n=$((n + 1)); done
    dest="$dest.$n"
  fi
  mkdir -p "$(dirname "$dest")"
  if ! mv -f -- "$arg" "$dest" 2>/dev/null; then
    echo "$tool: cannot remove '$arg'" >&2
    status=1
  fi
done
exit $status
"#;

/// mv/cp/truncate: copy what's about to be overwritten into the trash
/// directory `run_sandboxed` bound in at `TRASH_ROOT`, then `exec` the real
/// tool with `"$@"` untouched. Same collision handling as `TRASH_SHIM_SCRIPT`
/// -- `TRASH_ROOT` is fixed for the whole command execution now, so this
/// guards against overwriting the same relative path's trashed copy twice
/// in one run.
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

trash_root="${TRASH_ROOT:-$PWD/.temp-trash}"

keep() {
  [ -e "$1" ] || return 0
  case "$1" in
    /*) rel="${1#/}" ;;
    *) rel="$1" ;;
  esac
  dest="$trash_root/$rel"
  if [ -e "$dest" ]; then
    n=2
    while [ -e "$dest.$n" ]; do n=$((n + 1)); done
    dest="$dest.$n"
  fi
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

/// `cfg.sandbox_trash_dir` empty means the built-in default -- resolved here
/// (not stored pre-resolved anywhere) so a config change takes effect on the
/// very next command, same "never trust a stale copy" convention the rest of
/// the app follows. Passed into `run_sandboxed` by the caller rather than
/// read from `AppConfig` directly inside it, matching how `granted`/`scratch`
/// already work -- this module stays config-shape-agnostic beyond the one
/// field it actually needs.
pub fn resolve_trash_dir(cfg: &AppConfig) -> PathBuf {
    if cfg.sandbox_trash_dir.trim().is_empty() {
        crate::paths::app_config_dir().join("trash")
    } else {
        PathBuf::from(&cfg.sandbox_trash_dir)
    }
}

/// One identifier per `run_sandboxed` call -- day-granularity date (not a
/// fine-grained timestamp, so the trash directory doesn't fragment into one
/// folder per second) plus a short random suffix for uniqueness between
/// runs on the same day. Same dependency-free trick `server.rs`'s
/// `generate_session_token` uses (`RandomState`'s per-call OS-seeded key),
/// just one draw instead of four -- a trash folder name doesn't need
/// session-token-grade entropy, only enough that two runs on the same day
/// never collide.
fn generate_trash_run_id() -> String {
    let date = chrono::Local::now().format("%Y%m%d");
    let random = RandomState::new().build_hasher().finish();
    format!("{date}-{random:016x}")
}

/// `scratch`, when given, is bound read-write -- the model's only other
/// writable path outside the working folder. Passed in rather than read
/// from `memory::`, so the security module doesn't depend on the feature
/// that happens to use it.
///
/// `trash_base` (see `resolve_trash_dir`) is where trash for *this* run
/// lands, but only this run's own subfolder is ever bound into the sandbox
/// -- at a fixed path (`/trash`) that is deliberately not a child of `root`
/// or anywhere else the sandboxed process would think to look. A plain
/// `ls`/`ls -R` from inside the working folder can never show it, and even
/// a command that explicitly tried `ls /trash` would only ever see this
/// one run's own trashed files, never another run's or another folder's --
/// added after a real session showed the model repeatedly listing the old
/// `.temp-trash/` (which lived inside the working folder itself) despite an
/// explicit instruction to leave it alone.
pub fn run_sandboxed(
    root: &Path,
    shim_dir: &Path,
    granted: &[GrantedPath],
    scratch: Option<&Path>,
    trash_base: &Path,
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

    let run_trash_dir = trash_base.join(generate_trash_run_id());
    fs::create_dir_all(&run_trash_dir)?;
    let sandbox_trash_path = Path::new("/trash");
    c.arg("--bind").arg(&run_trash_dir).arg(sandbox_trash_path);

    let path_env = format!("{}:/usr/bin:/bin", shim_dir.display());

    c.arg("--chdir")
        .arg(root)
        .arg("--setenv")
        .arg("PATH")
        .arg(&path_env)
        .arg("--setenv")
        .arg("TRASH_ROOT")
        .arg(sandbox_trash_path)
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

    // Deleting the same path twice, in two separate approved commands, must
    // land in two distinct trash batches, not have `mv -f` overwrite the
    // first. Each `run_sandboxed` call draws its own random run ID, so --
    // unlike the old per-shim nanosecond timestamp -- this no longer needs
    // to force a delay between the two calls to guarantee they differ.
    #[test]
    fn repeated_rm_of_same_path_does_not_clobber_earlier_trash() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("repeated-rm");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/note.txt"), "version1").unwrap();

        let outcome =
            run_sandboxed(&root, &shim_dir, &[], None, &trash_base, "rm sub/note.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/note.txt"), "version2").unwrap();
        let outcome =
            run_sandboxed(&root, &shim_dir, &[], None, &trash_base, "rm sub/note.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        let mut run_dirs: Vec<PathBuf> = fs::read_dir(&trash_base)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        run_dirs.sort();
        assert_eq!(
            run_dirs.len(),
            2,
            "expected two separate trash batches, got {run_dirs:?}"
        );

        let contents: Vec<String> = run_dirs
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
        let _ = fs::remove_dir_all(&trash_base);
    }

    // Trashing the *same* relative path twice within one command execution
    // (one `run_sandboxed` call, so one fixed `TRASH_ROOT` for both deletes)
    // must not have the second `mv -f` clobber the first -- the shim's own
    // collision suffix (`.2`, `.3`, ...) is what's supposed to catch this
    // now that a fresh per-shim-call timestamp no longer does.
    #[test]
    fn same_path_trashed_twice_in_one_command_keeps_both_copies() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("same-path-one-run");
        fs::write(root.join("note.txt"), "version1").unwrap();

        let outcome = run_sandboxed(
            &root,
            &shim_dir,
            &[],
            None,
            &trash_base,
            "rm note.txt && echo version2 > note.txt && rm note.txt",
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        let trashed = trashed_files(&trash_base);
        assert_eq!(
            trashed.len(),
            2,
            "expected both trashed copies to survive under distinct names: {trashed:?}"
        );
        let contents: Vec<&String> = trashed.iter().map(|(_, text)| text).collect();
        assert!(
            contents.contains(&&"version1\n".to_string())
                || contents.contains(&&"version1".to_string()),
            "first trashed copy was lost: {trashed:?}"
        );
        assert!(
            contents.contains(&&"version2\n".to_string()),
            "second trashed copy was lost: {trashed:?}"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&trash_base);
    }

    /// Fresh folder + shims + an external trash base, named per test so
    /// parallel runs don't collide. The trash base is deliberately a
    /// sibling of `root`, not a child of it -- exercising the same
    /// outside-the-working-folder shape `run_sandboxed` actually uses now,
    /// not just a renamed `.temp-trash`.
    fn scratch(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "llm-assistant-sandbox-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let shim_dir = root.join("shims");
        ensure_shims(&shim_dir).unwrap();
        let trash_base = std::env::temp_dir().join(format!(
            "llm-assistant-sandbox-{name}-trash-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&trash_base);
        (root, shim_dir, trash_base)
    }

    // `mv` over an existing target destroys it as permanently as `rm`, and
    // looks routine enough to be commonly auto-approved.
    #[test]
    fn mv_over_an_existing_file_keeps_the_overwritten_copy() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("mv-overwrite");
        fs::write(root.join("new.txt"), "incoming").unwrap();
        fs::write(root.join("old.txt"), "about to be destroyed").unwrap();

        let outcome = run_sandboxed(
            &root,
            &shim_dir,
            &[],
            None,
            &trash_base,
            "mv new.txt old.txt",
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(
            fs::read_to_string(root.join("old.txt")).unwrap(),
            "incoming",
            "the move itself must still happen exactly as asked"
        );

        let trashed = trashed_files(&trash_base);
        assert!(
            trashed
                .iter()
                .any(|(_, text)| text == "about to be destroyed"),
            "the overwritten file should be recoverable from trash: {trashed:?}"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&trash_base);
    }

    #[test]
    fn mv_into_a_directory_keeps_the_file_it_replaces() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("mv-into-dir");
        fs::create_dir_all(root.join("dest")).unwrap();
        fs::write(root.join("note.txt"), "new version").unwrap();
        fs::write(root.join("dest/note.txt"), "old version").unwrap();

        let outcome =
            run_sandboxed(&root, &shim_dir, &[], None, &trash_base, "mv note.txt dest").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(
            fs::read_to_string(root.join("dest/note.txt")).unwrap(),
            "new version"
        );

        let trashed = trashed_files(&trash_base);
        assert!(
            trashed.iter().any(|(_, text)| text == "old version"),
            "dest/note.txt should have been preserved before being replaced: {trashed:?}"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&trash_base);
    }

    #[test]
    fn cp_over_an_existing_file_keeps_the_overwritten_copy() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("cp-overwrite");
        fs::write(root.join("src.txt"), "incoming").unwrap();
        fs::write(root.join("dst.txt"), "about to be destroyed").unwrap();

        let outcome = run_sandboxed(
            &root,
            &shim_dir,
            &[],
            None,
            &trash_base,
            "cp src.txt dst.txt",
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(
            fs::read_to_string(root.join("dst.txt")).unwrap(),
            "incoming"
        );

        let trashed = trashed_files(&trash_base);
        assert!(
            trashed
                .iter()
                .any(|(_, text)| text == "about to be destroyed"),
            "expected the clobbered destination in trash: {trashed:?}"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&trash_base);
    }

    // Destroys content without deleting or replacing, so neither the rm shim
    // nor the destination logic catches it.
    #[test]
    fn truncate_keeps_the_contents_it_discards() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("truncate");
        fs::write(root.join("log.txt"), "important history").unwrap();

        let outcome = run_sandboxed(
            &root,
            &shim_dir,
            &[],
            None,
            &trash_base,
            "truncate -s 0 log.txt",
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(fs::read_to_string(root.join("log.txt")).unwrap(), "");

        let trashed = trashed_files(&trash_base);
        assert!(
            trashed.iter().any(|(_, text)| text == "important history"),
            "expected the pre-truncation contents in trash: {trashed:?}"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&trash_base);
    }

    // Regression test for the PATH reset: shims call mv/cp/mkdir internally,
    // and those are now shimmed too. Fails by hanging, not by a wrong answer.
    #[test]
    fn shims_do_not_recurse_into_each_other() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("shim-recursion");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/a.txt"), "content").unwrap();

        let outcome =
            run_sandboxed(&root, &shim_dir, &[], None, &trash_base, "rm sub/a.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        fs::write(root.join("x.txt"), "x").unwrap();
        fs::write(root.join("y.txt"), "y").unwrap();
        let outcome =
            run_sandboxed(&root, &shim_dir, &[], None, &trash_base, "mv x.txt y.txt").unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(fs::read_to_string(root.join("y.txt")).unwrap(), "x");

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&trash_base);
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
        let (root, shim_dir, trash_base) = scratch("rmdir-non-empty");
        fs::create_dir_all(root.join("empty_one")).unwrap();
        fs::create_dir_all(root.join("archive")).unwrap();
        fs::write(root.join("archive/keep.txt"), "the user's file").unwrap();

        let outcome = run_sandboxed(
            &root,
            &shim_dir,
            &[],
            None,
            &trash_base,
            "rmdir empty_one archive",
        )
        .unwrap();
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
        let _ = fs::remove_dir_all(&trash_base);
    }

    // rmdir needs the same structural redirect as rm: a real one removes its
    // target with no recovery path.
    #[test]
    fn rmdir_moves_target_to_trash_instead_of_removing_it() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("rmdir-moves");
        fs::create_dir_all(root.join("empty_folder")).unwrap();

        let outcome = run_sandboxed(
            &root,
            &shim_dir,
            &[],
            None,
            &trash_base,
            "rmdir empty_folder",
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        assert!(
            !root.join("empty_folder").exists(),
            "empty_folder should no longer be at its original location"
        );
        let moved: Vec<PathBuf> = fs::read_dir(&trash_base)
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
        let _ = fs::remove_dir_all(&trash_base);
    }
}
