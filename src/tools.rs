//! Path-confined file tools executed in pure Rust (no shell). Everything the
//! model passes in is untrusted: paths are resolved against the folder (and
//! granted paths) with symlinks followed, and mutations back up first.
use crate::config::AppConfig;
use crate::sandbox;
use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

fn dot() -> String {
    ".".to_string()
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum Tool {
    ListDir {
        #[serde(default = "dot")]
        path: String,
        #[serde(default)]
        depth: Option<u32>,
    },
    ReadFile {
        path: String,
        #[serde(default)]
        offset: Option<u32>,
        #[serde(default)]
        limit: Option<u32>,
    },
    Search {
        pattern: String,
        #[serde(default = "dot")]
        path: String,
        #[serde(default)]
        glob: Option<String>,
        #[serde(default)]
        regex: bool,
        #[serde(default)]
        case_sensitive: bool,
    },
    FindFiles {
        glob: String,
        #[serde(default = "dot")]
        path: String,
    },
    WriteFile {
        path: String,
        content: String,
        /// Add `content` to the end instead of replacing the file.
        #[serde(default)]
        append: bool,
    },
    EditFile {
        path: String,
        edits: Vec<Edit>,
    },
    Move {
        from: String,
        to: String,
    },
    Delete {
        path: String,
    },
    MakeDir {
        path: String,
    },
    RunCommand {
        command: String,
    },
    Finish {
        summary: String,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: bool,
}

pub const DEFAULT_READ_LIMIT: u32 = 200;
pub const MAX_READ_LIMIT: u32 = 1000;
pub const MAX_LIST_DEPTH: u32 = 3;

impl Tool {
    pub fn name(&self) -> &'static str {
        match self {
            Tool::ListDir { .. } => "list_dir",
            Tool::ReadFile { .. } => "read_file",
            Tool::Search { .. } => "search",
            Tool::FindFiles { .. } => "find_files",
            Tool::WriteFile { .. } => "write_file",
            Tool::EditFile { .. } => "edit_file",
            Tool::Move { .. } => "move",
            Tool::Delete { .. } => "delete",
            Tool::MakeDir { .. } => "make_dir",
            Tool::RunCommand { .. } => "run_command",
            Tool::Finish { .. } => "finish",
        }
    }

    /// Short human line for the step list and the confirm dialog.
    pub fn title(&self) -> String {
        match self {
            Tool::ListDir { path, depth } => match depth {
                Some(d) if *d > 1 => format!("List {path} (depth {})", (*d).min(MAX_LIST_DEPTH)),
                _ => format!("List {path}"),
            },
            Tool::ReadFile {
                path,
                offset,
                limit,
            } => {
                let start = offset.unwrap_or(1).max(1);
                let n = limit.unwrap_or(DEFAULT_READ_LIMIT).clamp(1, MAX_READ_LIMIT);
                format!("Read {path} (lines {start}-{})", start + n - 1)
            }
            Tool::Search {
                pattern,
                path,
                glob,
                ..
            } => {
                let mut t = format!("Search \"{}\" in {path}", clip(pattern, 60));
                if let Some(g) = glob {
                    t.push_str(&format!(" ({g})"));
                }
                t
            }
            Tool::FindFiles { glob, path } => format!("Find files {glob} in {path}"),
            Tool::WriteFile {
                path,
                content,
                append,
            } => {
                let verb = if *append { "Append to" } else { "Write" };
                let n = content.lines().count();
                format!("{verb} {path} ({n} line{})", if n == 1 { "" } else { "s" })
            }
            Tool::EditFile { path, edits } => {
                format!(
                    "Edit {path} ({} edit{})",
                    edits.len(),
                    if edits.len() == 1 { "" } else { "s" }
                )
            }
            Tool::Move { from, to } => format!("Move {from} -> {to}"),
            Tool::Delete { path } => format!("Delete {path}"),
            Tool::MakeDir { path } => format!("Create folder {path}"),
            Tool::RunCommand { command } => {
                let first = command.lines().next().unwrap_or("");
                let more = if command.lines().nth(1).is_some() {
                    " ..."
                } else {
                    ""
                };
                format!("Run: {}{more}", clip(first, 100))
            }
            Tool::Finish { .. } => "Finish".to_string(),
        }
    }
}

/// Char-boundary-safe truncation with an ellipsis.
fn clip(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Listed, but never descended into (list_dir, search, find_files).
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", "__pycache__"];
const MAX_LIST_ENTRIES: usize = 300;
const MAX_FIND_RESULTS: usize = 300;
const MAX_SEARCH_MATCHES: usize = 100;
const MAX_SEARCH_COUNT: usize = 10_000;
const MAX_MATCH_CHARS: usize = 200;
const MAX_LINE_CHARS: usize = 500;
const BINARY_PROBE_BYTES: usize = 8 * 1024;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_DIFF_LINES: usize = 400;
const MAX_DIFF_FILE_LINES: usize = 5000;
const MAX_DP_CELLS: usize = 6_000_000;
const DIFF_CONTEXT: usize = 3;
const EXDEV: i32 = 18;

fn is_skipped_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name)
}

// ---------------------------------------------------------------------------
// Workspace and path resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Grant {
    /// Canonical.
    pub path: PathBuf,
    pub writable: bool,
    /// `false` exposes only the files directly inside (mirrors the sandbox).
    pub recursive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

pub struct Workspace {
    /// Canonical.
    root: PathBuf,
    grants: Vec<Grant>,
    trash_base: PathBuf,
    /// App-owned dirs (config, trash), always refused, even inside the root.
    protected: Vec<PathBuf>,
}

impl Workspace {
    /// Canonicalizes the root and the grants; grants that don't exist are
    /// skipped with a warning.
    pub fn new(root: &Path, cfg: &AppConfig) -> anyhow::Result<Self> {
        let mut grants = Vec::new();
        for g in &cfg.granted_paths {
            match fs::canonicalize(&g.path) {
                Ok(path) => grants.push(Grant {
                    path,
                    writable: g.read_write,
                    recursive: g.recursive,
                }),
                Err(e) => log::warn!("skipping granted path {}: {e}", g.path),
            }
        }
        Self::from_parts(root, grants, sandbox::resolve_trash_dir(cfg))
    }

    pub(crate) fn from_parts(
        root: &Path,
        grants: Vec<Grant>,
        trash_base: PathBuf,
    ) -> anyhow::Result<Self> {
        let root = fs::canonicalize(root)
            .map_err(|e| anyhow::anyhow!("cannot open folder {}: {e}", root.display()))?;
        let lenient =
            |p: &Path| sandbox::canonicalize_lenient(p).unwrap_or_else(|_| p.to_path_buf());
        let protected = vec![
            lenient(&crate::paths::app_config_dir()),
            lenient(&trash_base),
        ];
        Ok(Self {
            root,
            grants,
            trash_base,
            protected,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Relative to the root; granted paths stay absolute.
    pub fn display(&self, abs: &Path) -> String {
        match abs.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => abs.to_string_lossy().into_owned(),
        }
    }

    fn is_protected(&self, abs: &Path) -> bool {
        self.protected.iter().any(|p| abs.starts_with(p))
    }

    fn is_base(&self, abs: &Path) -> bool {
        abs == self.root || self.grants.iter().any(|g| g.path == abs)
    }

    /// Resolves a model-supplied path to a canonical absolute one that is
    /// guaranteed inside the root or a grant (symlinks included).
    pub fn resolve(&self, user_path: &str, access: Access) -> Result<PathBuf, String> {
        self.resolve_inner(user_path, access, true)
    }

    /// `follow_last = false` keeps a final symlink as the link itself, for
    /// move/delete, which must act on the link and not its target.
    fn resolve_inner(
        &self,
        user_path: &str,
        access: Access,
        follow_last: bool,
    ) -> Result<PathBuf, String> {
        let raw = user_path.trim();
        let outside = || format!("path '{raw}' is outside the folder; use a path inside it");
        if raw.contains('\0') {
            return Err("path contains a NUL character".to_string());
        }
        let raw = if raw.is_empty() { "." } else { raw };
        let p = Path::new(raw);
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.root.join(p)
        };
        let norm = lexical_normalize(&joined);
        let canon = match (follow_last, norm.parent(), norm.file_name()) {
            (false, Some(parent), Some(name)) => {
                sandbox::canonicalize_lenient(parent).map(|c| c.join(name))
            }
            _ => sandbox::canonicalize_lenient(&norm),
        }
        .map_err(|e| format!("cannot use path '{raw}': {e}"))?;

        // Most specific base wins, like later mounts in the sandbox.
        let mut best: Option<(usize, bool)> = canon
            .starts_with(&self.root)
            .then_some((self.root.components().count(), true));
        for g in &self.grants {
            let inside = canon.starts_with(&g.path)
                && (g.recursive || canon == g.path || canon.parent() == Some(g.path.as_path()));
            let depth = g.path.components().count();
            if inside && best.map_or(true, |(d, _)| depth > d) {
                best = Some((depth, g.writable));
            }
        }
        let Some((_, writable)) = best else {
            return Err(outside());
        };
        if self.is_protected(&canon) {
            return Err(format!(
                "'{raw}' is managed by the app and can't be accessed"
            ));
        }
        if access == Access::Write && !writable {
            return Err(format!(
                "'{}' is read-only (a read-only granted path); it can't be changed",
                self.display(&canon)
            ));
        }
        Ok(canon)
    }

    fn io_msg(&self, abs: &Path, e: &std::io::Error) -> String {
        let d = self.display(abs);
        match e.kind() {
            std::io::ErrorKind::NotFound => format!("'{d}' does not exist"),
            std::io::ErrorKind::PermissionDenied => format!("permission denied: '{d}'"),
            _ => format!("'{d}': {e}"),
        }
    }
}

/// Resolves `.` and `..` without touching the filesystem. `..` at the top
/// stays there, so it can never climb above `/`.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Per-task state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeKind {
    Created,
    Modified,
    Moved { from: String },
    Deleted,
    DirCreated,
}

impl ChangeKind {
    pub fn label(&self) -> &'static str {
        match self {
            ChangeKind::Created => "created",
            ChangeKind::Modified => "modified",
            ChangeKind::Moved { .. } => "moved",
            ChangeKind::Deleted => "deleted",
            ChangeKind::DirCreated => "folder created",
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Change {
    /// Display path.
    pub path: String,
    pub kind: ChangeKind,
}

/// What one agent task has seen and done. Recorded by the app, never by the
/// model, so the end-of-task report doesn't depend on its honesty.
#[derive(Debug, Default, Clone)]
pub struct TaskState {
    read_files: HashSet<PathBuf>,
    changes: Vec<Change>,
    trash_run: Option<PathBuf>,
}

impl TaskState {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub fn has_read(&self, abs: &Path) -> bool {
        self.read_files.contains(abs)
    }

    #[cfg(test)]
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    /// This task's trash dir, once something has been backed up.
    #[cfg(test)]
    pub fn trash_run(&self) -> Option<&Path> {
        self.trash_run.as_deref()
    }

    /// One entry per path, in first-touched order, with net effect: created
    /// then edited is still "created"; created then deleted vanishes.
    pub fn changed_files_summary(&self) -> Vec<Change> {
        let mut out: Vec<Change> = Vec::new();
        for ch in &self.changes {
            match out.iter().position(|c| c.path == ch.path) {
                None => out.push(ch.clone()),
                Some(i) => match merge_kinds(&out[i].kind, &ch.kind) {
                    Some(k) => out[i].kind = k,
                    None => {
                        out.remove(i);
                    }
                },
            }
        }
        out
    }

    fn record(&mut self, path: String, kind: ChangeKind) {
        self.changes.push(Change { path, kind });
    }
}

/// `None` = the two cancel out.
fn merge_kinds(old: &ChangeKind, new: &ChangeKind) -> Option<ChangeKind> {
    use ChangeKind::*;
    match (old, new) {
        (Created, Modified) | (Created, Created) => Some(Created),
        (Created, Deleted) | (DirCreated, Deleted) => None,
        (Modified, Modified) => Some(Modified),
        (Moved { from }, Modified) => Some(Moved { from: from.clone() }),
        (Deleted, Created) => Some(Modified),
        (_, new) => Some(new.clone()),
    }
}

// ---------------------------------------------------------------------------
// Read-only tools
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
    Link,
}

fn human_size(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.1} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Simple glob to regex: `*` and `?` stay within a path segment, `**`
/// crosses them, `**/` also matches zero directories.
fn glob_regex(glob: &str) -> Result<regex::Regex, String> {
    let chars: Vec<char> = glob.trim().trim_start_matches("./").chars().collect();
    if chars.is_empty() {
        return Err("glob is empty".to_string());
    }
    let mut re = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                if chars.get(i) == Some(&'/') {
                    i += 1;
                    re.push_str("(?:.*/)?");
                } else {
                    re.push_str(".*");
                }
                continue;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    regex::Regex::new(&re).map_err(|e| format!("bad glob '{glob}': {e}"))
}

/// A glob without `/` matches the file name; with `/`, the path relative to
/// the search base.
fn glob_matches(re: &regex::Regex, glob: &str, base: &Path, path: &Path) -> bool {
    if glob.contains('/') {
        path.strip_prefix(base)
            .is_ok_and(|rel| re.is_match(&rel.to_string_lossy()))
    } else {
        path.file_name()
            .is_some_and(|n| re.is_match(&n.to_string_lossy()))
    }
}

fn looks_binary(file: &mut fs::File) -> std::io::Result<bool> {
    let mut probe = Vec::with_capacity(BINARY_PROBE_BYTES);
    file.by_ref()
        .take(BINARY_PROBE_BYTES as u64)
        .read_to_end(&mut probe)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(probe.contains(&0))
}

fn trim_eol(buf: &mut Vec<u8>) {
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
}

/// Opens a regular file for reading. O_NONBLOCK makes opening a FIFO return
/// at once instead of waiting for a writer; the type is checked on the open
/// descriptor, so a file swapped for a FIFO after listing can't hang us.
fn open_regular(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(file)
}

fn read_regular(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    open_regular(path)?.read_to_end(&mut buf)?;
    Ok(buf)
}

impl Workspace {
    /// Sorted children of `dir`, skipping app-owned dirs and special files. Symlinks are
    /// reported as links and never followed.
    fn children(&self, dir: &Path) -> Vec<(String, PathBuf, Kind)> {
        let Ok(rd) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut v: Vec<(String, PathBuf, Kind)> = rd
            .flatten()
            .filter_map(|e| {
                let t = e.file_type().ok()?;
                let kind = if t.is_symlink() {
                    Kind::Link
                } else if t.is_dir() {
                    Kind::Dir
                } else if t.is_file() {
                    Kind::File
                } else {
                    return None; // FIFO/socket/device: opening one can block forever
                };
                let path = e.path();
                (!self.is_protected(&path))
                    .then(|| (e.file_name().to_string_lossy().into_owned(), path, kind))
            })
            .collect();
        v.sort_by(|a, b| {
            a.0.to_lowercase()
                .cmp(&b.0.to_lowercase())
                .then(a.0.cmp(&b.0))
        });
        v
    }

