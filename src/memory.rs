//! Per-session working record, injected into the system block each turn and
//! living *outside* the trimmed history so it survives `context.rs`.
//!
//! Solves a chain losing track of itself within one session: once early turns
//! are trimmed, the model can't tell whether the `mkdir` it proposed twenty
//! steps ago ever ran.
//!
//! **The app writes everything injected; the model writes only `temp/`.**
//! That split is the whole design -- a model writing its own progress notes
//! is the one already caught reporting a reorganization that never happened,
//! and in a file that lie would outlive the transcript contradicting it.
//!
//! - `intent.md` -- the user's messages verbatim, not a paraphrase.
//! - `original-state.md` -- folder listing read by the app at task start.
//! - `progress.md` -- commands that ran, with real exit codes, plus denials.
//! - `completed.md` -- previous tasks' ledgers.

use crate::config::AppConfig;
use crate::context::estimate_tokens;
use crate::paths::app_config_dir;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Oldest deleted first, same as `chat_log::MAX_LOG_FILES`.
const MAX_SESSIONS: usize = 5;

/// Fixed at startup so every command invocation writes to the same session.
static CURRENT_SESSION: OnceLock<PathBuf> = OnceLock::new();

const INTENT: &str = "intent.md";
const ORIGINAL_STATE: &str = "original-state.md";
const PROGRESS: &str = "progress.md";
const COMPLETED: &str = "completed.md";
const TEMP_DIR: &str = "temp";

/// On-disk cap, separate from the injected block's token cap.
const MAX_ENTRIES: usize = 200;

fn memory_root() -> PathBuf {
    app_config_dir().join("memory")
}

fn is_session_dir(path: &Path) -> bool {
    path.is_dir()
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("session-"))
            .unwrap_or(false)
}

/// Timestamp-suffixed, so lexicographic order is chronological.
pub fn init() -> anyhow::Result<PathBuf> {
    let root = memory_root();
    fs::create_dir_all(&root)?;

    let mut existing: Vec<PathBuf> = fs::read_dir(&root)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_session_dir(p))
        .collect();
    existing.sort();
    while existing.len() >= MAX_SESSIONS {
        let oldest = existing.remove(0);
        let _ = fs::remove_dir_all(&oldest);
    }

    let dir = root.join(format!(
        "session-{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));
    fs::create_dir_all(dir.join(TEMP_DIR))?;
    let _ = CURRENT_SESSION.set(dir.clone());
    Ok(dir)
}

// Thread-local because neither the global `OnceLock` nor `XDG_CONFIG_HOME`
// can isolate tests running in parallel; the harness gives each its own thread.
#[cfg(test)]
thread_local! {
    static TEST_SESSION_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Falls back to a shared directory if `init()` never ran.
pub fn session_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = TEST_SESSION_DIR.with(|d| d.borrow().clone()) {
        return dir;
    }
    if let Some(dir) = CURRENT_SESSION.get() {
        return dir.clone();
    }
    let dir = memory_root().join("session-current");
    let _ = fs::create_dir_all(dir.join(TEMP_DIR));
    dir
}

/// The only part the model can write, bound rw into the sandbox. A
/// subdirectory so the rest stays unreachable -- `progress.md` is trustworthy
/// only because nothing else can write to it.
pub fn temp_dir() -> PathBuf {
    session_dir().join(TEMP_DIR)
}

fn read_entries(name: &str) -> Vec<String> {
    fs::read_to_string(session_dir().join(name))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

fn append_entry(name: &str, line: &str) {
    let path = session_dir().join(name);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    // One entry per line, so reading/capping/archiving is line counting.
    let flat = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return;
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{flat}");
    }
    let entries = read_entries(name);
    if entries.len() > MAX_ENTRIES {
        let kept = entries[entries.len() - MAX_ENTRIES..].join("\n");
        let _ = fs::write(&path, format!("{kept}\n"));
    }
}

fn write_entries(name: &str, entries: &[String]) {
    let path = session_dir().join(name);
    let _ = fs::write(path, format!("{}\n", entries.join("\n")));
}

/// Archives the last task, clears scratch, re-snapshots the folder. Triggered
/// by the user speaking, not by the model declaring itself done -- that
/// judgment is the one this record exists to not depend on.
pub fn start_task(root: Option<&Path>, message: &str) {
    archive_progress();
    clear_temp();
    append_entry(INTENT, &format!("- {message}"));
    snapshot_state(root);
}

fn archive_progress() {
    let progress = read_entries(PROGRESS);
    if progress.is_empty() {
        return;
    }
    let path = session_dir().join(COMPLETED);
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        for line in &progress {
            let _ = writeln!(file, "{line}");
        }
    }
    let _ = fs::write(session_dir().join(PROGRESS), "");

    let entries = read_entries(COMPLETED);
    if entries.len() > MAX_ENTRIES {
        write_entries(COMPLETED, &entries[entries.len() - MAX_ENTRIES..]);
    }
}

