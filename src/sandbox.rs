//! Linux-only. Runs `run_command` in `bwrap`, confined to the working folder
//! plus granted paths, with no network. `rm`/`rmdir`/`mv`/`cp`/`truncate`
//! are shimmed to keep a copy of whatever they destroy in a per-run trash
//! dir (`/trash` inside the sandbox, outside the working folder on disk).
//! The shims do NOT cover redirects, `sed -i`, interpreters or absolute
//! paths to the real tools; file edits go through `tools.rs`, which backs up
//! itself.
//!
//! We never judge "is this command safe" from its text. The only text-based
//! judgment is "can this run without asking", which must only be
//! conservative in one direction.
use crate::config::{AppConfig, GrantedPath};
use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Fixed in-sandbox mount point for the shims; never under a user path.
pub const SANDBOX_SHIM_DIR: &str = "/run/llm-shims";
const SANDBOX_TRASH_DIR: &str = "/trash";

/// Per stream: keep this much from the start and from the end.
const OUTPUT_KEEP: usize = 32 * 1024;
/// How long to wait for the pipe readers after the child is gone.
const READER_GRACE: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

const READ_ONLY_BINARIES: &[&str] = &[
    "ls", "cat", "grep", "head", "tail", "wc", "file", "stat", "tree", "pwd", "echo", "du", "df",
    "find", "uname", "whoami", "id", "hostname",
];

/// "Always allow" on any of these would mean "always allow anything".
const NEVER_AUTO_APPROVE: &[&str] = &[
    "sh", "bash", "zsh", "dash", "env", "xargs", "timeout", "nice", "nohup", "perl", "ruby",
    "node", "awk", "sed", "find", "busybox", "sudo",
];