    /// Depth-first, sorted. `visit` returns false to stop everything.
    fn walk(&self, dir: &Path, visit: &mut dyn FnMut(&Path, Kind) -> bool) -> bool {
        for (name, path, kind) in self.children(dir) {
            if !visit(&path, kind) {
                return false;
            }
            if kind == Kind::Dir && !is_skipped_dir(&name) && !self.walk(&path, visit) {
                return false;
            }
        }
        true
    }

    pub fn list_dir(&self, path: &str, depth: Option<u32>) -> Result<String, String> {
        let abs = self.resolve(path, Access::Read)?;
        let md = fs::metadata(&abs).map_err(|e| self.io_msg(&abs, &e))?;
        let disp = self.display(&abs);
        if !md.is_dir() {
            return Err(format!("'{disp}' is a file, not a folder; use read_file"));
        }
        let depth = depth.unwrap_or(1).clamp(1, MAX_LIST_DEPTH) as usize;
        let mut lines = Vec::new();
        let mut total = 0usize;
        self.list_level(&abs, 0, depth, &mut lines, &mut total);
        if total == 0 {
            return Ok(format!("{disp}/ (empty)"));
        }
        let mut out = format!("{disp}/\n{}", lines.join("\n"));
        if total > lines.len() {
            out.push_str(&format!(
                "\n... {} more entries not shown; list a subfolder or use search/find_files",
                total - lines.len()
            ));
        }
        Ok(out)
    }

    fn list_level(
        &self,
        dir: &Path,
        level: usize,
        depth: usize,
        lines: &mut Vec<String>,
        total: &mut usize,
    ) {
        let indent = "  ".repeat(level);
        for (name, path, kind) in self.children(dir) {
            *total += 1;
            let expand = kind == Kind::Dir && level + 1 < depth;
            let line = match kind {
                Kind::Dir if expand && is_skipped_dir(&name) => {
                    format!("{indent}{name}/ (not expanded)")
                }
                Kind::Dir => format!("{indent}{name}/"),
                Kind::Link => {
                    let target = fs::read_link(&path)
                        .map(|t| t.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| "?".to_string());
                    format!("{indent}{name} -> {target}")
                }
                Kind::File => {
                    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    format!("{indent}{name}  ({})", human_size(size))
                }
            };
            if lines.len() < MAX_LIST_ENTRIES {
                lines.push(line);
            }
            if expand && !is_skipped_dir(&name) {
                self.list_level(&path, level + 1, depth, lines, total);
            }
        }
    }

    pub fn read_file(
        &self,
        path: &str,
        offset: Option<u32>,
        limit: Option<u32>,
        st: &mut TaskState,
    ) -> Result<String, String> {
        let abs = self.resolve(path, Access::Read)?;
        let disp = self.display(&abs);
        let md = fs::metadata(&abs).map_err(|e| self.io_msg(&abs, &e))?;
        if md.is_dir() {
            return Err(format!("'{disp}' is a folder, not a file; use list_dir"));
        }
        if !md.is_file() {
            return Err(format!("'{disp}' is not a regular file"));
        }
        let big = md.len() > MAX_FILE_BYTES;
        if big && offset.is_none() {
            return Err(format!(
                "'{disp}' is {} (over 10 MB); pass offset (and limit) to read part of it, or use search",
                human_size(md.len())
            ));
        }
        let start = offset.unwrap_or(1).max(1) as usize;
        let limit = limit.unwrap_or(DEFAULT_READ_LIMIT).clamp(1, MAX_READ_LIMIT) as usize;

        let mut file = open_regular(&abs).map_err(|e| self.io_msg(&abs, &e))?;
        if looks_binary(&mut file).map_err(|e| self.io_msg(&abs, &e))? {
            return Ok(format!("{disp}: binary file, {} bytes", md.len()));
        }
        let mut reader = BufReader::new(file);
        let mut buf = Vec::new();
        let mut line_no = 0usize;
        let mut shown: Vec<(usize, String)> = Vec::new();
        let end = start + limit - 1;
        let mut more = false;
        loop {
            buf.clear();
            let n = reader
                .read_until(b'\n', &mut buf)
                .map_err(|e| self.io_msg(&abs, &e))?;
            if n == 0 {
                break;
            }
            line_no += 1;
            if line_no >= start && line_no <= end {
                trim_eol(&mut buf);
                let text = String::from_utf8_lossy(&buf);
                shown.push((line_no, clip_chars(&text, MAX_LINE_CHARS)));
            } else if line_no > end {
                more = true;
                if big {
                    break; // total unknown; don't scan gigabytes
                }
            }
        }
        if line_no == 0 {
            st.read_files.insert(abs);
            return Ok(format!("{disp} (empty file)"));
        }
        let Some(last) = shown.last().map(|(n, _)| *n) else {
            return Err(format!(
                "offset {start} is past the end of '{disp}'{}",
                if big {
                    String::new()
                } else {
                    format!(" ({line_no} lines)")
                }
            ));
        };
        let first = shown[0].0;
        let total = (!big).then_some(line_no);
        let mut out = match total {
            Some(n) => format!("{disp} (lines {first}-{last} of {n})\n"),
            None => format!("{disp} (lines {first}-{last})\n"),
        };
        for (n, text) in &shown {
            out.push_str(&format!("{n:>6}\t{text}\n"));
        }
        if more {
            out.push_str(&format!("[more: use offset={}]", last + 1));
        } else if out.ends_with('\n') {
            out.pop();
        }
        st.read_files.insert(abs);
        Ok(out)
    }

    pub fn search(
        &self,
        pattern: &str,
        path: &str,
        glob: Option<&str>,
        use_regex: bool,
        case_sensitive: bool,
    ) -> Result<String, String> {
        if pattern.is_empty() {
            return Err("pattern is empty".to_string());
        }
        let source = if use_regex {
            pattern.to_string()
        } else {
            regex::escape(pattern)
        };
        let re = regex::RegexBuilder::new(&source)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|e| format!("invalid regex: {e}"))?;
        let glob = glob.map(str::trim).filter(|g| !g.is_empty());
        let glob_re = glob.map(glob_regex).transpose()?;

        let abs = self.resolve(path, Access::Read)?;
        let md = fs::metadata(&abs).map_err(|e| self.io_msg(&abs, &e))?;
        let disp = self.display(&abs);

        let mut files: Vec<PathBuf> = Vec::new();
        if md.is_dir() {
            self.walk(&abs, &mut |p, kind| {
                if kind == Kind::File
                    && glob_re
                        .as_ref()
                        .zip(glob)
                        .map_or(true, |(r, g)| glob_matches(r, g, &abs, p))
                {
                    files.push(p.to_path_buf());
                }
                true
            });
        } else {
            files.push(abs.clone());
        }

        let mut lines = Vec::new();
        let (mut total, mut files_hit) = (0usize, 0usize);
        'files: for f in &files {
            let Ok(mut file) = open_regular(f) else {
                continue;
            };
            if file.metadata().map_or(true, |m| m.len() > MAX_FILE_BYTES) {
                continue;
            }
            if looks_binary(&mut file).unwrap_or(true) {
                continue;
            }
            let fdisp = self.display(f);
            let mut reader = BufReader::new(file);
            let mut buf = Vec::new();
            let mut n = 0usize;
            let mut hit = false;
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                n += 1;
                trim_eol(&mut buf);
                let text = String::from_utf8_lossy(&buf);
                if !re.is_match(&text) {
                    continue;
                }
                hit = true;
                total += 1;
                if lines.len() < MAX_SEARCH_MATCHES {
                    lines.push(format!(
                        "{fdisp}:{n}: {}",
                        clip_chars(text.trim(), MAX_MATCH_CHARS)
                    ));
                }
                if total >= MAX_SEARCH_COUNT {
                    files_hit += 1;
                    break 'files;
                }
            }
            if hit {
                files_hit += 1;
            }
        }
        if total == 0 {
            return Ok(format!("no matches for \"{pattern}\" in {disp}"));
        }
        let plus = if total >= MAX_SEARCH_COUNT { "+" } else { "" };
        let mut out = lines.join("\n");
        out.push_str(&format!("\n{total}{plus} matches in {files_hit} files"));
        if total > lines.len() {
            out.push_str(&format!(
                "; showing the first {}. Narrow it with path or glob",
                lines.len()
            ));
        }
        Ok(out)
    }

    pub fn find_files(&self, glob: &str, path: &str) -> Result<String, String> {
        let re = glob_regex(glob)?;
        let abs = self.resolve(path, Access::Read)?;
        let md = fs::metadata(&abs).map_err(|e| self.io_msg(&abs, &e))?;
        if !md.is_dir() {
            return Err(format!("'{}' is a file, not a folder", self.display(&abs)));
        }
        let mut found: Vec<String> = Vec::new();
        let mut total = 0usize;
        self.walk(&abs, &mut |p, kind| {
            if glob_matches(&re, glob.trim().trim_start_matches("./"), &abs, p) {
                total += 1;
                if found.len() < MAX_FIND_RESULTS {
                    let d = self.display(p);
                    found.push(if kind == Kind::Dir {
                        format!("{d}/")
                    } else {
                        d
                    });
                }
            }
            total < MAX_SEARCH_COUNT * 2
        });
        if total == 0 {
            // A guessed name ("my_shopping_list.txt") misses; showing what is
            // there keeps a small model from creating a duplicate instead.
            let here: Vec<String> = self
                .children(&abs)
                .into_iter()
                .take(20)
                .map(|(name, _, kind)| match kind {
                    Kind::Dir => format!("{name}/"),
                    _ => name,
                })
                .collect();
            let listing = if here.is_empty() {
                "it is empty".to_string()
            } else {
                format!("it contains: {}", here.join(", "))
            };
            return Ok(format!(
                "no files match '{glob}' in {}; {listing}",
                self.display(&abs)
            ));
        }
        let mut out = found.join("\n");
        if total > found.len() {
            out.push_str(&format!(
                "\n... {} more not shown; narrow the glob or path",
                total - found.len()
            ));
        }
        Ok(out)
    }

    /// The four read-only tools.
    pub fn run_readonly(&self, tool: &Tool, st: &mut TaskState) -> Result<String, String> {
        match tool {
            Tool::ListDir { path, depth } => self.list_dir(path, *depth),
            Tool::ReadFile {
                path,
                offset,
                limit,
            } => self.read_file(path, *offset, *limit, st),
            Tool::Search {
                pattern,
                path,
                glob,
                regex,
                case_sensitive,
            } => self.search(pattern, path, glob.as_deref(), *regex, *case_sensitive),
            Tool::FindFiles { glob, path } => self.find_files(glob, path),
            other => Err(format!("{} is not a read-only tool", other.name())),
        }
    }

    /// Anything but `run_command`/`finish`.
    pub fn execute(&self, tool: &Tool, st: &mut TaskState) -> Result<String, String> {
        if is_read_only(tool) {
            self.run_readonly(tool, st)
        } else {
            self.apply(tool, st)
        }
    }
}

/// A whole-file write that would drop most of an existing file's lines.
/// Small models "add a line" this way; the agent refuses the first attempt
/// with this explanation and lets an identical re-send through to the user.
pub fn destructive_write_warning(ws: &Workspace, tool: &Tool) -> Option<String> {
    let Tool::WriteFile {
        path,
        content,
        append: false,
    } = tool
    else {
        return None;
    };
    let abs = ws.resolve(path, Access::Write).ok()?;
    if !fs::symlink_metadata(&abs).ok()?.is_file() {
        return None;
    }
    let old = fs::read_to_string(&abs).ok()?;
    let old_lines: Vec<&str> = old.lines().filter(|l| !l.trim().is_empty()).collect();
    if old_lines.is_empty() {
        return None;
    }
    let new_lines: std::collections::HashSet<&str> = content.lines().map(str::trim).collect();
    let removed: Vec<&str> = old_lines
        .iter()
        .copied()
        .filter(|l| !new_lines.contains(l.trim()))
        .collect();
    if removed.len() * 2 <= old_lines.len() {
        return None;
    }
    let sample = removed
        .iter()
        .take(3)
        .map(|l| format!("  {}", l.chars().take(80).collect::<String>()))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "not written: write_file replaces the WHOLE file, and this would remove {} of the {} \
         existing lines of '{}', for example:\n{sample}\nTo add text, call write_file with \
         append: true (or use an append block). To change part of it, use edit_file. If you really \
         mean to replace everything, send exactly the same write_file call again.",
        removed.len(),
        old_lines.len(),
        ws.display(&abs)
    ))
}

pub fn is_read_only(tool: &Tool) -> bool {
    matches!(
        tool,
        Tool::ListDir { .. } | Tool::ReadFile { .. } | Tool::Search { .. } | Tool::FindFiles { .. }
    )
}

/// Mutating tools the user must approve (`make_dir` is mutating but not).
pub fn needs_confirmation(tool: &Tool) -> bool {
    matches!(
        tool,
        Tool::WriteFile { .. } | Tool::EditFile { .. } | Tool::Move { .. } | Tool::Delete { .. }
    )
}

// ---------------------------------------------------------------------------
// Mutating tools: preview + apply
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Preview {
    pub title: String,
    /// Unified diff, capped at 400 lines.
    pub diff: Option<String>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub new_file: bool,
}