fn clear_temp() {
    let temp = temp_dir();
    let _ = fs::remove_dir_all(&temp);
    let _ = fs::create_dir_all(&temp);
}

/// Read directly, not through the sandbox: a fact about the folder rather
/// than the result of a command the model could have shaped, and it costs no turn.
fn snapshot_state(root: Option<&Path>) {
    let path = session_dir().join(ORIGINAL_STATE);
    let Some(root) = root else {
        let _ = fs::write(path, "");
        return;
    };
    let Ok(entries) = fs::read_dir(root) else {
        let _ = fs::write(path, "");
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // The protocol tells the model to ignore this; listing it would undo that.
            if name == ".temp-trash" {
                return None;
            }
            let suffix = if e.path().is_dir() { "/" } else { "" };
            Some(format!("{name}{suffix}"))
        })
        .collect();
    names.sort();
    let _ = fs::write(
        path,
        format!("- when this task started: {}\n", names.join("  ")),
    );
}

/// Called with `run_command`'s own result, so it cannot disagree with it.
pub fn record_command(cmd: &str, exit_code: i32) {
    append_entry(PROGRESS, &format!("- ran: {cmd} -> exit {exit_code}"));
}

/// A denial, stop, or skipped step. Recorded as firmly as a success --
/// "nothing happened" is what this app is worst at holding on to.
pub fn record_blocked(cmd: &str, why: &str) {
    append_entry(PROGRESS, &format!("- NOT run ({why}): {cmd}"));
}

/// The system-block addition, or `None` when off or empty.
///
/// `memory_max_tokens` is the only thing capping this: it goes in the system
/// block, which `context.rs` never trims.
pub fn build_block(cfg: &AppConfig) -> Option<String> {
    if !cfg.memory_enabled {
        return None;
    }
    let mut intent = read_entries(INTENT);
    let state = read_entries(ORIGINAL_STATE);
    let mut progress = read_entries(PROGRESS);
    let mut completed = read_entries(COMPLETED);
    if intent.is_empty() && progress.is_empty() && completed.is_empty() {
        return None;
    }

    let budget = cfg.memory_max_tokens as usize;
    let mut cut = Cut::default();
    loop {
        let block = render(&intent, &state, &progress, &completed, &cut);
        if budget == 0 || estimate_tokens(&block) <= budget {
            return Some(block);
        }
        // Shed most-reproducible first. The snapshot and latest request are
        // the floor -- nothing else in the prompt can supply them.
        if !completed.is_empty() {
            completed.remove(0);
            cut.completed += 1;
        } else if progress.len() > 1 {
            progress.remove(0);
            cut.progress += 1;
        } else if intent.len() > 1 {
            intent.remove(0);
            cut.intent += 1;
        } else {
            // Emitting the floor and saying so beats silently exceeding the cap.
            cut.over_budget = true;
            return Some(render(&intent, &state, &progress, &completed, &cut));
        }
    }
}