/// Any of these and it's no longer "just this one program".
const SHELL_METACHARACTERS: &[&str] = &["|", ">", "<", "&", ";", "`", "$(", "\n", "\r"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Classification {
    ReadOnly,
    NeedsConfirmation,
}

fn has_metacharacters(cmd: &str) -> bool {
    SHELL_METACHARACTERS.iter().any(|m| cmd.contains(m))
}

/// The program word, if it is a bare name: `./ls` or `/dir/cat` could be a
/// script in the working folder.
fn bare_program(tokens: &[String]) -> Option<&str> {
    let first = tokens.first()?;
    (!first.is_empty() && !first.contains('/')).then_some(first.as_str())
}

/// `-abc` style bundle (not `--long`): the flag letters.
fn short_flags(arg: &str) -> Option<&str> {
    (arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1).then(|| &arg[1..])
}

/// Arguments that turn an otherwise read-only program into a writer or a
/// non-terminating one.
fn dangerous_args(program: &str, args: &[String]) -> bool {
    args.iter().any(|a| match program {
        "find" => ["-delete", "-exec", "-ok", "-fprint", "-fls"]
            .iter()
            .any(|p| a.starts_with(p)),
        "tree" => a.starts_with("--output") || short_flags(a).is_some_and(|f| f.contains('o')),
        "tail" => {
            a.starts_with("--follow")
                || short_flags(a).is_some_and(|f| f.contains('f') || f.contains('F'))
        }
        "file" => a == "--compile" || short_flags(a).is_some_and(|f| f.contains('C')),
        _ => false,
    })
}

pub fn classify_command(cmd: &str) -> Classification {
    let trimmed = cmd.trim();
    if trimmed.is_empty() || has_metacharacters(trimmed) {
        return Classification::NeedsConfirmation;
    }
    let Ok(tokens) = shell_words::split(trimmed) else {
        return Classification::NeedsConfirmation;
    };
    match bare_program(&tokens) {
        Some(bin) if READ_ONLY_BINARIES.contains(&bin) && !dangerous_args(bin, &tokens[1..]) => {
            Classification::ReadOnly
        }
        _ => Classification::NeedsConfirmation,
    }
}

/// Whether "always allow <program>" may be offered or honoured.
pub fn can_auto_approve_program(p: &str) -> bool {
    let p = p.trim();
    !p.is_empty()
        && !p.contains('/')
        && !p.starts_with("python")
        && !NEVER_AUTO_APPROVE.contains(&p)
}

/// Metacharacters are still refused -- otherwise "always allow cat" becomes
/// "cat x > /etc/passwd".
pub fn is_auto_approved(cmd: &str, auto_approve: &[String]) -> bool {
    let trimmed = cmd.trim();
    if trimmed.is_empty() || has_metacharacters(trimmed) {
        return false;
    }
    let Ok(tokens) = shell_words::split(trimmed) else {
        return false;
    };
    match bare_program(&tokens) {
        Some(bin) => {
            can_auto_approve_program(bin)
                && !dangerous_args(bin, &tokens[1..])
                && auto_approve.iter().any(|b| b == bin)
        }
        None => false,
    }
}

/// Splits on unquoted `; | & ( ) `` ` and newlines (so `&&`/`||` too).
fn split_segments(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut single, mut double, mut escaped) = (false, false, false);
    for ch in cmd.chars() {
        if escaped {
            escaped = false;
            cur.push(ch);
            continue;
        }
        match ch {
            '\\' if !single => {
                escaped = true;
                cur.push(ch);
            }
            '\'' if !double => {
                single = !single;
                cur.push(ch);
            }
            '"' if !single => {
                double = !double;
                cur.push(ch);
            }
            ';' | '|' | '&' | '\n' | '(' | ')' | '`' if !single && !double => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    out.push(cur);
    out
}

/// The privilege-escalation program, if any command-position word is one.
/// Only command position counts, so `grep sudo notes` is fine.
pub fn needs_privilege(cmd: &str) -> Option<String> {
    const PRIVILEGED: &[&str] = &["sudo", "su", "doas", "pkexec"];
    const PREFIXES: &[&str] = &["command", "exec", "env", "nohup", "time", "builtin"];
    for seg in split_segments(cmd) {
        let Ok(words) = shell_words::split(&seg) else {
            continue;
        };
        let mut i = words.iter().take_while(|w| is_env_assignment(w)).count();
        while let Some(w) = words.get(i) {
            let base = Path::new(w)
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or(w);
            if PRIVILEGED.contains(&base) {
                return Some(base.to_string());
            }
            if !PREFIXES.contains(&base) {
                break;
            }
            // Skip the wrapper's own flags/assignments to reach the program.
            i += 1;
            while words
                .get(i)
                .is_some_and(|w| w.starts_with('-') || is_env_assignment(w))
            {
                i += 1;
            }
        }
    }
    None
}

fn is_env_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((k, _)) => {
            !k.is_empty()
                && !k.starts_with(|c: char| c.is_ascii_digit())
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Root safety
// ---------------------------------------------------------------------------

/// Canonicalizes the deepest existing ancestor and re-appends the rest, after
/// resolving `.`/`..` lexically. A dangling symlink is an error (we can't
/// tell where it would land).
pub(crate) fn canonicalize_lenient(path: &Path) -> std::io::Result<PathBuf> {
    let mut norm = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                norm.pop();
            }
            Component::CurDir => {}
            c => norm.push(c.as_os_str()),
        }
    }
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = norm;
    loop {
        match fs::canonicalize(&cur) {
            Ok(mut canon) => {
                for part in rest.iter().rev() {
                    canon.push(part);
                }
                return Ok(canon);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(20) => {
                if fs::symlink_metadata(&cur).is_ok() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("'{}' is a broken symlink", cur.display()),
                    ));
                }
                match (cur.file_name().map(|n| n.to_os_string()), cur.parent()) {
                    (Some(name), Some(parent)) => {
                        rest.push(name);
                        cur = parent.to_path_buf();
                    }
                    _ => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
}

fn canon_or_self(p: &Path) -> PathBuf {
    canonicalize_lenient(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Refuses folders whose sandbox would expose the app's own data or the
/// whole machine: `/`, `$HOME` itself, any ancestor of the app config dir,
/// or anything inside it.
pub fn check_root_allowed(root: &Path) -> Result<(), String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    check_root_allowed_with(root, home.as_deref(), &crate::paths::app_config_dir())
}

fn check_root_allowed_with(
    root: &Path,
    home: Option<&Path>,
    config_dir: &Path,
) -> Result<(), String> {
    let root_c = canon_or_self(root);
    if root_c == Path::new("/") {
        return Err(
            "the whole filesystem ('/') can't be the working folder; pick a more specific folder"
                .into(),
        );
    }
    if let Some(home) = home.filter(|h| !h.as_os_str().is_empty()) {
        if root_c == canon_or_self(home) || root == home {
            return Err(
                "your home folder itself can't be the working folder; pick a folder inside it"
                    .into(),
            );
        }
    }
    let cfg_c = canon_or_self(config_dir);
    for r in [root, root_c.as_path()] {
        for c in [config_dir, cfg_c.as_path()] {
            if c.starts_with(r) {
                return Err(format!(
                    "'{}' contains the app's own settings and data; pick a different folder",
                    root.display()
                ));
            }
            if r.starts_with(c) {
                return Err(format!(
                    "'{}' is inside the app's own settings and data folder; pick a different folder",
                    root.display()
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shims
// ---------------------------------------------------------------------------

// Every shim drops the shim dir from PATH first, or its own internal
// `mv`/`cp`/`mkdir` would re-enter the shim directory and loop forever.
const SHIM_PATH_RESET: &str = "PATH=/usr/bin:/bin:/usr/local/bin\nexport PATH\n";

// Trash destination under `TRASH_ROOT` for an operand: leading `/`, `.` and
// `..` components are dropped. Without that, `rm ../x` would build
// `/trash/../x`, which is the sandbox's ephemeral root, not the trash: the
// "backup" would vanish with the run. A subshell, so IFS/noglob don't leak.
const SHIM_HELPERS: &str = r#"safe_rel() (
  out=""
  IFS=/
  set -f
  for part in $1; do
    case "$part" in ""|.|..) continue ;; esac
    out="${out:+$out/}$part"
  done
  if [ -z "$out" ]; then
    out="$(basename -- "$1" 2>/dev/null)"
    case "$out" in ""|.|..|/) out="item" ;; esac
  fi
  printf '%s' "$out"
)
"#;

// rm/rmdir: move the target into `TRASH_ROOT` (this run's trash dir), keeping
// its relative path so a restore is just moving it back. No fallback when
// `TRASH_ROOT` is unset: failing loudly beats silently trashing somewhere
// unexpected. A repeat of the same path within one run gets `.2`, `.3`, ...
//
// Mirrors real `rm`: `-f` on a missing operand is silent success, operands
// after `--` are operands even if they start with `-`.
//
// `rmdir` refuses a non-empty directory like the real one: the refusal is a
// signal the model relies on.
const TRASH_SHIM_SCRIPT: &str = r#"tool="@TOOL@"
trash_root="${TRASH_ROOT:?TRASH_ROOT is not set; this shim only works inside the sandbox}"
status=0
force=0
opts_done=0
for arg in "$@"; do
  if [ "$opts_done" -eq 0 ]; then
    case "$arg" in
      --) opts_done=1; continue ;;
      --force) force=1; continue ;;
      --*) continue ;;
      -?*) case "$arg" in *f*) force=1 ;; esac; continue ;;
    esac
  fi
  if [ ! -e "$arg" ] && [ ! -L "$arg" ]; then
    if [ "$force" -eq 0 ]; then
      echo "$tool: cannot remove '$arg': No such file or directory" >&2
      status=1
    fi
    continue
  fi
  if [ "$tool" = "rmdir" ]; then
    if [ ! -d "$arg" ]; then
      echo "rmdir: failed to remove '$arg': Not a directory" >&2
      status=1
      continue
    fi
    if [ -n "$(ls -A -- "$arg" 2>/dev/null)" ]; then
      echo "rmdir: failed to remove '$arg': Directory not empty" >&2
      status=1
      continue
    fi
  fi
  rel="$(safe_rel "$arg")"
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

/// mv/cp/truncate: copy what's about to be overwritten into `TRASH_ROOT`,
/// then `exec` the real tool with `"$@"` untouched.
///
/// Operand scan: skip `-*` until a bare `--`, treat the last operand as the
/// destination (a directory means victims are `dest/basename(src)`). `@ALL@`
/// instead treats every operand as a victim, which `truncate` needs.
///
/// It mis-reads option *arguments* (`cp -t dir a b`) -- accepted, because the
/// cost is a needless trash copy, never a lost file or a changed command.
const PRESERVE_SHIM_SCRIPT: &str = r#"tool="@TOOL@"
real="$(command -v "$tool" 2>/dev/null)"
if [ -z "$real" ]; then
  echo "$tool: not available in this sandbox" >&2
  exit 127
fi

trash_root="${TRASH_ROOT:?TRASH_ROOT is not set; this shim only works inside the sandbox}"

keep() {
  [ -e "$1" ] || return 0
  rel="$(safe_rel "$1")"
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

/// Writes each shim atomically and only when its content or mode differs, so
/// concurrent sandboxes never see a half-written shim.
pub fn ensure_shims(shim_dir: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(shim_dir)?;
    let write_shim = |name: &str, body: String| -> anyhow::Result<()> {
        let shim = shim_dir.join(name);
        let content = format!("#!/bin/sh\n{SHIM_PATH_RESET}{SHIM_HELPERS}{body}");
        let up_to_date = fs::read(&shim).is_ok_and(|cur| cur == content.as_bytes())
            && fs::metadata(&shim).is_ok_and(|m| m.permissions().mode() & 0o777 == 0o755);
        if !up_to_date {
            crate::paths::write_atomic(&shim, content.as_bytes(), Some(0o755))?;
        }
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

// ---------------------------------------------------------------------------
// Trash
// ---------------------------------------------------------------------------

/// `cfg.sandbox_trash_dir` empty means the built-in default. Resolved per
/// call so a config change applies to the very next command.
pub fn resolve_trash_dir(cfg: &AppConfig) -> PathBuf {
    if cfg.sandbox_trash_dir.trim().is_empty() {
        crate::paths::app_config_dir().join("trash")
    } else {
        PathBuf::from(&cfg.sandbox_trash_dir)
    }
}

/// `<date>-<hex>`: one per command run / per agent task. Day-granularity date
/// plus a random suffix; `RandomState` is OS-seeded, which is enough here.
pub(crate) fn generate_trash_run_id() -> String {
    let date = chrono::Local::now().format("%Y%m%d");
    let random = RandomState::new().build_hasher().finish();
    format!("{date}-{random:016x}")
}

/// Drops the per-run trash dir when the run trashed nothing. Only the top
/// dir: an empty directory inside it is a legitimately trashed item.
fn prune_empty_run_dir(dir: &Path) {
    let _ = fs::remove_dir(dir);
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct RunOutcome {
    pub stdout: String,
    pub stderr: String,
    /// -1 when killed (timeout/cancel) or terminated by a signal.
    pub exit_code: i32,
    pub timed_out: bool,
    pub cancelled: bool,
}

/// Cancel signal for `run_sandboxed_async` (and the agent loop). A `watch`
/// channel rather than `Notify` + flag: `wait_for` checks the current value
/// first, so a cancel that lands before anyone waits is never missed.
#[derive(Clone)]
pub struct CancelToken(Arc<tokio::sync::watch::Sender<bool>>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(tokio::sync::watch::channel(false).0))
    }

    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once cancelled (immediately if already).
    pub async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

struct Prepared {
    cmd: Command,
    run_trash: PathBuf,
}

fn push_grant(c: &mut Command, g: &GrantedPath) {
    let flag = if g.read_write { "--bind" } else { "--ro-bind" };
    let path = Path::new(&g.path);
    if g.recursive {
        c.arg(flag).arg(path).arg(path);
    } else if let Ok(entries) = fs::read_dir(path) {
        // Binds are recursive, so "just this directory" means binding each
        // top-level file individually.
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_file() {
                c.arg(flag).arg(&entry_path).arg(&entry_path);
            }
        }
    }
}

/// Mount order matters (a later mount wins):
/// system dirs, grants, root, then everything that must stay read-only or
/// hidden -- shims, read-only grants under root, the app dir if a grant
/// covers it -- then scratch and trash.
fn build_bwrap(
    root: &Path,
    shim_dir: &Path,
    granted: &[GrantedPath],
    scratch: Option<&Path>,
    trash_base: &Path,
    cmd: &str,
    config_dir: &Path,
) -> anyhow::Result<Prepared> {
    let mut c = Command::new("bwrap");
    // env_clear() keeps our environment out of bwrap; --clearenv keeps
    // anything out of the shell. Only PATH and TRASH_ROOT get through.
    c.env_clear();
    c.arg("--die-with-parent")
        .arg("--new-session")
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

    // A stale grant shouldn't break every command.
    let live: Vec<&GrantedPath> = granted
        .iter()
        .filter(|g| Path::new(&g.path).exists())
        .collect();
    for g in &live {
        push_grant(&mut c, g);
    }

    // After the grants, so a grant that is an ancestor of root doesn't make
    // root read-only.
    c.arg("--bind").arg(root).arg(root);

    let root_c = canon_or_self(root);
    for g in live.iter().filter(|g| !g.read_write) {
        if canon_or_self(Path::new(&g.path)).starts_with(&root_c) {
            push_grant(&mut c, g);
        }
    }

    c.arg("--ro-bind").arg(shim_dir).arg(SANDBOX_SHIM_DIR);
    if canon_or_self(shim_dir).starts_with(&root_c) {
        c.arg("--ro-bind").arg(shim_dir).arg(shim_dir);
    }

    // A grant must not make the app's memory/config writable.
    let cfg_c = canon_or_self(config_dir);
    if live
        .iter()
        .any(|g| cfg_c.starts_with(canon_or_self(Path::new(&g.path))))
    {
        c.arg("--tmpfs").arg(config_dir);
    }

    // Scratch only. The rest of session memory stays unbound: `progress.md`
    // is worth trusting only if nothing but this app can write it.
    if let Some(scratch) = scratch.filter(|p| p.is_dir()) {
        c.arg("--bind").arg(scratch).arg(scratch);
    }

    // Only this run's own subdir is bound, at a path nothing else would look.
    let run_trash = trash_base.join(generate_trash_run_id());
    fs::create_dir_all(&run_trash)?;
    c.arg("--bind").arg(&run_trash).arg(SANDBOX_TRASH_DIR);

    c.arg("--chdir")
        .arg(root)
        .arg("--setenv")
        .arg("PATH")
        .arg(format!("{SANDBOX_SHIM_DIR}:/usr/bin:/bin"))
        .arg("--setenv")
        .arg("TRASH_ROOT")
        .arg(SANDBOX_TRASH_DIR)
        .arg("sh")
        .arg("-c")
        .arg(cmd);
    Ok(Prepared { cmd: c, run_trash })
}

/// Head + tail of a byte stream, constant memory regardless of volume.
#[derive(Default)]
struct Capped {
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
}

impl Capped {
    fn push(&mut self, mut chunk: &[u8]) {
        self.total += chunk.len();
        if self.head.len() < OUTPUT_KEEP {
            let n = (OUTPUT_KEEP - self.head.len()).min(chunk.len());
            self.head.extend_from_slice(&chunk[..n]);
            chunk = &chunk[n..];
        }
        if !chunk.is_empty() {
            self.tail.extend_from_slice(chunk);
            if self.tail.len() > 2 * OUTPUT_KEEP {
                let cut = self.tail.len() - OUTPUT_KEEP;
                self.tail.drain(..cut);
            }
        }
    }

    fn render(&self) -> String {
        let tail = &self.tail[self.tail.len().saturating_sub(OUTPUT_KEEP)..];
        let omitted = self.total - self.head.len() - tail.len();
        let mut s = String::from_utf8_lossy(&self.head).into_owned();
        if omitted > 0 {
            s.push_str(&format!(
                "\n[... {omitted} bytes omitted — read the file with read_file/offset or narrow the command ...]\n"
            ));
        }
        s.push_str(&String::from_utf8_lossy(tail));
        s
    }
}

fn spawn_reader<R>(mut r: R, sink: Arc<Mutex<Capped>>) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => sink.lock().unwrap().push(&buf[..n]),
            }
        }
    })
}