enum Plan {
    Write {
        abs: PathBuf,
        content: String,
        old: Option<Vec<u8>>,
        append: bool,
    },
    Edit {
        abs: PathBuf,
        old: String,
        new: String,
        replacements: usize,
    },
    Move {
        from: PathBuf,
        to: PathBuf,
    },
    Delete {
        abs: PathBuf,
    },
    MakeDir {
        abs: PathBuf,
    },
}

#[derive(Clone, Copy)]
enum Backup {
    Copy,
    Move,
}

impl Workspace {
    /// Runs every validation `apply` would, so a doomed call fails before
    /// the user is asked.
    pub fn preview(&self, tool: &Tool, st: &TaskState) -> Result<Preview, String> {
        let plan = self.plan(tool, st)?;
        let title = tool.title();
        Ok(match plan {
            Plan::Write {
                abs, content, old, ..
            } => {
                let disp = self.display(&abs);
                let diff = match &old {
                    None => unified_diff(&disp, None, &content),
                    Some(bytes) => match std::str::from_utf8(bytes) {
                        Ok(text) => unified_diff(&disp, Some(text), &content),
                        Err(_) => "(the existing file is binary or not UTF-8; it will be replaced)"
                            .to_string(),
                    },
                };
                Preview {
                    title,
                    diff: Some(diff),
                    new_file: old.is_none(),
                }
            }
            Plan::Edit { abs, old, new, .. } => Preview {
                title,
                diff: Some(unified_diff(&self.display(&abs), Some(&old), &new)),
                new_file: false,
            },
            Plan::Move { .. } | Plan::Delete { .. } | Plan::MakeDir { .. } => Preview {
                title,
                diff: None,
                new_file: false,
            },
        })
    }

    /// Backs up first (copy before overwrite/edit, move for delete), then
    /// changes. Returns the text for the model.
    pub fn apply(&self, tool: &Tool, st: &mut TaskState) -> Result<String, String> {
        match self.plan(tool, st)? {
            Plan::Write {
                abs,
                content,
                old,
                append,
            } => {
                let disp = self.display(&abs);
                let lines = content.lines().count();
                if old.as_deref() == Some(content.as_bytes()) {
                    st.read_files.insert(abs);
                    return Ok(format!(
                        "{disp} already has exactly this content; nothing changed"
                    ));
                }
                let existed = old.is_some();
                if existed {
                    self.backup(st, &abs, Backup::Copy)?;
                }
                self.write_bytes(&abs, content.as_bytes())?;
                st.read_files.insert(abs);
                st.record(
                    disp.clone(),
                    if existed {
                        ChangeKind::Modified
                    } else {
                        ChangeKind::Created
                    },
                );
                Ok(if append {
                    format!("appended to {disp} (now {lines} lines, backup kept)")
                } else if existed {
                    format!("overwrote {disp} ({lines} lines, backup kept)")
                } else {
                    format!("created {disp} ({lines} lines)")
                })
            }
            Plan::Edit {
                abs,
                new,
                replacements,
                ..
            } => {
                let disp = self.display(&abs);
                self.backup(st, &abs, Backup::Copy)?;
                self.write_bytes(&abs, new.as_bytes())?;
                st.record(disp.clone(), ChangeKind::Modified);
                Ok(format!(
                    "edited {disp}: {replacements} replacement{} (backup kept)",
                    if replacements == 1 { "" } else { "s" }
                ))
            }
            Plan::Move { from, to } => {
                let (fd, td) = (self.display(&from), self.display(&to));
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent).map_err(|e| self.io_msg(parent, &e))?;
                }
                move_path(&from, &to).map_err(|e| format!("could not move '{fd}': {e}"))?;
                let moved: Vec<PathBuf> = st
                    .read_files
                    .iter()
                    .filter(|p| p.starts_with(&from))
                    .cloned()
                    .collect();
                for p in moved {
                    st.read_files.remove(&p);
                    if let Ok(rel) = p.strip_prefix(&from) {
                        st.read_files.insert(to.join(rel));
                    }
                }
                st.record(td.clone(), ChangeKind::Moved { from: fd.clone() });
                Ok(format!("moved {fd} -> {td}"))
            }
            Plan::Delete { abs } => {
                let disp = self.display(&abs);
                self.backup(st, &abs, Backup::Move)?;
                st.read_files.retain(|p| !p.starts_with(&abs));
                st.record(disp.clone(), ChangeKind::Deleted);
                Ok(format!("deleted {disp}: moved to trash (recoverable)"))
            }
            Plan::MakeDir { abs } => {
                let disp = self.display(&abs);
                if abs.is_dir() {
                    return Ok(format!("{disp} already exists"));
                }
                fs::create_dir_all(&abs).map_err(|e| self.io_msg(&abs, &e))?;
                st.record(disp.clone(), ChangeKind::DirCreated);
                Ok(format!("created folder {disp}"))
            }
        }
    }

    fn plan(&self, tool: &Tool, st: &TaskState) -> Result<Plan, String> {
        match tool {
            Tool::WriteFile {
                path,
                content,
                append,
            } => {
                let abs = self.resolve(path, Access::Write)?;
                let disp = self.display(&abs);
                if self.is_base(&abs) || abs.is_dir() {
                    return Err(format!("'{disp}' is a folder; give a file name"));
                }
                let old = match read_regular(&abs) {
                    Ok(b) => Some(b),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(self.io_msg(&abs, &e)),
                };
                // Small models "add a line" by rewriting a file they never
                // looked at, silently dropping its contents.
                if !*append && old.is_some() && !st.read_files.contains(&abs) {
                    return Err(format!(
                        "'{disp}' already exists and you haven't read it yet. Read it with \
                         read_file first, then use edit_file to change part of it (or \
                         write_file to replace all of it)"
                    ));
                }
                let content = match (*append, &old) {
                    (true, Some(bytes)) => {
                        let text = std::str::from_utf8(bytes).map_err(|_| {
                            format!("'{disp}' is not a UTF-8 text file; it can't be appended to")
                        })?;
                        append_text(text, content)
                    }
                    _ => content.clone(),
                };
                Ok(Plan::Write {
                    abs,
                    content,
                    old,
                    append: *append,
                })
            }
            Tool::EditFile { path, edits } => self.plan_edit(path, edits, st),
            Tool::Move { from, to } => self.plan_move(from, to),
            Tool::Delete { path } => {
                let abs = self.resolve_inner(path, Access::Write, false)?;
                let disp = self.display(&abs);
                if self.is_base(&abs) {
                    return Err(format!(
                        "'{disp}' is the working folder itself; it can't be deleted"
                    ));
                }
                if fs::symlink_metadata(&abs).is_err() {
                    return Err(format!("'{disp}' does not exist"));
                }
                self.refuse_if_contains_protected(&abs, "deleted")?;
                Ok(Plan::Delete { abs })
            }
            Tool::MakeDir { path } => {
                let abs = self.resolve(path, Access::Write)?;
                if abs.exists() && !abs.is_dir() {
                    return Err(format!(
                        "'{}' already exists and is a file",
                        self.display(&abs)
                    ));
                }
                Ok(Plan::MakeDir { abs })
            }
            other => Err(format!("{} is not a file-changing tool", other.name())),
        }
    }

    fn plan_edit(&self, path: &str, edits: &[Edit], st: &TaskState) -> Result<Plan, String> {
        if edits.is_empty() {
            return Err("edits is empty; give at least one {old_text, new_text}".to_string());
        }
        let abs = self.resolve(path, Access::Write)?;
        let disp = self.display(&abs);
        let md = fs::metadata(&abs).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("'{disp}' does not exist; use write_file to create it")
            } else {
                self.io_msg(&abs, &e)
            }
        })?;
        if md.is_dir() {
            return Err(format!("'{disp}' is a folder, not a file"));
        }
        if !md.is_file() {
            return Err(format!("'{disp}' is not a regular file"));
        }
        if !st.read_files.contains(&abs) {
            return Err(format!(
                "read the file first with read_file, then retry edit_file on '{disp}'"
            ));
        }
        let old = read_regular(&abs)
            .map_err(|e| self.io_msg(&abs, &e))
            .and_then(|b| {
                String::from_utf8(b).map_err(|_| {
                    format!("'{disp}' is not a UTF-8 text file; edit_file can't change it")
                })
            })?;
        // Small models try to "add to the end" with a blank old_text.
        let (new, replacements) = match edits {
            [e] if e.old_text.trim().is_empty() && !e.new_text.is_empty() => {
                (append_text(&old, &e.new_text), 1)
            }
            _ => apply_edits(&old, edits, &disp)?,
        };
        if new == old {
            return Err(format!("these edits would not change '{disp}'"));
        }
        Ok(Plan::Edit {
            abs,
            old,
            new,
            replacements,
        })
    }

    fn plan_move(&self, from: &str, to: &str) -> Result<Plan, String> {
        let from_abs = self.resolve_inner(from, Access::Write, false)?;
        let fd = self.display(&from_abs);
        if self.is_base(&from_abs) {
            return Err(format!(
                "'{fd}' is the working folder itself; it can't be moved"
            ));
        }
        if fs::symlink_metadata(&from_abs).is_err() {
            return Err(format!("'{fd}' does not exist"));
        }
        self.refuse_if_contains_protected(&from_abs, "moved")?;
        let to_abs = self.resolve(to, Access::Write)?;
        let into_dir = to.trim_end().ends_with('/') || to_abs.is_dir();
        let target = if into_dir {
            let name = from_abs
                .file_name()
                .ok_or_else(|| format!("cannot move '{fd}'"))?;
            to_abs.join(name)
        } else {
            to_abs
        };
        let td = self.display(&target);
        self.refuse_if_contains_protected(&target, "moved")?;
        if target == from_abs {
            return Err(format!("'{fd}' is already there"));
        }
        if target.starts_with(&from_abs) {
            return Err(format!("can't move '{fd}' into itself"));
        }
        if fs::symlink_metadata(&target).is_ok() {
            return Err(format!(
                "destination '{td}' already exists; pick another name, or delete it first"
            ));
        }
        Ok(Plan::Move {
            from: from_abs,
            to: target,
        })
    }

    /// Move/delete act on whole subtrees, so refuse an operand that holds a
    /// protected location: the app's own dirs, a read-only grant (it would
    /// become writable at its new place) or the working folder itself.
    fn refuse_if_contains_protected(&self, abs: &Path, verb: &str) -> Result<(), String> {
        let holds = self.protected.iter().any(|p| p.starts_with(abs))
            || self.root.starts_with(abs)
            || self
                .grants
                .iter()
                .any(|g| !g.writable && g.path.starts_with(abs));
        if holds {
            return Err(format!(
                "'{}' contains the working folder, app data or a read-only folder; it can't be {verb}",
                self.display(abs)
            ));
        }
        Ok(())
    }

    fn write_bytes(&self, abs: &Path, bytes: &[u8]) -> Result<(), String> {
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create folder '{}': {e}", self.display(parent)))?;
        }
        let mode = fs::metadata(abs)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777);
        crate::paths::write_atomic(abs, bytes, mode).map_err(|e| self.io_msg(abs, &e))
    }

    /// Copies or moves `abs` into this task's trash run dir, keeping its
    /// relative path (granted paths: absolute path without the leading `/`).
    fn backup(&self, st: &mut TaskState, abs: &Path, how: Backup) -> Result<(), String> {
        let run = st
            .trash_run
            .get_or_insert_with(|| self.trash_base.join(sandbox::generate_trash_run_id()))
            .clone();
        let rel = abs
            .strip_prefix(&self.root)
            .or_else(|_| abs.strip_prefix("/"))
            .unwrap_or(abs);
        let mut dest = run.join(rel);
        if fs::symlink_metadata(&dest).is_ok() {
            let mut n = 2;
            loop {
                let mut name = dest.as_os_str().to_os_string();
                name.push(format!(".{n}"));
                let cand = PathBuf::from(name);
                if fs::symlink_metadata(&cand).is_err() {
                    dest = cand;
                    break;
                }
                n += 1;
            }
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create the backup folder: {e}"))?;
        }
        let result = match how {
            Backup::Copy => copy_recursive(abs, &dest),
            Backup::Move => move_path(abs, &dest),
        };
        result.map_err(|e| format!("could not back up '{}': {e}", self.display(abs)))
    }
}

fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    let md = fs::symlink_metadata(src)?;
    if md.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)
    } else if md.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        fs::set_permissions(dst, md.permissions())
    } else if md.is_file() {
        fs::copy(src, dst).map(|_| ())
    } else {
        // fs::copy would open a FIFO and block forever.
        Err(std::io::Error::other("not a regular file"))
    }
}