/// What `build_block` had to leave out to fit, so the rendered block can say
/// so rather than quietly presenting itself as complete.
#[derive(Default)]
struct Cut {
    completed: usize,
    progress: usize,
    intent: usize,
    over_budget: bool,
}

fn render(
    intent: &[String],
    state: &[String],
    progress: &[String],
    completed: &[String],
    cut: &Cut,
) -> String {
    let mut out = String::from(
        "# Session record (kept by the app, not by you)\n\n\
         Recorded from what actually happened: the user's own words, a listing the app read \
         itself, and the real exit code of every command. You did not write this and it is not a \
         summary, so prefer it over your own recollection and never contradict it. It survives \
         trimming of the turns above.\n",
    );
    if cut.over_budget {
        out.push_str(
            "\n[this record is larger than its configured token budget even after trimming -- \
             raise the budget in Settings if it keeps being cut]\n",
        );
    }

    if !intent.is_empty() {
        out.push_str("\n## What the user asked for, in their words\n");
        if cut.intent > 0 {
            out.push_str(&format!(
                "- [{} earlier request(s) dropped from this list to save space]\n",
                cut.intent
            ));
        }
        for line in intent {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !state.is_empty() {
        out.push_str("\n## Folder contents when the current task started\n");
        for line in state {
            out.push_str(line);
            out.push('\n');
        }
    }

    out.push_str("\n## Commands run for the current task\n");
    if progress.is_empty() {
        out.push_str("- nothing has run yet for this task\n");
    } else {
        if cut.progress > 0 {
            out.push_str(&format!(
                "- [{} earlier command(s) dropped from this list to save space -- they did run]\n",
                cut.progress
            ));
        }
        for line in progress {
            out.push_str(line);
            out.push('\n');
        }
    }

    if !completed.is_empty() || cut.completed > 0 {
        out.push_str("\n## Earlier tasks this session\n");
        if cut.completed > 0 {
            out.push_str(&format!(
                "- [{} older entr(ies) dropped from this list to save space]\n",
                cut.completed
            ));
        }
        for line in completed {
            out.push_str(line);
            out.push('\n');
        }
    }

    out.push_str(
        "\nA command not listed here did NOT run, however sure you are that it did. Use this to \
         avoid redoing finished work and to resume a task interrupted partway.\n",
    );
    out
}

/// The note telling the model where its scratch directory is. Separate from
/// `build_block` because it's about capability rather than record, and it's
/// only true while a sandbox is actually available to write in.
pub fn scratch_note(cfg: &AppConfig) -> Option<String> {
    if !cfg.memory_enabled {
        return None;
    }
    Some(format!(
        "You have a scratch directory at {} that is writable and is cleared at the start of every \
         new task. Use it for working notes or intermediate files instead of writing them into the \
         user's folder. Nothing in it is read back into this prompt automatically -- read it with a \
         command if you want it.",
        temp_dir().display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Isolates one test's session directory on its own thread. Neither
    /// `CURRENT_SESSION` (a global `OnceLock`) nor `XDG_CONFIG_HOME` (a
    /// process-wide env var) can do that under the parallel test harness --
    /// caught the hard way, with one test asserting on another's commands.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-assistant-memory-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        TEST_SESSION_DIR.with(|d| *d.borrow_mut() = Some(dir.join("session")));
        let _ = fs::create_dir_all(temp_dir());
        dir
    }

    fn cfg(budget: u32) -> AppConfig {
        AppConfig {
            memory_enabled: true,
            memory_max_tokens: budget,
            ..Default::default()
        }
    }

    #[test]
    fn records_what_ran_and_what_did_not() {
        let dir = scratch("records");
        start_task(None, "organize my downloads");
        record_command("mkdir -p A", 0);
        record_blocked("rm -rf .", "the user denied it");

        let block = build_block(&cfg(0)).expect("expected a memory block");
        assert!(block.contains("organize my downloads"), "{block}");
        assert!(block.contains("ran: mkdir -p A -> exit 0"), "{block}");
        assert!(block.contains("NOT run (the user denied it)"), "{block}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_produces_nothing_at_all() {
        let dir = scratch("disabled");
        start_task(None, "do a thing");
        record_command("ls", 0);

        let off = AppConfig {
            memory_enabled: false,
            ..Default::default()
        };
        assert!(build_block(&off).is_none());
        assert!(scratch_note(&off).is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_task_archives_the_previous_ledger_and_keeps_intent() {
        let dir = scratch("archive");
        start_task(None, "first request");
        record_command("mkdir A", 0);
        start_task(None, "second request");
        record_command("mkdir B", 0);

        let block = build_block(&cfg(0)).unwrap();
        // Both requests are still intent; only B is current progress.
        assert!(block.contains("first request"), "{block}");
        assert!(block.contains("second request"), "{block}");
        let current = block.split("## Earlier tasks").next().unwrap();
        assert!(current.contains("mkdir B"), "{current}");
        assert!(
            !current.contains("mkdir A"),
            "the finished task's command should have moved to the archive: {current}"
        );
        assert!(block.contains("mkdir A"), "{block}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_block_respects_its_token_cap_and_says_what_it_cut() {
        let dir = scratch("cap");
        start_task(None, "first request");
        for i in 0..40 {
            record_command(&format!("mkdir folder-number-{i}"), 0);
        }
        start_task(None, "second request");
        for i in 0..40 {
            record_command(&format!("mv file-number-{i} folder-number-{i}"), 0);
        }

        let uncapped = build_block(&cfg(0)).unwrap();
        let capped = build_block(&cfg(400)).unwrap();
        assert!(
            estimate_tokens(&capped) <= 400,
            "still over budget: {} tokens",
            estimate_tokens(&capped)
        );
        assert!(capped.len() < uncapped.len());
        assert!(
            capped.contains("dropped from this list"),
            "truncation must be stated, not silent: {capped}"
        );
        // The least reproducible parts survive.
        assert!(capped.contains("second request"), "{capped}");

        let _ = fs::remove_dir_all(&dir);
    }

    // The fixed preamble alone costs more than a very small budget, so there
    // is a floor this can't get under. It must overshoot loudly rather than
    // silently, since a cap that quietly doesn't hold is worse than none.
    #[test]
    fn an_impossible_budget_overshoots_out_loud() {
        let dir = scratch("impossible-cap");
        start_task(None, "a request");
        record_command("ls", 0);

        let block = build_block(&cfg(10)).unwrap();
        assert!(estimate_tokens(&block) > 10, "sanity: the floor is bigger");
        assert!(
            block.contains("larger than its configured token budget"),
            "exceeding the cap must be stated: {block}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_task_clears_scratch_but_keeps_the_directory() {
        let dir = scratch("scratch-clear");
        fs::write(temp_dir().join("notes.txt"), "working").unwrap();
        start_task(None, "next thing");
        assert!(temp_dir().is_dir(), "scratch dir must still exist");
        assert!(!temp_dir().join("notes.txt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_folder_snapshot_ignores_the_trash_directory() {
        let dir = scratch("snapshot");
        let root = dir.join("work");
        fs::create_dir_all(root.join(".temp-trash")).unwrap();
        fs::create_dir_all(root.join("photos")).unwrap();
        fs::write(root.join("list.txt"), "x").unwrap();

        start_task(Some(&root), "tidy up");
        let block = build_block(&cfg(0)).unwrap();
        assert!(block.contains("photos/"), "{block}");
        assert!(block.contains("list.txt"), "{block}");
        assert!(
            !block.contains(".temp-trash"),
            "the app's own trash is not the user's content: {block}"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