/// `scratch`, when given, is bound read-write -- the model's only other
/// writable path outside the working folder. Passed in rather than read from
/// `memory::`, so this module doesn't depend on that feature.
///
/// Output is capped per stream. On timeout or
/// cancel the child is killed (`--die-with-parent` takes the sandbox down
/// with bwrap) and the outcome carries the matching flag with `exit_code: -1`;
/// output captured so far is still returned.
#[allow(clippy::too_many_arguments)]
pub async fn run_sandboxed_async(
    root: &Path,
    shim_dir: &Path,
    granted: &[GrantedPath],
    scratch: Option<&Path>,
    trash_base: &Path,
    cmd: &str,
    timeout: Duration,
    cancel: &CancelToken,
) -> anyhow::Result<RunOutcome> {
    let p = build_bwrap(
        root,
        shim_dir,
        granted,
        scratch,
        trash_base,
        cmd,
        &crate::paths::app_config_dir(),
    )?;
    let run_trash = p.run_trash;
    let result = run_child(p.cmd, timeout, cancel).await;
    prune_empty_run_dir(&run_trash);
    result
}

async fn run_child(
    cmd: Command,
    timeout: Duration,
    cancel: &CancelToken,
) -> anyhow::Result<RunOutcome> {
    let mut c = tokio::process::Command::from(cmd);
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = c.spawn()?;

    let out = Arc::new(Mutex::new(Capped::default()));
    let err = Arc::new(Mutex::new(Capped::default()));
    let mut readers = vec![
        spawn_reader(child.stdout.take().expect("piped"), out.clone()),
        spawn_reader(child.stderr.take().expect("piped"), err.clone()),
    ];

    let mut outcome = RunOutcome::default();
    tokio::select! {
        status = child.wait() => {
            outcome.exit_code = status?.code().unwrap_or(-1);
        }
        _ = tokio::time::sleep(timeout) => {
            outcome.timed_out = true;
            outcome.exit_code = -1;
        }
        _ = cancel.cancelled() => {
            outcome.cancelled = true;
            outcome.exit_code = -1;
        }
    }
    if outcome.timed_out || outcome.cancelled {
        let _ = child.kill().await;
    }
    // A stray grandchild holding the pipes open must not hang us.
    for r in &mut readers {
        if tokio::time::timeout(READER_GRACE, &mut *r).await.is_err() {
            r.abort();
        }
    }
    outcome.stdout = out.lock().unwrap().render();
    outcome.stderr = err.lock().unwrap().render();
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

    /// Fresh folder + shims + an external trash base, named per test so
    /// parallel runs don't collide. The trash base is a sibling of `root`,
    /// the same outside-the-working-folder shape the real one has. The shims
    /// live inside root on purpose: that is the hard case for read-only.
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

    fn cleanup(root: &Path, trash_base: &Path) {
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(trash_base);
    }

    /// Blocking wrapper over `run_sandboxed_async` for the tests below.
    fn run_sandboxed(
        root: &Path,
        shims: &Path,
        granted: &[GrantedPath],
        scratch: Option<&Path>,
        trash: &Path,
        cmd: &str,
    ) -> anyhow::Result<RunOutcome> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_sandboxed_async(
                root,
                shims,
                granted,
                scratch,
                trash,
                cmd,
                Duration::from_secs(60),
                &CancelToken::new(),
            ))
    }

    fn run(root: &Path, shims: &Path, trash: &Path, cmd: &str) -> RunOutcome {
        run_sandboxed(root, shims, &[], None, trash, cmd).unwrap()
    }

    // ---- shims / trash ----------------------------------------------------

    // Deleting the same path twice, in two separate commands, must land in
    // two distinct trash batches, not have `mv -f` overwrite the first.
    #[test]
    fn repeated_rm_of_same_path_does_not_clobber_earlier_trash() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("repeated-rm");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/note.txt"), "version1").unwrap();

        let outcome = run(&root, &shim_dir, &trash_base, "rm sub/note.txt");
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/note.txt"), "version2").unwrap();
        let outcome = run(&root, &shim_dir, &trash_base, "rm sub/note.txt");
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        let mut run_dirs: Vec<PathBuf> = fs::read_dir(&trash_base)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        run_dirs.sort();
        assert_eq!(run_dirs.len(), 2, "expected two batches, got {run_dirs:?}");

        let contents: Vec<String> = run_dirs
            .iter()
            .map(|d| fs::read_to_string(d.join("sub/note.txt")).unwrap())
            .collect();
        assert!(contents.contains(&"version1".to_string()), "{contents:?}");
        assert!(contents.contains(&"version2".to_string()), "{contents:?}");
        cleanup(&root, &trash_base);
    }

    // Same relative path trashed twice in ONE run: the shim's own `.2`
    // suffix must keep both copies.
    #[test]
    fn same_path_trashed_twice_in_one_command_keeps_both_copies() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("same-path-one-run");
        fs::write(root.join("note.txt"), "version1").unwrap();

        let outcome = run(
            &root,
            &shim_dir,
            &trash_base,
            "rm note.txt && echo version2 > note.txt && rm note.txt",
        );
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        let trashed = trashed_files(&trash_base);
        assert_eq!(trashed.len(), 2, "{trashed:?}");
        let contents: Vec<&str> = trashed.iter().map(|(_, t)| t.as_str()).collect();
        assert!(contents.contains(&"version1"), "{trashed:?}");
        assert!(contents.contains(&"version2\n"), "{trashed:?}");
        cleanup(&root, &trash_base);
    }

    // `mv` over an existing target destroys it as permanently as `rm`.
    #[test]
    fn mv_over_an_existing_file_keeps_the_overwritten_copy() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("mv-overwrite");
        fs::write(root.join("new.txt"), "incoming").unwrap();
        fs::write(root.join("old.txt"), "about to be destroyed").unwrap();

        let outcome = run(&root, &shim_dir, &trash_base, "mv new.txt old.txt");
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(
            fs::read_to_string(root.join("old.txt")).unwrap(),
            "incoming"
        );

        let trashed = trashed_files(&trash_base);
        assert!(
            trashed.iter().any(|(_, t)| t == "about to be destroyed"),
            "{trashed:?}"
        );
        cleanup(&root, &trash_base);
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

        let outcome = run(&root, &shim_dir, &trash_base, "mv note.txt dest");
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(
            fs::read_to_string(root.join("dest/note.txt")).unwrap(),
            "new version"
        );
        let trashed = trashed_files(&trash_base);
        assert!(
            trashed.iter().any(|(_, t)| t == "old version"),
            "{trashed:?}"
        );
        cleanup(&root, &trash_base);
    }

    #[test]
    fn cp_over_an_existing_file_keeps_the_overwritten_copy() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("cp-overwrite");
        fs::write(root.join("src.txt"), "incoming").unwrap();
        fs::write(root.join("dst.txt"), "about to be destroyed").unwrap();

        let outcome = run(&root, &shim_dir, &trash_base, "cp src.txt dst.txt");
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(
            fs::read_to_string(root.join("dst.txt")).unwrap(),
            "incoming"
        );
        let trashed = trashed_files(&trash_base);
        assert!(
            trashed.iter().any(|(_, t)| t == "about to be destroyed"),
            "{trashed:?}"
        );
        cleanup(&root, &trash_base);
    }

    #[test]
    fn truncate_keeps_the_contents_it_discards() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("truncate");
        fs::write(root.join("log.txt"), "important history").unwrap();

        let outcome = run(&root, &shim_dir, &trash_base, "truncate -s 0 log.txt");
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(fs::read_to_string(root.join("log.txt")).unwrap(), "");
        let trashed = trashed_files(&trash_base);
        assert!(
            trashed.iter().any(|(_, t)| t == "important history"),
            "{trashed:?}"
        );
        cleanup(&root, &trash_base);
    }

    // Shims call mv/cp/mkdir internally. Fails by hanging, not by a wrong
    // answer.
    #[test]
    fn shims_do_not_recurse_into_each_other() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("shim-recursion");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/a.txt"), "content").unwrap();
        let outcome = run(&root, &shim_dir, &trash_base, "rm sub/a.txt");
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);

        fs::write(root.join("x.txt"), "x").unwrap();
        fs::write(root.join("y.txt"), "y").unwrap();
        let outcome = run(&root, &shim_dir, &trash_base, "mv x.txt y.txt");
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(fs::read_to_string(root.join("y.txt")).unwrap(), "x");
        cleanup(&root, &trash_base);
    }

    // Real rmdir refuses a non-empty directory; a shim that trashes it turns
    // a safe failure into a reported success.
    #[test]
    fn rmdir_refuses_a_non_empty_directory() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("rmdir-non-empty");
        fs::create_dir_all(root.join("empty_one")).unwrap();
        fs::create_dir_all(root.join("archive")).unwrap();
        fs::write(root.join("archive/keep.txt"), "the user's file").unwrap();

        let outcome = run(&root, &shim_dir, &trash_base, "rmdir empty_one archive");
        assert_ne!(outcome.exit_code, 0, "must fail like the real rmdir");
        assert!(
            outcome.stderr.contains("Directory not empty"),
            "{}",
            outcome.stderr
        );
        assert!(root.join("archive/keep.txt").exists());
        assert!(!root.join("empty_one").exists());
        cleanup(&root, &trash_base);
    }

    #[test]
    fn rmdir_moves_target_to_trash_instead_of_removing_it() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("rmdir-moves");
        fs::create_dir_all(root.join("empty_folder")).unwrap();

        let outcome = run(&root, &shim_dir, &trash_base, "rmdir empty_folder");
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert!(!root.join("empty_folder").exists());
        let moved: Vec<PathBuf> = fs::read_dir(&trash_base)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().join("empty_folder"))
            .filter(|p| p.is_dir())
            .collect();
        assert_eq!(moved.len(), 1, "{moved:?}");
        cleanup(&root, &trash_base);
    }

    #[test]
    fn rm_f_on_a_missing_file_succeeds_silently_but_plain_rm_fails() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("rm-f-missing");
        let outcome = run(
            &root,
            &shim_dir,
            &trash_base,
            "rm -f nothing.txt && echo next",
        );
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert_eq!(outcome.stdout.trim(), "next");
        assert!(outcome.stderr.is_empty(), "{}", outcome.stderr);

        let outcome = run(&root, &shim_dir, &trash_base, "rm nothing.txt");
        assert_ne!(outcome.exit_code, 0);
        assert!(
            outcome.stderr.contains("No such file"),
            "{}",
            outcome.stderr
        );
        cleanup(&root, &trash_base);
    }

    #[test]
    fn rm_after_double_dash_treats_dash_names_as_operands() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("rm-dashdash");
        fs::write(root.join("-weird"), "odd name").unwrap();
        fs::write(root.join("-rf"), "worse name").unwrap();

        let outcome = run(&root, &shim_dir, &trash_base, "rm -- -weird -rf");
        assert_eq!(outcome.exit_code, 0, "stderr: {}", outcome.stderr);
        assert!(!root.join("-weird").exists());
        assert!(!root.join("-rf").exists());
        let trashed = trashed_files(&trash_base);
        assert!(trashed.iter().any(|(_, t)| t == "odd name"), "{trashed:?}");
        assert!(
            trashed.iter().any(|(_, t)| t == "worse name"),
            "{trashed:?}"
        );
        cleanup(&root, &trash_base);
    }

    #[test]
    fn shim_fails_loudly_when_trash_root_is_unset() {
        let (root, shim_dir, trash_base) = scratch("no-trash-root");
        for tool in ["rm", "mv"] {
            let out = Command::new("sh")
                .arg(shim_dir.join(tool))
                .arg("a")
                .arg("b")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                !out.status.success(),
                "{tool} must not run without TRASH_ROOT"
            );
            assert!(
                String::from_utf8_lossy(&out.stderr).contains("TRASH_ROOT"),
                "{tool}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        assert!(!root.join(".temp-trash").exists());
        cleanup(&root, &trash_base);
    }

    #[test]
    fn ensure_shims_is_idempotent_and_executable() {
        let (root, shim_dir, trash_base) = scratch("shims-idem");
        let rm = shim_dir.join("rm");
        let before = fs::metadata(&rm).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        ensure_shims(&shim_dir).unwrap();
        assert_eq!(fs::metadata(&rm).unwrap().modified().unwrap(), before);
        assert_eq!(
            fs::metadata(&rm).unwrap().permissions().mode() & 0o777,
            0o755
        );

        fs::write(&rm, "tampered").unwrap();
        ensure_shims(&shim_dir).unwrap();
        assert!(fs::read_to_string(&rm).unwrap().starts_with("#!/bin/sh"));
        cleanup(&root, &trash_base);
    }

    #[test]
    fn empty_trash_run_dir_is_removed_after_the_run() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("trash-prune");
        fs::write(root.join("a.txt"), "a").unwrap();
        let outcome = run(&root, &shim_dir, &trash_base, "ls");
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(fs::read_dir(&trash_base).unwrap().count(), 0);

        // A failed rm trashes nothing: still no leftover dir.
        let outcome = run(&root, &shim_dir, &trash_base, "rm nothing/a.txt");
        assert_ne!(outcome.exit_code, 0);
        assert_eq!(fs::read_dir(&trash_base).unwrap().count(), 0);

        // A real trash entry keeps its run dir.
        run(&root, &shim_dir, &trash_base, "rm a.txt");
        assert_eq!(fs::read_dir(&trash_base).unwrap().count(), 1);
        cleanup(&root, &trash_base);
    }

    // ---- mounts -----------------------------------------------------------

    #[test]
    fn shims_stay_read_only_even_when_under_the_root() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("shim-ro");
        let before = fs::read(shim_dir.join("rm")).unwrap();

        let outcome = run(
            &root,
            &shim_dir,
            &trash_base,
            "echo pwned > /run/llm-shims/rm",
        );
        assert_ne!(outcome.exit_code, 0);
        let outcome = run(&root, &shim_dir, &trash_base, "echo pwned > shims/rm");
        assert_ne!(
            outcome.exit_code, 0,
            "root bind must not re-open the shim dir"
        );
        assert_eq!(fs::read(shim_dir.join("rm")).unwrap(), before);

        // And they are what `rm` resolves to.
        let outcome = run(&root, &shim_dir, &trash_base, "command -v rm");
        assert_eq!(outcome.stdout.trim(), "/run/llm-shims/rm");
        cleanup(&root, &trash_base);
    }

    // `rm ../x` used to build `/trash/../x` -- the sandbox's ephemeral root --
    // so a granted file was deleted and its "backup" vanished with the run.
    #[test]
    fn dotdot_operands_still_land_in_the_trash() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("dotdot");
        let outer = root.join("outer");
        fs::create_dir_all(root.join("work")).unwrap();
        fs::create_dir_all(&outer).unwrap();
        fs::write(outer.join("x.txt"), "keep me").unwrap();
        fs::write(outer.join("y.txt"), "old").unwrap();
        fs::write(root.join("work/new.txt"), "new").unwrap();
        // The grant is outside the work dir, reached with `..`.
        let work = root.join("work");
        let grants = [GrantedPath {
            path: outer.to_string_lossy().into_owned(),
            note: String::new(),
            read_write: true,
            recursive: true,
        }];

        let out = run_sandboxed(
            &work,
            &shim_dir,
            &grants,
            None,
            &trash_base,
            "rm ../outer/x.txt && cp new.txt ../outer/y.txt",
        )
        .unwrap();
        assert_eq!(out.exit_code, 0, "{}", out.stderr);
        assert!(!outer.join("x.txt").exists());
        let trashed = trashed_files(&trash_base);
        assert!(
            trashed
                .iter()
                .any(|(p, t)| p.ends_with("outer/x.txt") && t == "keep me"),
            "removed file must be kept in the trash, not under `..`: {trashed:?}"
        );
        assert!(
            trashed
                .iter()
                .any(|(p, t)| p.ends_with("outer/y.txt") && t == "old"),
            "the overwritten file must be kept too: {trashed:?}"
        );
        assert!(
            trashed
                .iter()
                .all(|(p, _)| !p.to_string_lossy().contains("..")),
            "{trashed:?}"
        );
        cleanup(&root, &trash_base);
    }

    #[test]
    fn read_only_grant_under_root_stays_read_only() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("ro-grant");
        fs::create_dir_all(root.join("vendor")).unwrap();
        fs::write(root.join("vendor/lib.txt"), "orig").unwrap();
        let grants = [GrantedPath {
            path: root.join("vendor").to_string_lossy().into_owned(),
            note: String::new(),
            read_write: false,
            recursive: true,
        }];

        let out = run_sandboxed(
            &root,
            &shim_dir,
            &grants,
            None,
            &trash_base,
            "echo x > vendor/lib.txt; echo y > vendor/new.txt; echo z > ok.txt",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("vendor/lib.txt")).unwrap(),
            "orig"
        );
        assert!(!root.join("vendor/new.txt").exists(), "{}", out.stderr);
        assert!(root.join("ok.txt").exists());
        cleanup(&root, &trash_base);
    }

    #[test]
    fn grant_covering_the_app_dir_does_not_expose_it() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("app-dir-grant");
        let outer =
            std::env::temp_dir().join(format!("llm-assistant-grant-outer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&outer);
        let cfg = outer.join("cfg");
        fs::create_dir_all(&cfg).unwrap();
        fs::write(cfg.join("secret"), "s3cret").unwrap();
        fs::write(outer.join("shared.txt"), "fine").unwrap();
        let grants = [GrantedPath {
            path: outer.to_string_lossy().into_owned(),
            note: String::new(),
            read_write: true,
            recursive: true,
        }];

        let mut p = build_bwrap(
            &root,
            &shim_dir,
            &grants,
            None,
            &trash_base,
            &format!(
                "cat {}/shared.txt; cat {}/secret; echo x > {}/secret",
                outer.display(),
                cfg.display(),
                cfg.display()
            ),
            &cfg,
        )
        .unwrap();
        let out = p.cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("fine"), "{stdout}");
        assert!(!stdout.contains("s3cret"), "{stdout}");
        assert_eq!(fs::read_to_string(cfg.join("secret")).unwrap(), "s3cret");
        let _ = fs::remove_dir_all(&outer);
        cleanup(&root, &trash_base);
    }

    #[test]
    fn bwrap_gets_session_and_parent_death_flags() {
        let (root, shim_dir, trash_base) = scratch("flags");
        let p = build_bwrap(
            &root,
            &shim_dir,
            &[],
            None,
            &trash_base,
            "true",
            &root.join("cfg"),
        )
        .unwrap();
        let args: Vec<String> = p
            .cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--new-session".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
        // Read-only re-binds come after the root bind.
        let root_bind = args
            .windows(3)
            .position(|w| w[0] == "--bind" && w[1] == root.to_string_lossy())
            .unwrap();
        let shim_bind = args
            .windows(3)
            .position(|w| w[0] == "--ro-bind" && w[2] == SANDBOX_SHIM_DIR)
            .unwrap();
        assert!(shim_bind > root_bind);
        cleanup(&root, &trash_base);
    }

    // ---- async runner -----------------------------------------------------

    #[tokio::test]
    async fn async_runner_captures_output_and_exit_code() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("async-basic");
        let cancel = CancelToken::new();
        let out = run_sandboxed_async(
            &root,
            &shim_dir,
            &[],
            None,
            &trash_base,
            "echo hi; echo oops >&2; exit 3",
            Duration::from_secs(20),
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(out.stdout, "hi\n");
        assert_eq!(out.stderr, "oops\n");
        assert_eq!(out.exit_code, 3);
        assert!(!out.timed_out && !out.cancelled);
        cleanup(&root, &trash_base);
    }

    #[tokio::test]
    async fn async_runner_times_out_and_kills_the_command() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("async-timeout");
        let started = std::time::Instant::now();
        let out = run_sandboxed_async(
            &root,
            &shim_dir,
            &[],
            None,
            &trash_base,
            "echo started; sleep 30",
            Duration::from_millis(700),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert!(out.timed_out && !out.cancelled);
        assert_eq!(out.exit_code, -1);
        assert!(
            out.stdout.contains("started"),
            "partial output kept: {:?}",
            out.stdout
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        cleanup(&root, &trash_base);
    }

    #[tokio::test]
    async fn async_runner_stops_on_cancel() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("async-cancel");
        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            c2.cancel();
        });
        let started = std::time::Instant::now();
        let out = run_sandboxed_async(
            &root,
            &shim_dir,
            &[],
            None,
            &trash_base,
            "sleep 30",
            Duration::from_secs(60),
            &cancel,
        )
        .await
        .unwrap();
        assert!(out.cancelled && !out.timed_out);
        assert_eq!(out.exit_code, -1);
        assert!(started.elapsed() < Duration::from_secs(10));

        // Already-cancelled token: returns promptly too.
        let out = run_sandboxed_async(
            &root,
            &shim_dir,
            &[],
            None,
            &trash_base,
            "sleep 30",
            Duration::from_secs(60),
            &cancel,
        )
        .await
        .unwrap();
        assert!(out.cancelled);
        cleanup(&root, &trash_base);
    }

    #[tokio::test]
    async fn async_runner_caps_huge_output_keeping_head_and_tail() {
        if !sandbox_available() {
            return;
        }
        let (root, shim_dir, trash_base) = scratch("async-cap");
        let out = run_sandboxed_async(
            &root,
            &shim_dir,
            &[],
            None,
            &trash_base,
            "echo HEAD; head -c 300000 /dev/zero | tr '\\0' a; echo; echo TAIL",
            Duration::from_secs(30),
            &CancelToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.exit_code, 0, "{}", out.stderr);
        assert!(out.stdout.len() < 70_000, "{}", out.stdout.len());
        assert!(out.stdout.starts_with("HEAD"));
        assert!(out.stdout.trim_end().ends_with("TAIL"));
        assert!(out
            .stdout
            .contains("bytes omitted — read the file with read_file/offset"));
        cleanup(&root, &trash_base);
    }

    #[test]
    fn capped_buffer_keeps_everything_up_to_the_limit() {
        let mut c = Capped::default();
        let data: Vec<u8> = (0..2 * OUTPUT_KEEP)
            .map(|i| b'a' + (i % 26) as u8)
            .collect();
        for chunk in data.chunks(1000) {
            c.push(chunk);
        }
        assert_eq!(c.render().as_bytes(), &data[..]);

        c.push(b"MORE");
        let r = c.render();
        assert!(
            r.contains("4 bytes omitted") || r.contains("bytes omitted"),
            "{}",
            &r[OUTPUT_KEEP - 5..OUTPUT_KEEP + 120]
        );
        assert!(r.ends_with("MORE"));
        assert!(r.starts_with("abcdef"));
    }

    // ---- classification ---------------------------------------------------

    fn auto(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classify_plain_read_only_commands() {
        for c in [
            "ls -la",
            "cat notes.txt",
            "grep -rn foo .",
            "find . -name '*.rs'",
            "tail -n 5 log",
            "tree -L 2",
            "file a.bin",
        ] {
            assert_eq!(classify_command(c), Classification::ReadOnly, "{c}");
        }
        for c in [
            "",
            "rm a",
            "mv a b",
            "ls > out",
            "cat a | head",
            "ls; rm a",
            "echo $(whoami)",
            "ls &",
        ] {
            assert_eq!(
                classify_command(c),
                Classification::NeedsConfirmation,
                "{c}"
            );
        }
    }

    #[test]
    fn classify_requires_a_bare_program_name() {
        for c in ["./ls", "/bin/ls", "../cat x", "sub/grep foo"] {
            assert_eq!(
                classify_command(c),
                Classification::NeedsConfirmation,
                "{c}"
            );
        }
        assert!(!is_auto_approved("./cat x", &auto(&["cat", "./cat"])));
    }

    #[test]
    fn classify_find_with_write_or_exec_actions() {
        for c in [
            "find . -delete",
            "find . -exec rm {} +",
            "find . -execdir ls {} +",
            "find . -ok rm {} ;",
            "find . -fprint out.txt",
            "find . -fprintf out.txt %p",
            "find . -fls out.txt",
        ] {
            assert_eq!(
                classify_command(c),
                Classification::NeedsConfirmation,
                "{c}"
            );
        }
        // Even if someone put find on the allow list.
        assert!(!is_auto_approved("find . -delete", &auto(&["find"])));
        assert!(!is_auto_approved("find . -name x", &auto(&["find"])));
    }

    #[test]
    fn classify_tree_o_and_tail_follow() {
        for c in [
            "tree -o out.txt",
            "tree -ao out.txt",
            "tree --output=x",
            "tail -f log",
            "tail -F log",
            "tail -fn5 log",
            "tail --follow log",
            "tail --follow=name log",
            "file -C",
        ] {
            assert_eq!(
                classify_command(c),
                Classification::NeedsConfirmation,
                "{c}"
            );
        }
        for c in ["tail -n 20 log", "tail -c 100 log", "tree -a -L 2"] {
            assert_eq!(classify_command(c), Classification::ReadOnly, "{c}");
        }
    }

    #[test]
    fn auto_approve_follows_the_list_but_not_for_wrappers() {
        let list = auto(&["cat", "sh", "python3", "env", "xargs"]);
        assert!(is_auto_approved("cat a.txt", &list));
        assert!(!is_auto_approved("cat a.txt > b", &list));
        assert!(!is_auto_approved("ls", &list));
        for c in ["sh -c 'rm -rf x'", "python3 x.py", "env rm x", "xargs rm"] {
            assert!(!is_auto_approved(c, &list), "{c}");
        }
    }

    #[test]
    fn can_auto_approve_program_blocks_wrappers_and_interpreters() {
        for p in ["cat", "ls", "mkdir", "git", "rsync"] {
            assert!(can_auto_approve_program(p), "{p}");
        }
        for p in [
            "sh",
            "bash",
            "zsh",
            "dash",
            "env",
            "xargs",
            "timeout",
            "nice",
            "nohup",
            "python",
            "python3",
            "python3.12",
            "perl",
            "ruby",
            "node",
            "awk",
            "sed",
            "find",
            "busybox",
            "sudo",
            "",
            "./cat",
            "/bin/cat",
        ] {
            assert!(!can_auto_approve_program(p), "{p:?}");
        }
    }

    // ---- privilege --------------------------------------------------------

    #[test]
    fn privilege_only_in_command_position() {
        assert_eq!(needs_privilege("sudo rm x").as_deref(), Some("sudo"));
        assert_eq!(needs_privilege("ls && sudo rm x").as_deref(), Some("sudo"));
        assert_eq!(needs_privilege("ls || su -c id").as_deref(), Some("su"));
        assert_eq!(needs_privilege("ls; doas reboot").as_deref(), Some("doas"));
        assert_eq!(
            needs_privilege("cat a | pkexec tee b").as_deref(),
            Some("pkexec")
        );
        assert_eq!(needs_privilege("echo hi\nsudo ls").as_deref(), Some("sudo"));
        assert_eq!(needs_privilege("/usr/bin/sudo ls").as_deref(), Some("sudo"));
        assert_eq!(needs_privilege("FOO=1 sudo ls").as_deref(), Some("sudo"));
        assert_eq!(needs_privilege("env -i sudo ls").as_deref(), Some("sudo"));
        assert_eq!(needs_privilege("command sudo ls").as_deref(), Some("sudo"));
        assert_eq!(needs_privilege("ls;sudo ls").as_deref(), Some("sudo"));
        assert_eq!(needs_privilege("echo $(sudo id)").as_deref(), Some("sudo"));
    }

    #[test]
    fn privilege_words_in_arguments_are_fine() {
        for c in [
            "grep sudo notes.txt",
            "echo su",
            "cat sudoers.txt",
            "echo \"a; sudo b\"",
            "echo 'x && sudo y'",
            "ls -la",
            "",
            "doas_notes.sh",
        ] {
            assert_eq!(needs_privilege(c), None, "{c}");
        }
    }

    // ---- root safety ------------------------------------------------------

    #[test]
    fn root_safety_refuses_dangerous_roots() {
        let base =
            std::env::temp_dir().join(format!("llm-assistant-rootcheck-{}", std::process::id()));
        let home = base.join("home");
        let cfg = home.join(".config/llm-assistant");
        fs::create_dir_all(&cfg).unwrap();
        fs::create_dir_all(home.join("project")).unwrap();
        fs::create_dir_all(cfg.join("memory")).unwrap();

        let check = |p: &Path| check_root_allowed_with(p, Some(&home), &cfg);
        assert!(check(Path::new("/")).is_err());
        assert!(check(&home).is_err(), "home itself");
        assert!(
            check(&home.join(".config")).is_err(),
            "ancestor of config dir"
        );
        assert!(check(&base).is_err(), "ancestor of config dir");
        assert!(check(&cfg).is_err(), "config dir itself");
        assert!(check(&cfg.join("memory")).is_err(), "inside config dir");
        assert!(check(&home.join("project")).is_ok());
        assert!(check(&home.join("project/../project")).is_ok());
        assert!(
            check(&home.join("project/../.config")).is_err(),
            "lexical tricks"
        );

        // A symlink into the config dir is the same place.
        let link = base.join("link");
        std::os::unix::fs::symlink(&cfg, &link).unwrap();
        assert!(check(&link).is_err());
        let _ = fs::remove_dir_all(&base);
    }
}