/// `rename`, or copy-then-delete across devices.
fn move_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    match fs::rename(src, dst) {
        Err(e) if e.raw_os_error() == Some(EXDEV) => {
            copy_recursive(src, dst)?;
            let md = fs::symlink_metadata(src)?;
            if md.is_dir() {
                fs::remove_dir_all(src)
            } else {
                fs::remove_file(src)
            }
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// edit_file matching
// ---------------------------------------------------------------------------

/// `old` + `add`, with a newline between them unless `old` already ends in one.
fn append_text(old: &str, add: &str) -> String {
    let sep = if old.is_empty() || old.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{old}{sep}{add}")
}

fn line_of(text: &str, byte_pos: usize) -> usize {
    text[..byte_pos].matches('\n').count() + 1
}

fn list_lines(lines: &[usize]) -> String {
    let mut s: Vec<String> = lines.iter().take(10).map(|n| n.to_string()).collect();
    if lines.len() > 10 {
        s.push("…".to_string());
    }
    s.join(", ")
}

fn leading_ws(s: &str) -> &str {
    &s[..s.len() - s.trim_start().len()]
}

/// All edits applied in order to the in-memory text; any failure aborts the
/// lot. Returns the new text and the number of replacements.
fn apply_edits(text: &str, edits: &[Edit], disp: &str) -> Result<(String, usize), String> {
    let mut cur = text.to_string();
    let mut count = 0;
    for (i, e) in edits.iter().enumerate() {
        let prefix = if edits.len() > 1 {
            format!("edit {}: ", i + 1)
        } else {
            String::new()
        };
        let (next, n) = apply_one(&cur, e, disp).map_err(|m| format!("{prefix}{m}"))?;
        cur = next;
        count += n;
    }
    Ok((cur, count))
}

fn apply_one(text: &str, e: &Edit, disp: &str) -> Result<(String, usize), String> {
    if e.old_text.is_empty() {
        return Err(
            "old_text is empty; copy the exact lines to replace from read_file".to_string(),
        );
    }
    let crlf = text.contains("\r\n");
    let new_text = if crlf && !e.new_text.contains('\r') {
        e.new_text.replace('\n', "\r\n")
    } else {
        e.new_text.clone()
    };

    let hits: Vec<usize> = text.match_indices(&e.old_text).map(|(i, _)| i).collect();
    if !hits.is_empty() {
        if hits.len() > 1 && !e.replace_all {
            let lines: Vec<usize> = hits.iter().map(|&p| line_of(text, p)).collect();
            return Err(multiple_msg(&lines));
        }
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for &p in &hits {
            out.push_str(&text[last..p]);
            out.push_str(&new_text);
            last = p + e.old_text.len();
        }
        out.push_str(&text[last..]);
        return Ok((out, hits.len()));
    }

    // Whitespace-tolerant: same lines modulo indentation/trailing blanks.
    let spans = tolerant_matches(text, &e.old_text);
    if !spans.is_empty() {
        if spans.len() > 1 && !e.replace_all {
            let lines: Vec<usize> = spans.iter().map(|s| s.first_line).collect();
            return Err(multiple_msg(&lines));
        }
        let old_indent = e
            .old_text
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| leading_ws(l).to_string())
            .unwrap_or_default();
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for s in &spans {
            out.push_str(&text[last..s.start]);
            let file_indent = leading_ws(&text[s.start..s.end]).to_string();
            let mut repl = reindent(&new_text, &old_indent, &file_indent);
            if e.old_text.ends_with('\n') && repl.ends_with('\n') {
                repl.pop();
                if repl.ends_with('\r') {
                    repl.pop();
                }
            }
            if text[..s.end].ends_with('\r') && !repl.ends_with('\r') {
                repl.push('\r');
            }
            out.push_str(&repl);
            last = s.end;
        }
        out.push_str(&text[last..]);
        return Ok((out, spans.len()));
    }

    Err(closest_msg(text, &e.old_text, disp))
}

fn multiple_msg(lines: &[usize]) -> String {
    format!(
        "old_text matches {} times (lines {}); include more surrounding lines or set replace_all",
        lines.len(),
        list_lines(lines)
    )
}

/// Swaps `old_indent` for `file_indent` at the start of each line that has it.
fn reindent(new_text: &str, old_indent: &str, file_indent: &str) -> String {
    if old_indent == file_indent {
        return new_text.to_string();
    }
    new_text
        .split('\n')
        .map(|l| match l.strip_prefix(old_indent) {
            Some(rest) if !l.trim().is_empty() => format!("{file_indent}{rest}"),
            _ => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Span {
    start: usize,
    end: usize,
    first_line: usize,
}

/// Lines of `old` with blank edges dropped, each trimmed.
fn trimmed_lines(old: &str) -> Vec<&str> {
    let mut v: Vec<&str> = old.split('\n').map(str::trim).collect();
    while v.first().is_some_and(|l| l.is_empty()) {
        v.remove(0);
    }
    while v.last().is_some_and(|l| l.is_empty()) {
        v.pop();
    }
    v
}

/// Byte span of each line (without its `\n`).
fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let mut pos = 0;
    for l in text.split('\n') {
        v.push((pos, pos + l.len()));
        pos += l.len() + 1;
    }
    v
}

fn tolerant_matches(text: &str, old: &str) -> Vec<Span> {
    let want = trimmed_lines(old);
    if want.is_empty() {
        return Vec::new();
    }
    let spans = line_spans(text);
    let have: Vec<&str> = spans.iter().map(|&(s, e)| text[s..e].trim()).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + want.len() <= have.len() {
        if have[i..i + want.len()] == want[..] {
            out.push(Span {
                start: spans[i].0,
                end: spans[i + want.len() - 1].1,
                first_line: i + 1,
            });
            i += want.len();
        } else {
            i += 1;
        }
    }
    out
}

fn bigrams(s: &str) -> Vec<u16> {
    let b = s.as_bytes();
    let mut v: Vec<u16> = match b.len() {
        0 => Vec::new(),
        1 => vec![(b[0] as u16) << 8],
        _ => b
            .windows(2)
            .map(|w| ((w[0] as u16) << 8) | w[1] as u16)
            .collect(),
    };
    v.sort_unstable();
    v
}

fn dice(a: &[u16], b: &[u16]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let (mut i, mut j, mut common) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    2.0 * common as f32 / (a.len() + b.len()) as f32
}

/// Error for a failed match: the window of file lines most like `old`.
fn closest_msg(text: &str, old: &str, disp: &str) -> String {
    let want = trimmed_lines(old);
    let spans = line_spans(text);
    let have: Vec<&str> = spans.iter().map(|&(s, e)| text[s..e].trim()).collect();
    if text.trim().is_empty() || want.is_empty() {
        return format!("old_text not found in {disp} (the file has no matching text)");
    }
    let n = want.len().min(have.len());
    let want_bg: Vec<Vec<u16>> = want.iter().map(|l| bigrams(l)).collect();
    let have_bg: Vec<Vec<u16>> = have.iter().map(|l| bigrams(l)).collect();
    let sim = |w: usize, h: usize| -> f32 {
        if want[w] == have[h] {
            if want[w].is_empty() {
                0.1
            } else {
                1.0
            }
        } else {
            dice(&want_bg[w], &have_bg[h])
        }
    };
    let (mut best, mut best_at) = (-1.0f32, 0usize);
    for start in 0..=(have.len() - n) {
        let score: f32 = (0..n).map(|j| sim(j, start + j)).sum();
        if score > best {
            best = score;
            best_at = start;
        }
    }
    let shown = n.min(15);
    let mut out = format!(
        "old_text not found in {disp}. Closest lines {}-{}:",
        best_at + 1,
        best_at + n
    );
    for k in 0..shown {
        let raw = &text[spans[best_at + k].0..spans[best_at + k].1];
        out.push_str(&format!(
            "\n{:>6}\t{}",
            best_at + k + 1,
            clip_chars(raw.trim_end_matches('\r'), 200)
        ));
    }
    if n > shown {
        out.push_str("\n       …");
    }
    out.push_str("\nCopy the exact text from read_file and retry.");
    out
}

// ---------------------------------------------------------------------------
// Diff
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Del,
    Ins,
}

/// Line ops turning `a` into `b`: common prefix/suffix trimmed, LCS table on
/// the middle, and a plain delete-all/insert-all when that would be too big.
fn diff_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(Op, &'a str)> {
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }
    let (ma, mb) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let (n, m) = (ma.len(), mb.len());

    let mut ops: Vec<(Op, &str)> = a[..pre].iter().map(|l| (Op::Eq, *l)).collect();
    if n * m <= MAX_DP_CELLS {
        let w = m + 1;
        let mut t = vec![0u16; (n + 1) * w];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                t[i * w + j] = if ma[i] == mb[j] {
                    t[(i + 1) * w + j + 1] + 1
                } else {
                    t[(i + 1) * w + j].max(t[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if ma[i] == mb[j] {
                ops.push((Op::Eq, ma[i]));
                i += 1;
                j += 1;
            } else if t[(i + 1) * w + j] >= t[i * w + j + 1] {
                ops.push((Op::Del, ma[i]));
                i += 1;
            } else {
                ops.push((Op::Ins, mb[j]));
                j += 1;
            }
        }
        ops.extend(ma[i..].iter().map(|l| (Op::Del, *l)));
        ops.extend(mb[j..].iter().map(|l| (Op::Ins, *l)));
    } else {
        ops.extend(ma.iter().map(|l| (Op::Del, *l)));
        ops.extend(mb.iter().map(|l| (Op::Ins, *l)));
    }
    ops.extend(a[a.len() - suf..].iter().map(|l| (Op::Eq, *l)));
    ops
}

/// Unified diff with 3 lines of context. `old: None` means a new file.
pub fn unified_diff(path: &str, old: Option<&str>, new: &str) -> String {
    let old_text = old.unwrap_or("");
    let a: Vec<&str> = old_text.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a.len() > MAX_DIFF_FILE_LINES || b.len() > MAX_DIFF_FILE_LINES {
        return format!(
            "(too large for a diff preview: {} lines -> {} lines)",
            a.len(),
            b.len()
        );
    }
    if old.is_some() && a == b {
        return if old_text == new {
            "(no changes)".to_string()
        } else {
            "(only the trailing newline changes)".to_string()
        };
    }
    let from = if old.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".to_string()
    };
    let mut out: Vec<String> = vec![format!("--- {from}"), format!("+++ b/{path}")];

    let ops = diff_ops(&a, &b);
    // Cumulative old/new line counts before each op.
    let (mut before_old, mut before_new) = (vec![0usize], vec![0usize]);
    for (op, _) in &ops {
        let (o, n) = (*before_old.last().unwrap(), *before_new.last().unwrap());
        before_old.push(o + usize::from(*op != Op::Ins));
        before_new.push(n + usize::from(*op != Op::Del));
    }
    let changes: Vec<usize> = (0..ops.len()).filter(|&i| ops[i].0 != Op::Eq).collect();
    let mut k = 0;
    while k < changes.len() {
        let mut last = changes[k];
        let mut j = k + 1;
        while j < changes.len() && changes[j] - last <= 2 * DIFF_CONTEXT {
            last = changes[j];
            j += 1;
        }
        let s = changes[k].saturating_sub(DIFF_CONTEXT);
        let e = (last + DIFF_CONTEXT + 1).min(ops.len());
        let (oc, nc) = (before_old[e] - before_old[s], before_new[e] - before_new[s]);
        let os = if oc == 0 {
            before_old[s]
        } else {
            before_old[s] + 1
        };
        let ns = if nc == 0 {
            before_new[s]
        } else {
            before_new[s] + 1
        };
        out.push(format!("@@ -{os},{oc} +{ns},{nc} @@"));
        for (op, line) in &ops[s..e] {
            let sign = match op {
                Op::Eq => ' ',
                Op::Del => '-',
                Op::Ins => '+',
            };
            out.push(format!("{sign}{line}"));
        }
        k = j;
    }
    if out.len() > MAX_DIFF_LINES {
        let more = out.len() - MAX_DIFF_LINES;
        out.truncate(MAX_DIFF_LINES);
        out.push(format!("... (diff truncated, {more} more lines)"));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Env {
        base: PathBuf,
        root: PathBuf,
        trash: PathBuf,
        ws: Workspace,
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn env_with(grants: impl FnOnce(&Path) -> Vec<Grant>) -> Env {
        static N: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::temp_dir().join(format!(
            "llm-assistant-tools-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        let trash = base.join("trash");
        fs::create_dir_all(&root).unwrap();
        let base = fs::canonicalize(&base).unwrap();
        let root = base.join("root");
        let grants = grants(&base);
        let ws = Workspace::from_parts(&root, grants, trash.clone()).unwrap();
        Env {
            base,
            root,
            trash,
            ws,
        }
    }

    fn env() -> Env {
        env_with(|_| Vec::new())
    }

    fn put(e: &Env, rel: &str, text: &str) {
        let p = e.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn get(e: &Env, rel: &str) -> String {
        fs::read_to_string(e.root.join(rel)).unwrap()
    }

    fn edit(old: &str, new: &str) -> Edit {
        Edit {
            old_text: old.into(),
            new_text: new.into(),
            replace_all: false,
        }
    }

    fn do_edit(
        e: &Env,
        st: &mut TaskState,
        path: &str,
        edits: Vec<Edit>,
    ) -> Result<String, String> {
        e.ws.apply(
            &Tool::EditFile {
                path: path.into(),
                edits,
            },
            st,
        )
    }

    fn read(e: &Env, st: &mut TaskState, path: &str) -> String {
        e.ws.read_file(path, None, None, st).unwrap()
    }

    fn trashed(e: &Env) -> Vec<(String, String)> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, String)>) {
            for ent in fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = ent.path();
                if p.is_dir() {
                    walk(&p, base, out);
                } else if let Ok(t) = fs::read_to_string(&p) {
                    out.push((p.strip_prefix(base).unwrap().to_string_lossy().into(), t));
                }
            }
        }
        let mut out = Vec::new();
        for run in fs::read_dir(&e.trash).into_iter().flatten().flatten() {
            walk(&run.path(), &run.path(), &mut out);
        }
        out
    }

    // ---- types ------------------------------------------------------------

    #[test]
    fn tool_json_shapes_and_defaults() {
        let t: Tool = serde_json::from_str(r#"{"tool":"read_file","path":"a.txt"}"#).unwrap();
        assert_eq!(
            t,
            Tool::ReadFile {
                path: "a.txt".into(),
                offset: None,
                limit: None
            }
        );
        let t: Tool = serde_json::from_str(r#"{"tool":"list_dir"}"#).unwrap();
        assert_eq!(
            t,
            Tool::ListDir {
                path: ".".into(),
                depth: None
            }
        );
        let t: Tool = serde_json::from_str(r#"{"tool":"search","pattern":"x"}"#).unwrap();
        assert!(
            matches!(t, Tool::Search { ref path, regex: false, case_sensitive: false, .. } if path == ".")
        );
        let t: Tool = serde_json::from_str(
            r#"{"tool":"edit_file","path":"a","edits":[{"old_text":"x","new_text":"y"}]}"#,
        )
        .unwrap();
        assert_eq!(t.name(), "edit_file");
        let round: Tool = serde_json::from_str(&serde_json::to_string(&t).unwrap()).unwrap();
        assert_eq!(round, t);
        assert!(serde_json::from_str::<Tool>(r#"{"tool":"nope"}"#).is_err());
    }

    #[test]
    fn names_and_titles() {
        let t = Tool::ReadFile {
            path: "notes.txt".into(),
            offset: None,
            limit: None,
        };
        assert_eq!(t.name(), "read_file");
        assert_eq!(t.title(), "Read notes.txt (lines 1-200)");
        let t = Tool::ReadFile {
            path: "a".into(),
            offset: Some(50),
            limit: Some(5000),
        };
        assert_eq!(t.title(), "Read a (lines 50-1049)");
        assert_eq!(
            Tool::Move {
                from: "a".into(),
                to: "b".into()
            }
            .name(),
            "move"
        );
        assert_eq!(
            Tool::RunCommand {
                command: "ls\nwc".into()
            }
            .title(),
            "Run: ls ..."
        );
        assert_eq!(
            Tool::Finish {
                summary: "x".into()
            }
            .title(),
            "Finish"
        );
    }

    // ---- path resolution --------------------------------------------------

    #[test]
    fn resolve_accepts_paths_inside_the_root() {
        let e = env();
        put(&e, "a/b.txt", "x");
        assert_eq!(
            e.ws.resolve("a/b.txt", Access::Read).unwrap(),
            e.root.join("a/b.txt")
        );
        assert_eq!(
            e.ws.resolve("./a/../a/b.txt", Access::Read).unwrap(),
            e.root.join("a/b.txt")
        );
        assert_eq!(e.ws.resolve("", Access::Read).unwrap(), e.root);
        assert_eq!(e.ws.resolve(".", Access::Read).unwrap(), e.root);
        assert_eq!(
            e.ws.resolve(e.root.join("a/b.txt").to_str().unwrap(), Access::Read)
                .unwrap(),
            e.root.join("a/b.txt")
        );
        // Not-yet-existing paths are fine, even deep ones.
        assert_eq!(
            e.ws.resolve("new/deep/f.txt", Access::Write).unwrap(),
            e.root.join("new/deep/f.txt")
        );
    }

    #[test]
    fn resolve_refuses_dotdot_escapes() {
        let e = env();
        for p in [
            "../x",
            "a/../../x",
            "../root/../x",
            "../../../../etc/passwd",
        ] {
            let err = e.ws.resolve(p, Access::Read).unwrap_err();
            assert!(err.contains("outside the folder"), "{p}: {err}");
            assert!(err.contains(&format!("'{p}'")), "{err}");
        }
        // `..` that stays inside is fine.
        assert!(e.ws.resolve("a/../b", Access::Write).is_ok());
    }

    #[test]
    fn resolve_refuses_absolute_paths_outside() {
        let e = env();
        assert!(e
            .ws
            .resolve("/etc/passwd", Access::Read)
            .unwrap_err()
            .contains("outside"));
        assert!(e.ws.resolve("/", Access::Read).is_err());
        // A sibling that merely shares a name prefix with the root.
        let sib = e.base.join("root-sibling");
        fs::create_dir_all(&sib).unwrap();
        assert!(e.ws.resolve(sib.to_str().unwrap(), Access::Read).is_err());
    }

    #[test]
    fn resolve_refuses_symlinks_that_point_outside() {
        let e = env();
        symlink("/etc/passwd", e.root.join("pw")).unwrap();
        symlink("/etc", e.root.join("etc-link")).unwrap();
        symlink("../trash", e.root.join("rel-out")).unwrap();
        symlink("/nonexistent-target-xyz", e.root.join("dangling")).unwrap();
        for p in ["pw", "etc-link", "etc-link/passwd", "rel-out/x", "dangling"] {
            for access in [Access::Read, Access::Write] {
                assert!(e.ws.resolve(p, access).is_err(), "{p}");
            }
        }
        // And the tools built on it.
        let mut st = TaskState::new();
        assert!(e.ws.read_file("pw", None, None, &mut st).is_err());
        assert!(e.ws.list_dir("etc-link", None).is_err());
        assert!(e
            .ws
            .apply(
                &Tool::WriteFile {
                    path: "etc-link/x".into(),
                    content: "x".into(),
                    append: false,
                },
                &mut st
            )
            .is_err());
    }

    #[test]
    fn resolve_allows_symlinks_that_stay_inside() {
        let e = env();
        put(&e, "real/f.txt", "x");
        symlink("real", e.root.join("alias")).unwrap();
        assert_eq!(
            e.ws.resolve("alias/f.txt", Access::Read).unwrap(),
            e.root.join("real/f.txt")
        );
    }

    #[test]
    fn grants_respect_read_only_and_writable() {
        let mut ro_dir = PathBuf::new();
        let mut rw_dir = PathBuf::new();
        let e = env_with(|base| {
            ro_dir = base.join("ro");
            rw_dir = base.join("rw");
            fs::create_dir_all(&ro_dir).unwrap();
            fs::create_dir_all(&rw_dir).unwrap();
            vec![
                Grant {
                    path: ro_dir.clone(),
                    writable: false,
                    recursive: true,
                },
                Grant {
                    path: rw_dir.clone(),
                    writable: true,
                    recursive: true,
                },
            ]
        });
        fs::write(ro_dir.join("r.txt"), "ro text").unwrap();
        let ro_file = ro_dir.join("r.txt");
        assert!(e
            .ws
            .resolve(ro_file.to_str().unwrap(), Access::Read)
            .is_ok());
        let err =
            e.ws.resolve(ro_file.to_str().unwrap(), Access::Write)
                .unwrap_err();
        assert!(err.contains("read-only"), "{err}");
        assert!(e
            .ws
            .resolve(rw_dir.join("new.txt").to_str().unwrap(), Access::Write)
            .is_ok());

        let mut st = TaskState::new();
        let w = e.ws.apply(
            &Tool::WriteFile {
                path: ro_file.to_str().unwrap().into(),
                content: "x".into(),
                append: false,
            },
            &mut st,
        );
        assert!(w.unwrap_err().contains("read-only"));
        assert_eq!(fs::read_to_string(&ro_file).unwrap(), "ro text");
        let out =
            e.ws.read_file(ro_file.to_str().unwrap(), None, None, &mut st)
                .unwrap();
        assert!(out.contains("ro text"));
        // Writable grant: write works and is backed up under its absolute path.
        let target = rw_dir.join("g.txt");
        fs::write(&target, "v1").unwrap();
        e.ws.read_file(target.to_str().unwrap(), None, None, &mut st)
            .unwrap();
        e.ws.apply(
            &Tool::WriteFile {
                path: target.to_str().unwrap().into(),
                content: "v2".into(),
                append: false,
            },
            &mut st,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "v2");
        assert!(trashed(&e).iter().any(|(_, t)| t == "v1"));
    }

    #[test]
    fn read_only_grant_nested_in_root_wins_over_root() {
        let mut vendor = PathBuf::new();
        let e = env_with(|base| {
            vendor = base.join("root/vendor");
            fs::create_dir_all(&vendor).unwrap();
            vec![Grant {
                path: vendor.clone(),
                writable: false,
                recursive: true,
            }]
        });
        assert!(e.ws.resolve("vendor/lib.txt", Access::Read).is_ok());
        assert!(e
            .ws
            .resolve("vendor/lib.txt", Access::Write)
            .unwrap_err()
            .contains("read-only"));
        assert!(e.ws.resolve("other.txt", Access::Write).is_ok());
    }

    #[test]
    fn non_recursive_grant_exposes_only_direct_files() {
        let mut g = PathBuf::new();
        let e = env_with(|base| {
            g = base.join("flat");
            fs::create_dir_all(g.join("sub")).unwrap();
            vec![Grant {
                path: g.clone(),
                writable: false,
                recursive: false,
            }]
        });
        assert!(e
            .ws
            .resolve(g.join("a.txt").to_str().unwrap(), Access::Read)
            .is_ok());
        assert!(e
            .ws
            .resolve(g.join("sub/b.txt").to_str().unwrap(), Access::Read)
            .is_err());
    }

    #[test]
    fn app_owned_dirs_are_refused() {
        let mut e = env();
        let app = e.root.join("appdata");
        fs::create_dir_all(&app).unwrap();
        e.ws.protected.push(app.clone());
        let err = e.ws.resolve("appdata/x", Access::Read).unwrap_err();
        assert!(err.contains("managed by the app"), "{err}");
        assert!(e.ws.resolve("appdata", Access::Write).is_err());
        // The trash dir itself, wherever it lives, is protected too.
        fs::create_dir_all(&e.trash).unwrap();
        let ws2 = Workspace::from_parts(&e.base, Vec::new(), e.trash.clone()).unwrap();
        assert!(ws2
            .resolve("trash/x", Access::Read)
            .unwrap_err()
            .contains("managed by the app"));
        // Listings skip it.
        let listing = ws2.list_dir(".", None).unwrap();
        assert!(!listing.contains("trash"), "{listing}");
        let found = ws2.find_files("*", ".").unwrap();
        assert!(!found.contains("trash"), "{found}");
    }

    #[test]
    fn display_paths_are_relative_to_the_root() {
        let e = env();
        assert_eq!(e.ws.display(&e.root.join("a/b")), "a/b");
        assert_eq!(e.ws.display(&e.root), ".");
        assert_eq!(e.ws.display(Path::new("/elsewhere/x")), "/elsewhere/x");
    }

    // ---- list_dir ---------------------------------------------------------

    #[test]
    fn list_dir_sorted_with_sizes_slashes_and_links() {
        let e = env();
        put(&e, "b.txt", "12345");
        put(&e, "A.md", "");
        put(&e, "sub/inner.txt", "x");
        symlink("b.txt", e.root.join("ln")).unwrap();
        let out = e.ws.list_dir(".", None).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "./");
        assert_eq!(
            &lines[1..],
            ["A.md  (0 B)", "b.txt  (5 B)", "ln -> b.txt", "sub/"]
        );
        // Depth 2 shows children indented.
        let out = e.ws.list_dir("", Some(2)).unwrap();
        assert!(out.contains("sub/\n  inner.txt  (1 B)"), "{out}");
        assert_eq!(
            e.ws.list_dir("sub", None).unwrap(),
            "sub/\ninner.txt  (1 B)"
        );
    }

    #[test]
    fn list_dir_depth_is_capped_and_skip_dirs_are_listed_not_entered() {
        let e = env();
        put(&e, "a/b/c/d/deep.txt", "x");
        put(&e, "node_modules/pkg/index.js", "x");
        put(&e, ".git/HEAD", "x");
        let out = e.ws.list_dir(".", Some(99)).unwrap();
        assert!(out.contains("c/"), "{out}");
        assert!(!out.contains("d/"), "depth 3 max: {out}");
        assert!(out.contains("node_modules/ (not expanded)"), "{out}");
        assert!(out.contains(".git/ (not expanded)"), "{out}");
        assert!(!out.contains("index.js") && !out.contains("HEAD"), "{out}");
    }

    #[test]
    fn list_dir_caps_entries_and_reports_the_rest() {
        let e = env();
        for i in 0..320 {
            put(&e, &format!("f{i:03}.txt"), "");
        }
        let out = e.ws.list_dir(".", None).unwrap();
        assert_eq!(out.lines().filter(|l| l.starts_with('f')).count(), 300);
        assert!(out.contains("... 20 more entries not shown"), "{out}");
    }

    #[test]
    fn list_dir_errors_and_empty() {
        let e = env();
        put(&e, "f.txt", "x");
        assert!(e
            .ws
            .list_dir("f.txt", None)
            .unwrap_err()
            .contains("use read_file"));
        assert!(e
            .ws
            .list_dir("missing", None)
            .unwrap_err()
            .contains("does not exist"));
        fs::create_dir(e.root.join("empty")).unwrap();
        assert_eq!(e.ws.list_dir("empty", None).unwrap(), "empty/ (empty)");
    }

    // ---- read_file --------------------------------------------------------

    #[test]
    fn read_file_numbers_lines_and_marks_read() {
        let e = env();
        put(&e, "n.txt", "alpha\nbeta\ngamma\n");
        let mut st = TaskState::new();
        let out = e.ws.read_file("n.txt", None, None, &mut st).unwrap();
        assert_eq!(
            out,
            "n.txt (lines 1-3 of 3)\n     1\talpha\n     2\tbeta\n     3\tgamma"
        );
        assert!(st.has_read(&e.root.join("n.txt")));
    }

    #[test]
    fn read_file_paginates_with_a_footer() {
        let e = env();
        let text: String = (1..=500).map(|i| format!("line{i}\n")).collect();
        put(&e, "big.txt", &text);
        let mut st = TaskState::new();
        let out = e.ws.read_file("big.txt", None, None, &mut st).unwrap();
        assert!(
            out.starts_with("big.txt (lines 1-200 of 500)\n"),
            "{}",
            &out[..40]
        );
        assert!(out.ends_with("[more: use offset=201]"));
        let out =
            e.ws.read_file("big.txt", Some(201), Some(5), &mut st)
                .unwrap();
        assert!(out.starts_with("big.txt (lines 201-205 of 500)\n"));
        assert!(out.contains("   201\tline201") && out.ends_with("[more: use offset=206]"));
        let out =
            e.ws.read_file("big.txt", Some(499), Some(100), &mut st)
                .unwrap();
        assert!(out.contains("lines 499-500 of 500") && !out.contains("[more"));
        // limit is capped at 1000, offset 0 is treated as 1.
        let out =
            e.ws.read_file("big.txt", Some(0), Some(5000), &mut st)
                .unwrap();
        assert!(out.contains("lines 1-500 of 500"));
        let err =
            e.ws.read_file("big.txt", Some(900), None, &mut st)
                .unwrap_err();
        assert!(
            err.contains("past the end") && err.contains("500 lines"),
            "{err}"
        );
    }

    #[test]
    fn read_file_handles_long_lines_crlf_and_no_trailing_newline() {
        let e = env();
        put(
            &e,
            "w.txt",
            &format!("{}\r\nshort\r\nlast", "x".repeat(800)),
        );
        let mut st = TaskState::new();
        let out = read(&e, &mut st, "w.txt");
        let first = out.lines().nth(1).unwrap();
        assert!(
            first.ends_with('…') && first.chars().count() < 520,
            "{}",
            first.len()
        );
        assert!(out.contains("     2\tshort\n") && out.contains("     3\tlast"));
        assert!(!out.contains('\r'));
    }

    #[test]
    fn read_file_reports_binary_empty_and_directories() {
        let e = env();
        fs::write(e.root.join("b.bin"), [1u8, 2, 0, 3, 4]).unwrap();
        let mut st = TaskState::new();
        assert_eq!(read(&e, &mut st, "b.bin"), "b.bin: binary file, 5 bytes");
        assert!(!st.has_read(&e.root.join("b.bin")));
        put(&e, "e.txt", "");
        assert_eq!(read(&e, &mut st, "e.txt"), "e.txt (empty file)");
        fs::create_dir(e.root.join("d")).unwrap();
        assert!(e
            .ws
            .read_file("d", None, None, &mut st)
            .unwrap_err()
            .contains("list_dir"));
        assert!(e
            .ws
            .read_file("nope", None, None, &mut st)
            .unwrap_err()
            .contains("does not exist"));
    }

    #[test]
    fn read_file_rejects_huge_files_without_offset() {
        let e = env();
        let f = fs::File::create(e.root.join("huge.txt")).unwrap();
        f.set_len(MAX_FILE_BYTES + 1).unwrap();
        drop(f);
        let mut st = TaskState::new();
        let err = e.ws.read_file("huge.txt", None, None, &mut st).unwrap_err();
        assert!(
            err.contains("over 10 MB") && err.contains("offset"),
            "{err}"
        );
        // A sparse file is all NULs, so with an offset it is reported binary
        // rather than read; the point is that it is not rejected or slurped.
        assert!(e.ws.read_file("huge.txt", Some(1), None, &mut st).is_ok());
    }

    // ---- search / find ----------------------------------------------------

    #[test]
    fn search_plain_is_case_insensitive_and_literal() {
        let e = env();
        put(&e, "a.txt", "Hello World\nnothing\nhello again (x+y)\n");
        put(&e, "sub/b.txt", "HELLO\n");
        let out = e.ws.search("hello", ".", None, false, false).unwrap();
        assert!(out.contains("a.txt:1: Hello World"), "{out}");
        assert!(out.contains("a.txt:3: hello again (x+y)"));
        assert!(out.contains("sub/b.txt:1: HELLO"));
        assert!(out.ends_with("3 matches in 2 files"), "{out}");
        // Regex metacharacters are literal in plain mode.
        let out = e.ws.search("(x+y)", ".", None, false, false).unwrap();
        assert!(out.contains("a.txt:3"), "{out}");
        let out = e.ws.search("hello", ".", None, false, true).unwrap();
        assert!(
            out.ends_with("1 matches in 1 files") && out.contains("hello again"),
            "{out}"
        );
    }

    #[test]
    fn search_regex_glob_and_errors() {
        let e = env();
        put(&e, "a.rs", "fn main() {}\nfn helper() {}\n");
        put(&e, "b.txt", "fn not rust\n");
        put(&e, "src/deep/c.rs", "fn deep() {}\n");
        let out =
            e.ws.search(r"fn \w+\(\)", ".", Some("*.rs"), true, false)
                .unwrap();
        assert!(
            out.contains("a.rs:1") && out.contains("a.rs:2") && out.contains("src/deep/c.rs:1")
        );
        assert!(!out.contains("b.txt"));
        let out =
            e.ws.search("fn", ".", Some("src/**/*.rs"), false, false)
                .unwrap();
        assert!(
            out.contains("src/deep/c.rs") && !out.contains("a.rs:"),
            "{out}"
        );
        assert!(e
            .ws
            .search("(", ".", None, true, false)
            .unwrap_err()
            .contains("invalid regex"));
        assert!(e.ws.search("", ".", None, false, false).is_err());
        assert!(e
            .ws
            .search("zzz", ".", None, false, false)
            .unwrap()
            .starts_with("no matches"));
        // A single file works as the path.
        let out = e.ws.search("helper", "a.rs", None, false, false).unwrap();
        assert!(out.contains("a.rs:2"));
    }

    #[test]
    fn search_skips_binaries_symlinks_and_skip_dirs() {
        let e = env();
        fs::write(e.root.join("bin.dat"), b"needle\0binary").unwrap();
        put(&e, "node_modules/x.js", "needle\n");
        put(&e, "target/x.txt", "needle\n");
        put(&e, "ok.txt", "needle\n");
        symlink("ok.txt", e.root.join("link.txt")).unwrap();
        let out = e.ws.search("needle", ".", None, false, false).unwrap();
        assert!(
            out.contains("ok.txt:1") && out.ends_with("1 matches in 1 files"),
            "{out}"
        );
    }

    #[test]
    fn search_caps_matches_and_line_length() {
        let e = env();
        let text: String = (0..250)
            .map(|i| format!("hit {i} {}\n", "y".repeat(400)))
            .collect();
        put(&e, "many.txt", &text);
        let out = e.ws.search("hit", ".", None, false, false).unwrap();
        assert_eq!(
            out.lines().filter(|l| l.starts_with("many.txt:")).count(),
            100
        );
        assert!(
            out.contains("250 matches in 1 files; showing the first 100"),
            "{out}"
        );
        let line = out.lines().next().unwrap();
        assert!(
            line.chars().count() < 230 && line.ends_with('…'),
            "{}",
            line.chars().count()
        );
    }

    #[test]
    fn find_files_matches_names_and_paths() {
        let e = env();
        put(&e, "a.rs", "");
        put(&e, "b.txt", "");
        put(&e, "src/c.rs", "");
        put(&e, "src/deep/d.rs", "");
        put(&e, "node_modules/e.rs", "");
        let out = e.ws.find_files("*.rs", ".").unwrap();
        assert_eq!(out, "a.rs\nsrc/c.rs\nsrc/deep/d.rs");
        let out = e.ws.find_files("src/**/*.rs", ".").unwrap();
        assert_eq!(out, "src/c.rs\nsrc/deep/d.rs");
        let out = e.ws.find_files("src", ".").unwrap();
        assert_eq!(out, "src/");
        let out = e.ws.find_files("*.rs", "src").unwrap();
        assert_eq!(out, "src/c.rs\nsrc/deep/d.rs");
        assert!(e
            .ws
            .find_files("*.zip", ".")
            .unwrap()
            .starts_with("no files match"));
        assert!(e.ws.find_files("", ".").is_err());
    }

    #[test]
    fn find_files_caps_results() {
        let e = env();
        for i in 0..320 {
            put(&e, &format!("f{i:03}.log"), "");
        }
        let out = e.ws.find_files("*.log", ".").unwrap();
        assert_eq!(out.lines().filter(|l| l.ends_with(".log")).count(), 300);
        assert!(out.contains("... 20 more not shown"), "{out}");
    }

    #[test]
    fn glob_translation() {
        let m = |g: &str, s: &str| glob_regex(g).unwrap().is_match(s);
        assert!(m("*.rs", "a.rs") && !m("*.rs", "a.rsx") && !m("*.rs", "dir/a.rs"));
        assert!(m("?.txt", "a.txt") && !m("?.txt", "ab.txt"));
        assert!(m("**/*.rs", "a.rs") && m("**/*.rs", "x/y/a.rs"));
        assert!(m("src/**", "src/a/b") && !m("src/*", "src/a/b"));
        assert!(m("a.b", "a.b") && !m("a.b", "axb"));
        assert!(m("./x*", "xy"));
    }

    // ---- write_file -------------------------------------------------------

    #[test]
    fn write_file_creates_with_parents_and_overwrites_with_backup() {
        let e = env();
        let mut st = TaskState::new();
        let w = |p: &str, c: &str, st: &mut TaskState| {
            e.ws.apply(
                &Tool::WriteFile {
                    path: p.into(),
                    content: c.into(),
                    append: false,
                },
                st,
            )
        };
        assert_eq!(
            w("new/dir/a.txt", "one\ntwo\n", &mut st).unwrap(),
            "created new/dir/a.txt (2 lines)"
        );
        assert_eq!(get(&e, "new/dir/a.txt"), "one\ntwo\n");
        assert!(st.has_read(&e.root.join("new/dir/a.txt")));

        let msg = w("new/dir/a.txt", "three\n", &mut st).unwrap();
        assert_eq!(msg, "overwrote new/dir/a.txt (1 lines, backup kept)");
        assert_eq!(get(&e, "new/dir/a.txt"), "three\n");
        let t = trashed(&e);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, "new/dir/a.txt");
        assert_eq!(t[0].1, "one\ntwo\n");

        let kinds: Vec<_> = st.changes().iter().map(|c| c.kind.clone()).collect();
        assert_eq!(kinds, [ChangeKind::Created, ChangeKind::Modified]);
        // No temp files left next to it.
        let names: Vec<_> = fs::read_dir(e.root.join("new/dir"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn write_file_keeps_mode_and_second_backup_gets_a_suffix() {
        let e = env();
        put(&e, "run.sh", "#!/bin/sh\n");
        fs::set_permissions(e.root.join("run.sh"), fs::Permissions::from_mode(0o750)).unwrap();
        let mut st = TaskState::new();
        e.ws.read_file("run.sh", None, None, &mut st).unwrap();
        for body in ["a\n", "b\n"] {
            e.ws.apply(
                &Tool::WriteFile {
                    path: "run.sh".into(),
                    content: body.into(),
                    append: false,
                },
                &mut st,
            )
            .unwrap();
        }
        let mode = fs::metadata(e.root.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o750);
        let mut t = trashed(&e);
        t.sort();
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t.iter().any(|(n, c)| n == "run.sh" && c == "#!/bin/sh\n"));
        assert!(t.iter().any(|(n, c)| n == "run.sh.2" && c == "a\n"));
    }

    fn app(path: &str, content: &str) -> Tool {
        Tool::WriteFile {
            path: path.into(),
            content: content.into(),
            append: true,
        }
    }

    fn blank_edit(old: &str, new: &str) -> Tool {
        Tool::EditFile {
            path: "f.txt".into(),
            edits: vec![Edit {
                old_text: old.into(),
                new_text: new.into(),
                replace_all: false,
            }],
        }
    }

    #[test]
    fn append_adds_to_existing_file_with_or_without_trailing_newline() {
        let e = env();
        let mut st = TaskState::new();
        put(&e, "f.txt", "a\n");
        let p = e.ws.preview(&app("f.txt", "b\n"), &st).unwrap();
        assert_eq!(p.title, "Append to f.txt (1 line)");
        let d = p.diff.unwrap();
        assert!(
            d.contains("+b") && !d.contains("-a") && !d.contains("+a"),
            "{d}"
        );
        e.ws.apply(&app("f.txt", "b\n"), &mut st).unwrap();
        assert_eq!(get(&e, "f.txt"), "a\nb\n");
        put(&e, "g.txt", "a");
        e.ws.apply(&app("g.txt", "b\n"), &mut st).unwrap();
        assert_eq!(get(&e, "g.txt"), "a\nb\n");
    }

    #[test]
    fn append_creates_missing_file_and_needs_no_prior_read() {
        let e = env();
        let mut st = TaskState::new();
        e.ws.apply(&app("sub/new.txt", "x\n"), &mut st).unwrap();
        assert_eq!(get(&e, "sub/new.txt"), "x\n");
        put(&e, "old.txt", "keep\n");
        let mut fresh = TaskState::new();
        e.ws.apply(&app("old.txt", "more\n"), &mut fresh).unwrap();
        assert_eq!(get(&e, "old.txt"), "keep\nmore\n");
    }

    #[test]
    fn blank_old_text_edit_appends_when_sole_edit() {
        let e = env();
        put(&e, "f.txt", "a\nb");
        let mut st = TaskState::new();
        e.ws.read_file("f.txt", None, None, &mut st).unwrap();
        e.ws.apply(&blank_edit(" ", "c\n"), &mut st).unwrap();
        assert_eq!(get(&e, "f.txt"), "a\nb\nc\n");
        e.ws.apply(&blank_edit("", "d\n"), &mut st).unwrap();
        assert_eq!(get(&e, "f.txt"), "a\nb\nc\nd\n");
    }

    #[test]
    fn blank_old_text_mixed_with_other_edits_errors() {
        let e = env();
        put(&e, "f.txt", "a\nb\n");
        let mut st = TaskState::new();
        e.ws.read_file("f.txt", None, None, &mut st).unwrap();
        let tool = Tool::EditFile {
            path: "f.txt".into(),
            edits: vec![
                Edit {
                    old_text: "a".into(),
                    new_text: "A".into(),
                    replace_all: false,
                },
                Edit {
                    old_text: " ".into(),
                    new_text: "c".into(),
                    replace_all: false,
                },
            ],
        };
        assert!(e.ws.apply(&tool, &mut st).is_err());
        assert_eq!(get(&e, "f.txt"), "a\nb\n");
    }

    #[test]
    fn write_file_preview_has_diff_and_new_file_flag() {
        let e = env();
        put(&e, "f.txt", "a\nb\nc\n");
        let mut st = TaskState::new();
        e.ws.read_file("f.txt", None, None, &mut st).unwrap();
        let p =
            e.ws.preview(
                &Tool::WriteFile {
                    path: "f.txt".into(),
                    content: "a\nB\nc\n".into(),
                    append: false,
                },
                &st,
            )
            .unwrap();
        assert!(!p.new_file);
        assert_eq!(p.title, "Write f.txt (3 lines)");
        let d = p.diff.unwrap();
        assert!(
            d.contains("--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c"),
            "{d}"
        );
        let p =
            e.ws.preview(
                &Tool::WriteFile {
                    path: "n.txt".into(),
                    content: "x\n".into(),
                    append: false,
                },
                &st,
            )
            .unwrap();
        assert!(p.new_file);
        assert!(p
            .diff
            .unwrap()
            .starts_with("--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1,1 @@\n+x"));
        // Preview changes nothing.
        assert!(!e.root.join("n.txt").exists());
    }

    #[test]
    fn write_file_errors_are_caught_by_preview_too() {
        let e = env();
        fs::create_dir(e.root.join("d")).unwrap();
        let st = TaskState::new();
        for (path, needle) in [
            ("d", "is a folder"),
            ("../x", "outside the folder"),
            (".", "is a folder"),
        ] {
            let err =
                e.ws.preview(
                    &Tool::WriteFile {
                        path: path.into(),
                        content: "x".into(),
                        append: false,
                    },
                    &st,
                )
                .unwrap_err();
            assert!(err.contains(needle), "{path}: {err}");
        }
        // A file where a parent folder is needed.
        put(&e, "file", "x");
        let mut st = TaskState::new();
        let err =
            e.ws.apply(
                &Tool::WriteFile {
                    path: "file/sub.txt".into(),
                    content: "x".into(),
                    append: false,
                },
                &mut st,
            )
            .unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn write_file_refuses_to_overwrite_an_unread_file() {
        let e = env();
        put(&e, "list.txt", "milk\n");
        let mut st = TaskState::new();
        let err =
            e.ws.apply(
                &Tool::WriteFile {
                    path: "list.txt".into(),
                    content: "eggs\n".into(),
                    append: false,
                },
                &mut st,
            )
            .unwrap_err();
        assert!(err.contains("haven't read it"), "{err}");
        assert_eq!(
            fs::read_to_string(e.root.join("list.txt")).unwrap(),
            "milk\n"
        );
        // A brand-new file needs no read.
        e.ws.apply(
            &Tool::WriteFile {
                path: "new.txt".into(),
                content: "x\n".into(),
                append: false,
            },
            &mut st,
        )
        .unwrap();
    }

    #[test]
    fn write_file_identical_content_is_a_no_op() {
        let e = env();
        put(&e, "f.txt", "same\n");
        let mut st = TaskState::new();
        e.ws.read_file("f.txt", None, None, &mut st).unwrap();
        let msg =
            e.ws.apply(
                &Tool::WriteFile {
                    path: "f.txt".into(),
                    content: "same\n".into(),
                    append: false,
                },
                &mut st,
            )
            .unwrap();
        assert!(msg.contains("nothing changed"));
        assert!(st.changes().is_empty() && trashed(&e).is_empty());
    }

    // ---- edit_file --------------------------------------------------------

    #[test]
    fn edit_file_unique_match_backs_up_and_records() {
        let e = env();
        put(&e, "c.txt", "one\ntwo\nthree\n");
        let mut st = TaskState::new();
        read(&e, &mut st, "c.txt");
        let msg = do_edit(&e, &mut st, "c.txt", vec![edit("two", "TWO")]).unwrap();
        assert_eq!(msg, "edited c.txt: 1 replacement (backup kept)");
        assert_eq!(get(&e, "c.txt"), "one\nTWO\nthree\n");
        assert_eq!(
            trashed(&e),
            [("c.txt".to_string(), "one\ntwo\nthree\n".to_string())]
        );
        assert_eq!(
            st.changed_files_summary(),
            [Change {
                path: "c.txt".into(),
                kind: ChangeKind::Modified
            }]
        );
    }

    #[test]
    fn edit_file_requires_read_first_unless_created_in_this_task() {
        let e = env();
        put(&e, "old.txt", "x\n");
        let mut st = TaskState::new();
        let err = do_edit(&e, &mut st, "old.txt", vec![edit("x", "y")]).unwrap_err();
        assert!(err.contains("read the file first with read_file"), "{err}");
        let p = e.ws.preview(
            &Tool::EditFile {
                path: "old.txt".into(),
                edits: vec![edit("x", "y")],
            },
            &st,
        );
        assert!(p.unwrap_err().contains("read the file first"));
        assert_eq!(get(&e, "old.txt"), "x\n");

        e.ws.apply(
            &Tool::WriteFile {
                path: "made.txt".into(),
                content: "a\nb\n".into(),
                append: false,
            },
            &mut st,
        )
        .unwrap();
        do_edit(&e, &mut st, "made.txt", vec![edit("b", "c")]).unwrap();
        assert_eq!(get(&e, "made.txt"), "a\nc\n");

        // A fresh task state has not read anything.
        let mut st2 = TaskState::new();
        assert!(do_edit(&e, &mut st2, "made.txt", vec![edit("c", "d")]).is_err());
    }

    #[test]
    fn edit_file_multiple_matches_list_lines_unless_replace_all() {
        let e = env();
        put(&e, "m.txt", "foo\nbar\nfoo\nbaz\nfoo\n");
        let mut st = TaskState::new();
        read(&e, &mut st, "m.txt");
        let err = do_edit(&e, &mut st, "m.txt", vec![edit("foo", "X")]).unwrap_err();
        assert_eq!(
            err,
            "old_text matches 3 times (lines 1, 3, 5); include more surrounding lines or set replace_all"
        );
        assert_eq!(get(&e, "m.txt"), "foo\nbar\nfoo\nbaz\nfoo\n");
        let mut all = edit("foo", "X");
        all.replace_all = true;
        let msg = do_edit(&e, &mut st, "m.txt", vec![all]).unwrap();
        assert!(msg.contains("3 replacements"));
        assert_eq!(get(&e, "m.txt"), "X\nbar\nX\nbaz\nX\n");
        // More surrounding lines disambiguate.
        put(&e, "m2.txt", "foo\nbar\nfoo\nbaz\n");
        read(&e, &mut st, "m2.txt");
        do_edit(&e, &mut st, "m2.txt", vec![edit("foo\nbaz", "foo\nQUX")]).unwrap();
        assert_eq!(get(&e, "m2.txt"), "foo\nbar\nfoo\nQUX\n");
    }

    #[test]
    fn edit_file_no_match_shows_closest_lines() {
        let e = env();
        let text: String = (1..=40)
            .map(|i| format!("row {i}: value {}\n", i * 7))
            .collect();
        put(&e, "t.txt", &text);
        let mut st = TaskState::new();
        read(&e, &mut st, "t.txt");
        let err = do_edit(
            &e,
            &mut st,
            "t.txt",
            vec![edit("row 20: value 141\nrow 21: value 148", "x")],
        )
        .unwrap_err();
        assert!(
            err.starts_with("old_text not found in t.txt. Closest lines 20-21:"),
            "{err}"
        );
        assert!(err.contains("    20\trow 20: value 140"), "{err}");
        assert!(err.contains("    21\trow 21: value 147"), "{err}");
        assert_eq!(get(&e, "t.txt"), text);
    }

    #[test]
    fn edit_file_whitespace_tolerant_fallback() {
        let e = env();
        put(
            &e,
            "ws.py",
            "def f():\n    x = 1   \n    y = 2\n    return x + y\n",
        );
        let mut st = TaskState::new();
        read(&e, &mut st, "ws.py");
        // Wrong indentation + no trailing blanks in old_text; new_text is
        // re-indented to match the file.
        let msg = do_edit(
            &e,
            &mut st,
            "ws.py",
            vec![edit("x = 1\ny = 2", "x = 10\ny = 20\nz = 30")],
        )
        .unwrap();
        assert!(msg.contains("1 replacement"));
        assert_eq!(
            get(&e, "ws.py"),
            "def f():\n    x = 10\n    y = 20\n    z = 30\n    return x + y\n"
        );
    }

    #[test]
    fn edit_file_tolerant_match_adopts_file_indentation_for_new_text() {
        let e = env();
        put(&e, "i.txt", "{\n        a\n        b\n}\n");
        let mut st = TaskState::new();
        read(&e, &mut st, "i.txt");
        do_edit(
            &e,
            &mut st,
            "i.txt",
            vec![edit("    a\n    b\n", "    a\n    new\n    b\n")],
        )
        .unwrap();
        assert_eq!(
            get(&e, "i.txt"),
            "{\n        a\n        new\n        b\n}\n"
        );
    }

    #[test]
    fn edit_file_tolerant_match_must_still_be_unique() {
        let e = env();
        put(&e, "u.txt", "  dup\nmid\n    dup\n");
        let mut st = TaskState::new();
        read(&e, &mut st, "u.txt");
        let err = do_edit(&e, &mut st, "u.txt", vec![edit("\tdup", "x")]).unwrap_err();
        assert!(err.contains("matches 2 times (lines 1, 3)"), "{err}");
    }

    #[test]
    fn edit_file_preserves_crlf() {
        let e = env();
        put(&e, "w.txt", "a\r\nb\r\nc\r\n");
        let mut st = TaskState::new();
        read(&e, &mut st, "w.txt");
        do_edit(&e, &mut st, "w.txt", vec![edit("a\nb", "A\nB\nB2")]).unwrap();
        assert_eq!(get(&e, "w.txt"), "A\r\nB\r\nB2\r\nc\r\n");
    }

    #[test]
    fn edit_file_is_all_or_nothing() {
        let e = env();
        put(&e, "a.txt", "one\ntwo\nthree\n");
        let mut st = TaskState::new();
        read(&e, &mut st, "a.txt");
        let err = do_edit(
            &e,
            &mut st,
            "a.txt",
            vec![edit("one", "1"), edit("two", "2"), edit("missing", "3")],
        )
        .unwrap_err();
        assert!(err.starts_with("edit 3: old_text not found"), "{err}");
        assert_eq!(get(&e, "a.txt"), "one\ntwo\nthree\n");
        assert!(trashed(&e).is_empty() && st.changes().is_empty());
        // Edits apply in order: the second sees the first's output.
        do_edit(
            &e,
            &mut st,
            "a.txt",
            vec![edit("one", "uno"), edit("uno\ntwo", "UNO\nDOS")],
        )
        .unwrap();
        assert_eq!(get(&e, "a.txt"), "UNO\nDOS\nthree\n");
    }

    #[test]
    fn edit_file_validation_errors() {
        let e = env();
        put(&e, "a.txt", "text\n");
        fs::write(e.root.join("bin.dat"), [0xffu8, 0xfe, 0x00]).unwrap();
        fs::create_dir(e.root.join("d")).unwrap();
        let mut st = TaskState::new();
        read(&e, &mut st, "a.txt");
        st.read_files.insert(e.root.join("bin.dat"));
        let t =
            |p: &str, edits: Vec<Edit>, st: &mut TaskState| do_edit(&e, st, p, edits).unwrap_err();
        assert!(t("a.txt", vec![edit("", "")], &mut st).contains("old_text is empty"));
        assert!(t("a.txt", vec![], &mut st).contains("edits is empty"));
        assert!(t("a.txt", vec![edit("text", "text")], &mut st).contains("would not change"));
        assert!(t("nope.txt", vec![edit("a", "b")], &mut st).contains("use write_file"));
        assert!(t("d", vec![edit("a", "b")], &mut st).contains("is a folder"));
        assert!(t("bin.dat", vec![edit("a", "b")], &mut st).contains("not a UTF-8"));
        assert!(t("../x", vec![edit("a", "b")], &mut st).contains("outside the folder"));
        // Two-edit errors name the failing edit.
        assert!(
            t("a.txt", vec![edit("text", "ok"), edit("", "x")], &mut st).starts_with("edit 2: ")
        );
    }

    #[test]
    fn edit_file_preview_shows_the_diff() {
        let e = env();
        put(&e, "p.txt", "a\nb\nc\n");
        let mut st = TaskState::new();
        read(&e, &mut st, "p.txt");
        let tool = Tool::EditFile {
            path: "p.txt".into(),
            edits: vec![edit("b", "B")],
        };
        let p = e.ws.preview(&tool, &st).unwrap();
        assert_eq!(p.title, "Edit p.txt (1 edit)");
        assert!(!p.new_file);
        assert!(p.diff.unwrap().contains("-b\n+B"));
        assert_eq!(get(&e, "p.txt"), "a\nb\nc\n");
    }

    // ---- move -------------------------------------------------------------

    fn mv(e: &Env, st: &mut TaskState, from: &str, to: &str) -> Result<String, String> {
        e.ws.apply(
            &Tool::Move {
                from: from.into(),
                to: to.into(),
            },
            st,
        )
    }

    #[test]
    fn move_renames_creates_parents_and_tracks_reads() {
        let e = env();
        put(&e, "a.txt", "content");
        let mut st = TaskState::new();
        read(&e, &mut st, "a.txt");
        assert_eq!(
            mv(&e, &mut st, "a.txt", "new/sub/b.txt").unwrap(),
            "moved a.txt -> new/sub/b.txt"
        );
        assert!(!e.root.join("a.txt").exists());
        assert_eq!(get(&e, "new/sub/b.txt"), "content");
        assert!(st.has_read(&e.root.join("new/sub/b.txt")) && !st.has_read(&e.root.join("a.txt")));
        assert_eq!(
            st.changed_files_summary(),
            [Change {
                path: "new/sub/b.txt".into(),
                kind: ChangeKind::Moved {
                    from: "a.txt".into()
                }
            }]
        );
        // The moved file can be edited without another read.
        do_edit(
            &e,
            &mut st,
            "new/sub/b.txt",
            vec![edit("content", "changed")],
        )
        .unwrap();
    }

    #[test]
    fn move_into_directory_and_destination_exists() {
        let e = env();
        put(&e, "a.txt", "A");
        put(&e, "b.txt", "B");
        put(&e, "dir/b.txt", "old");
        fs::create_dir_all(e.root.join("empty")).unwrap();
        let mut st = TaskState::new();
        let err = mv(&e, &mut st, "a.txt", "b.txt").unwrap_err();
        assert!(err.contains("destination 'b.txt' already exists"), "{err}");
        // Into a directory: the item lands inside, and must not collide.
        let err = mv(&e, &mut st, "b.txt", "dir").unwrap_err();
        assert!(err.contains("'dir/b.txt' already exists"), "{err}");
        assert_eq!(get(&e, "b.txt"), "B");
        assert_eq!(get(&e, "dir/b.txt"), "old");
        mv(&e, &mut st, "a.txt", "empty").unwrap();
        assert_eq!(get(&e, "empty/a.txt"), "A");
        // Trailing slash on a missing destination means "folder".
        mv(&e, &mut st, "b.txt", "fresh/").unwrap();
        assert_eq!(get(&e, "fresh/b.txt"), "B");
        // Nothing was trashed: moves never overwrite.
        assert!(trashed(&e).is_empty());
    }

    #[test]
    fn move_directories_and_refuses_nonsense() {
        let e = env();
        put(&e, "d/x.txt", "x");
        let mut st = TaskState::new();
        mv(&e, &mut st, "d", "renamed").unwrap();
        assert_eq!(get(&e, "renamed/x.txt"), "x");
        assert!(mv(&e, &mut st, "renamed", "renamed/inside")
            .unwrap_err()
            .contains("into itself"));
        put(&e, "solo.txt", "s");
        assert!(mv(&e, &mut st, "solo.txt", "solo.txt")
            .unwrap_err()
            .contains("already there"));
        assert!(mv(&e, &mut st, "missing", "x")
            .unwrap_err()
            .contains("does not exist"));
        assert!(mv(&e, &mut st, ".", "x")
            .unwrap_err()
            .contains("working folder itself"));
        assert!(mv(&e, &mut st, "renamed", "../out")
            .unwrap_err()
            .contains("outside the folder"));
        assert!(mv(&e, &mut st, "../x", "y")
            .unwrap_err()
            .contains("outside the folder"));
    }

    #[test]
    fn move_acts_on_a_symlink_itself() {
        let e = env();
        put(&e, "real.txt", "real");
        symlink("real.txt", e.root.join("link")).unwrap();
        let mut st = TaskState::new();
        mv(&e, &mut st, "link", "moved-link").unwrap();
        assert!(e.root.join("real.txt").exists());
        assert!(fs::symlink_metadata(e.root.join("moved-link"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(fs::symlink_metadata(e.root.join("link")).is_err());
        // An escaping link can be moved or deleted (the link, not its target)...
        symlink("/etc", e.root.join("evil")).unwrap();
        e.ws.apply(
            &Tool::Delete {
                path: "evil".into(),
            },
            &mut st,
        )
        .unwrap();
        assert!(fs::symlink_metadata(e.root.join("evil")).is_err());
        assert!(Path::new("/etc").exists());
    }

    // ---- delete / make_dir ------------------------------------------------

    #[test]
    fn delete_moves_files_and_folders_to_trash() {
        let e = env();
        put(&e, "gone.txt", "bye");
        put(&e, "dir/a.txt", "A");
        put(&e, "dir/sub/b.txt", "B");
        let mut st = TaskState::new();
        let msg =
            e.ws.apply(
                &Tool::Delete {
                    path: "gone.txt".into(),
                },
                &mut st,
            )
            .unwrap();
        assert!(msg.contains("moved to trash"), "{msg}");
        assert!(!e.root.join("gone.txt").exists());
        e.ws.apply(&Tool::Delete { path: "dir".into() }, &mut st)
            .unwrap();
        assert!(!e.root.join("dir").exists());
        let mut t = trashed(&e);
        t.sort();
        assert_eq!(
            t,
            [
                ("dir/a.txt".to_string(), "A".to_string()),
                ("dir/sub/b.txt".to_string(), "B".to_string()),
                ("gone.txt".to_string(), "bye".to_string()),
            ]
        );
        // All of it inside ONE per-task run dir.
        assert_eq!(fs::read_dir(&e.trash).unwrap().count(), 1);
        assert_eq!(st.trash_run().map(|p| p.starts_with(&e.trash)), Some(true));
        let sum = st.changed_files_summary();
        assert_eq!(sum.len(), 2);
        assert!(sum.iter().all(|c| c.kind == ChangeKind::Deleted));
    }

    #[test]
    fn delete_errors() {
        let e = env();
        let mut st = TaskState::new();
        let d = |p: &str, st: &mut TaskState| {
            e.ws.apply(&Tool::Delete { path: p.into() }, st)
                .unwrap_err()
        };
        assert!(d("missing", &mut st).contains("does not exist"));
        assert!(d(".", &mut st).contains("working folder itself"));
        assert!(d("", &mut st).contains("working folder itself"));
        assert!(d("../x", &mut st).contains("outside the folder"));
        assert!(d("/etc/hostname", &mut st).contains("outside the folder"));
        assert!(trashed(&e).is_empty() && !e.trash.exists());
    }

    #[test]
    fn make_dir_is_idempotent_and_needs_no_confirmation() {
        let e = env();
        let mut st = TaskState::new();
        let tool = Tool::MakeDir {
            path: "a/b/c".into(),
        };
        assert!(!needs_confirmation(&tool) && !is_read_only(&tool));
        assert_eq!(e.ws.apply(&tool, &mut st).unwrap(), "created folder a/b/c");
        assert!(e.root.join("a/b/c").is_dir());
        assert_eq!(e.ws.apply(&tool, &mut st).unwrap(), "a/b/c already exists");
        assert_eq!(st.changes().len(), 1);
        put(&e, "file", "x");
        let err =
            e.ws.apply(
                &Tool::MakeDir {
                    path: "file".into(),
                },
                &mut st,
            )
            .unwrap_err();
        assert!(err.contains("already exists and is a file"));
        assert!(e
            .ws
            .apply(
                &Tool::MakeDir {
                    path: "../x".into()
                },
                &mut st
            )
            .is_err());
    }

    #[test]
    fn dispatch_helpers_classify_tools() {
        assert!(is_read_only(&Tool::ListDir {
            path: ".".into(),
            depth: None
        }));
        assert!(!is_read_only(&Tool::RunCommand {
            command: "ls".into()
        }));
        for t in [
            Tool::WriteFile {
                path: "a".into(),
                content: String::new(),
                append: false,
            },
            Tool::EditFile {
                path: "a".into(),
                edits: vec![],
            },
            Tool::Move {
                from: "a".into(),
                to: "b".into(),
            },
            Tool::Delete { path: "a".into() },
        ] {
            assert!(needs_confirmation(&t), "{}", t.name());
        }
        let e = env();
        let mut st = TaskState::new();
        assert!(e
            .ws
            .run_readonly(&Tool::Delete { path: "x".into() }, &mut st)
            .is_err());
        assert!(e
            .ws
            .apply(
                &Tool::Finish {
                    summary: String::new()
                },
                &mut st
            )
            .is_err());
        assert!(e
            .ws
            .preview(
                &Tool::ListDir {
                    path: ".".into(),
                    depth: None
                },
                &st
            )
            .is_err());
        put(&e, "z.txt", "z");
        assert!(e
            .ws
            .execute(
                &Tool::ReadFile {
                    path: "z.txt".into(),
                    offset: None,
                    limit: None
                },
                &mut st
            )
            .is_ok());
        assert!(e
            .ws
            .execute(&Tool::MakeDir { path: "q".into() }, &mut st)
            .is_ok());
    }

    // ---- change summary ---------------------------------------------------

    #[test]
    fn changed_files_summary_merges_per_path() {
        let mut st = TaskState::new();
        st.record("a".into(), ChangeKind::Created);
        st.record("a".into(), ChangeKind::Modified);
        st.record("b".into(), ChangeKind::Modified);
        st.record("b".into(), ChangeKind::Modified);
        st.record("c".into(), ChangeKind::Created);
        st.record("c".into(), ChangeKind::Deleted);
        st.record("d".into(), ChangeKind::Modified);
        st.record("d".into(), ChangeKind::Deleted);
        st.record("e".into(), ChangeKind::Deleted);
        st.record("e".into(), ChangeKind::Created);
        st.record("f".into(), ChangeKind::DirCreated);
        let sum = st.changed_files_summary();
        let got: Vec<(String, &str)> = sum
            .iter()
            .map(|c| (c.path.clone(), c.kind.label()))
            .collect();
        assert_eq!(
            got,
            [
                ("a".to_string(), "created"),
                ("b".to_string(), "modified"),
                ("d".to_string(), "deleted"),
                ("e".to_string(), "modified"),
                ("f".to_string(), "folder created"),
            ]
        );
    }

    // ---- diff -------------------------------------------------------------

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn diff_has_context_hunks_and_correct_ranges() {
        let old = numbered(30);
        let new = old.replace("line 5\n", "LINE 5\n").replace("line 25\n", "");
        let d = unified_diff("f.txt", Some(&old), &new);
        let lines: Vec<&str> = d.lines().collect();
        assert_eq!(lines[0], "--- a/f.txt");
        assert_eq!(lines[1], "+++ b/f.txt");
        assert_eq!(lines[2], "@@ -2,7 +2,7 @@");
        assert_eq!(&lines[3..6], [" line 2", " line 3", " line 4"]);
        assert_eq!(&lines[6..8], ["-line 5", "+LINE 5"]);
        // Distant change: its own hunk with 3 lines of context each side.
        let second = lines
            .iter()
            .position(|l| l.starts_with("@@ -22"))
            .expect(&d);
        assert_eq!(lines[second], "@@ -22,7 +22,6 @@");
        assert_eq!(lines[second + 4], "-line 25");
        assert_eq!(lines.iter().filter(|l| l.starts_with("@@")).count(), 2);
    }

    #[test]
    fn diff_merges_close_changes_and_handles_edges() {
        let old = numbered(20);
        let new = old.replace("line 5\n", "x\n").replace("line 9\n", "y\n");
        let d = unified_diff("f", Some(&old), &new);
        assert_eq!(d.lines().filter(|l| l.starts_with("@@")).count(), 1, "{d}");
        // Insert at the start and the end.
        let d = unified_diff("f", Some("b\n"), "a\nb\nc\n");
        assert!(d.contains("@@ -1,1 +1,3 @@\n+a\n b\n+c"), "{d}");
        // Delete everything.
        let d = unified_diff("f", Some("a\nb\n"), "");
        assert!(d.contains("@@ -1,2 +0,0 @@\n-a\n-b"), "{d}");
        assert_eq!(unified_diff("f", Some("a\n"), "a\n"), "(no changes)");
        assert_eq!(
            unified_diff("f", Some("a"), "a\n"),
            "(only the trailing newline changes)"
        );
    }

    #[test]
    fn diff_is_correct_for_a_shuffled_file() {
        // Apply the produced ops back and check we get `new`.
        let a: Vec<&str> = "a b c d e f g h".split(' ').collect();
        let b: Vec<&str> = "a c d x y f h g".split(' ').collect();
        let ops = diff_ops(&a, &b);
        let old: Vec<&str> = ops
            .iter()
            .filter(|(o, _)| *o != Op::Ins)
            .map(|(_, l)| *l)
            .collect();
        let new: Vec<&str> = ops
            .iter()
            .filter(|(o, _)| *o != Op::Del)
            .map(|(_, l)| *l)
            .collect();
        assert_eq!(old, a);
        assert_eq!(new, b);
    }

    #[test]
    fn diff_caps_lines_and_summarizes_huge_files() {
        let d = unified_diff("big", None, &numbered(1000));
        assert_eq!(d.lines().count(), MAX_DIFF_LINES + 1);
        assert!(
            d.lines()
                .last()
                .unwrap()
                .starts_with("... (diff truncated, "),
            "{}",
            d.lines().last().unwrap()
        );
        let d = unified_diff("huge", Some(&numbered(6000)), &numbered(6001));
        assert_eq!(
            d,
            "(too large for a diff preview: 6000 lines -> 6001 lines)"
        );
        // A total rewrite of a mid-size file still finishes (fallback path).
        let a: String = (0..3000).map(|i| format!("a{i}\n")).collect();
        let b: String = (0..3000).map(|i| format!("b{i}\n")).collect();
        let d = unified_diff("rw", Some(&a), &b);
        assert!(d.contains("-a0\n") || d.contains("-a1\n"));
    }

    // ---- special files and protected containers ---------------------------

    fn mkfifo(path: &Path) {
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    #[test]
    fn fifos_never_block_and_are_not_files() {
        let e = env();
        put(&e, "real.txt", "needle");
        mkfifo(&e.root.join("pipe"));
        let mut st = TaskState::new();
        let listing = e.ws.list_dir(".", None).unwrap();
        assert!(
            listing.contains("real.txt") && !listing.contains("pipe"),
            "{listing}"
        );
        assert!(e
            .ws
            .read_file("pipe", None, None, &mut st)
            .unwrap_err()
            .contains("not a regular file"));
        // Search of the folder and of the FIFO itself both return.
        assert!(e
            .ws
            .search("needle", ".", None, false, false)
            .unwrap()
            .contains("real.txt:1"));
        assert!(e
            .ws
            .search("x", "pipe", None, false, false)
            .unwrap()
            .contains("no matches"));
        let w = e.ws.apply(
            &Tool::WriteFile {
                path: "pipe".into(),
                content: "x".into(),
                append: false,
            },
            &mut st,
        );
        assert!(w.unwrap_err().contains("not a regular file"));
        st.read_files.insert(e.root.join("pipe"));
        assert!(do_edit(&e, &mut st, "pipe", vec![edit("a", "b")])
            .unwrap_err()
            .contains("not a regular file"));
        assert!(open_regular(&e.root.join("pipe")).is_err());
    }

    #[test]
    fn move_and_delete_refuse_a_parent_of_a_read_only_grant() {
        let e = env_with(|base| {
            let docs = base.join("root/parent/docs");
            fs::create_dir_all(&docs).unwrap();
            vec![Grant {
                path: docs,
                writable: false,
                recursive: true,
            }]
        });
        put(&e, "parent/docs/a.txt", "A");
        let mut st = TaskState::new();
        let err = mv(&e, &mut st, "parent", "elsewhere").unwrap_err();
        assert!(err.contains("read-only folder"), "{err}");
        let err =
            e.ws.apply(
                &Tool::Delete {
                    path: "parent".into(),
                },
                &mut st,
            )
            .unwrap_err();
        assert!(err.contains("read-only folder"), "{err}");
        assert_eq!(get(&e, "parent/docs/a.txt"), "A");
        // The grant itself and unrelated siblings behave as before.
        put(&e, "parent/other.txt", "o");
        mv(&e, &mut st, "parent/other.txt", "moved.txt").unwrap();
    }

    #[test]
    fn move_refuses_a_dir_holding_the_app_config_dir() {
        let mut home = PathBuf::new();
        let mut e = env_with(|base| {
            home = base.join("home");
            fs::create_dir_all(home.join(".config/app")).unwrap();
            vec![Grant {
                path: home.clone(),
                writable: true,
                recursive: true,
            }]
        });
        e.ws.protected.push(home.join(".config/app"));
        let mut st = TaskState::new();
        let from = home.join(".config");
        let to = home.join("cfg2");
        let err = mv(&e, &mut st, from.to_str().unwrap(), to.to_str().unwrap()).unwrap_err();
        assert!(err.contains("app data"), "{err}");
        let err =
            e.ws.apply(
                &Tool::Delete {
                    path: from.to_str().unwrap().into(),
                },
                &mut st,
            )
            .unwrap_err();
        assert!(err.contains("app data"), "{err}");
        assert!(from.join("app").is_dir());
    }
}
