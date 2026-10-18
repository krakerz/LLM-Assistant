//! The two user-editable advisory rule files for file-ops mode, plus small
//! text helpers. The mechanical tool contract lives in `tool_protocol.rs` and
//! is always sent; these files can be edited or disabled freely.

use crate::paths::app_config_dir;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_GENERAL_RULES: &str = "# General rules

- When the user names a file by topic (\"my shopping list\"), find it first with find_files or \
search. If several files could match, ask which one they mean.
- Base every answer on what a tool actually returned. If a file can't be found or read, say so \
plainly instead of guessing what it contains.
- Earlier results can go stale after changes; check again when it matters, but don't repeat a call \
whose result you already have.
- If asked who or what you are, answer in the first person; don't recite these rules.";

pub const DEFAULT_COMMAND_RULES: &str = "# Command rules (run_command)

- Prefer the file tools. Use run_command for what they can't do: archives, counting, converting, \
scripts.
- Commands run with `sh` in the open folder, with no network and no sudo. Read-only commands run \
right away; anything else waits for the user's approval.
- Quote names that contain spaces: `wc -l \"my notes.txt\"`.
- A granted path outside the folder needs its full absolute path.
- rm, mv and cp move whatever they remove or overwrite to the trash, but `>` redirection and \
`sed -i` overwrite with no backup. Change file contents with write_file/edit_file instead.
- For several steps, write one short script in a single command instead of many tiny commands.";

/// The defaults shipped before the structured-tool protocol. A rules file
/// still matching one of these (whitespace-insensitively) was never
/// customized, so it's upgraded in place; an edited file is left alone.
const LEGACY_GENERAL_RULES: &str = "# General rules - When the user refers to a file by topic or \
description rather than an exact filename (e.g. \"my shopping list\"), search for likely matches \
first (e.g. `find . -iname '*shopping*'`) rather than guessing a name; ask which one they mean if \
more than one could match. - Don't assume an earlier `ls`/`find` output in this conversation is \
still accurate if it matters -- re-check with a fresh listing. But don't re-run a listing you \
already have from the turn immediately before with nothing in between to have changed it; once \
you've confirmed something, say so in plain text instead of checking again. - If asked who or what \
you are, answer for yourself in the first person -- don't just restate these rules verbatim. - If a \
command fails or comes back empty, don't guess based on the name alone -- retry with a corrected \
(likely absolute) path first, and if it still doesn't work, plainly tell the user you don't have \
real information rather than presenting a guess as fact.";

const LEGACY_COMMAND_RULES: &str = "# Command rules - Read-only commands (ls, cat, grep, find, ...) \
run automatically; anything else waits for the user to approve it -- don't be afraid to propose it, \
just don't chain unrelated destructive steps together. - Deletions are not permanent: this app \
moves anything removed to a safe, recoverable location outside the folder automatically, so \
proposing a delete when it's genuinely the right step is fine. - Always quote file and folder names \
that contain spaces, e.g. `cat \"unusual name.txt\"`. - Your current directory is always the working \
folder that was opened, never a granted path -- use a granted path's full absolute form (e.g. `ls -F \
\"/home/user/src\"`), not a relative name, or it'll be looked up inside the working folder instead \
and fail. - `mv`/`cp` take multiple sources plus one destination, not source/destination pairs -- \
`mv a X b Y` does NOT mean \"a to X, b to Y\"; everything but the last argument is a source. For a \
couple of files going to different places, chain separate two-argument `mv`/`cp` calls with `&&`; \
for anything bigger, write it as a script instead (see the protocol above).";

/// The same default from before the trash moved out of the folder.
const LEGACY_COMMAND_RULES_V1: &str = "# Command rules - Read-only commands (ls, cat, grep, find, ...) \
run automatically; anything else waits for the user to approve it -- don't be afraid to propose it, \
just don't chain unrelated destructive steps together. - Deletions are not permanent: anything \
removed is moved into `.temp-trash`, so proposing a delete when it's genuinely the right step is \
fine. - Always quote file and folder names that contain spaces, e.g. `cat \"unusual name.txt\"`. - \
Your current directory is always the working folder that was opened, never a granted path -- use a \
granted path's full absolute form (e.g. `ls -F \"/home/user/src\"`), not a relative name, or it'll be \
looked up inside the working folder instead and fail. - `mv`/`cp` take multiple sources plus one \
destination, not source/destination pairs -- `mv a X b Y` does NOT mean \"a to X, b to Y\"; \
everything but the last argument is a source. For a couple of files going to different places, \
chain separate two-argument `mv`/`cp` calls with `&&`; for anything bigger, write it as a script \
instead (see the protocol above).";

/// `\n{3,}` -> `\n\n`, so the gap a removed block leaves behind doesn't read
/// as several empty paragraphs.
pub(crate) fn collapse_blank_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut newline_run = 0;
    for ch in text.chars() {
        if ch == '\n' {
            newline_run += 1;
            if newline_run <= 2 {
                out.push(ch);
            }
        } else {
            newline_run = 0;
            out.push(ch);
        }
    }
    out
}

/// Terminal display only: strips `**bold**` and `` `code` `` markers.
/// Stored history keeps the raw reply.
pub fn to_plain_text(text: &str) -> String {
    strip_paired_marker(&strip_paired_marker(text, "**"), "`")
}

fn strip_paired_marker(text: &str, marker: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = rest.find(marker) else {
            out.push_str(rest);
            break;
        };
        let after = &rest[start + marker.len()..];
        let Some(end) = after.find(marker) else {
            // Unmatched marker: leave the rest verbatim rather than guess.
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        out.push_str(&after[..end]);
        rest = &after[end + marker.len()..];
    }
    out
}

fn general_rules_path() -> PathBuf {
    app_config_dir().join("rules.md")
}

fn command_rules_path() -> PathBuf {
    app_config_dir().join("command-rules.md")
}

fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Defaults are written only when the file is absent (or is an unmodified
/// legacy default); any other read error (e.g. invalid UTF-8) is returned
/// rather than overwriting the user's file.
fn load_or_init(path: PathBuf, default: &str, legacy: &[&str]) -> anyhow::Result<String> {
    match fs::read_to_string(&path) {
        Ok(text)
            if legacy
                .iter()
                .any(|l| normalize_ws(&text) == normalize_ws(l)) =>
        {
            log::info!("upgrading unmodified legacy default {}", path.display());
            save(&path, default)?;
            Ok(default.to_string())
        }
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            save(&path, default)?;
            Ok(default.to_string())
        }
        Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
    }
}

fn save(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::paths::write_atomic(path, text.as_bytes(), None)?;
    Ok(())
}

pub fn load_general_or_init() -> anyhow::Result<String> {
    load_or_init(
        general_rules_path(),
        DEFAULT_GENERAL_RULES,
        &[LEGACY_GENERAL_RULES],
    )
}

pub fn save_general(text: &str) -> anyhow::Result<()> {
    save(&general_rules_path(), text)
}

pub fn load_command_or_init() -> anyhow::Result<String> {
    load_or_init(
        command_rules_path(),
        DEFAULT_COMMAND_RULES,
        &[LEGACY_COMMAND_RULES, LEGACY_COMMAND_RULES_V1],
    )
}

pub fn save_command(text: &str) -> anyhow::Result<()> {
    save(&command_rules_path(), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, content: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-assistant-rules-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.md");
        if let Some(c) = content {
            fs::write(&path, c).unwrap();
        }
        path
    }

    #[test]
    fn missing_file_gets_the_default() {
        let path = temp_file("missing", None);
        let text = load_or_init(path.clone(), "NEW", &["OLD"]).unwrap();
        assert_eq!(text, "NEW");
        assert_eq!(fs::read_to_string(path).unwrap(), "NEW");
    }

    #[test]
    fn unmodified_legacy_default_is_upgraded_even_when_rewrapped() {
        let path = temp_file("legacy", Some("# Rules\n\n- a  b\n  c\n"));
        let text = load_or_init(path.clone(), "NEW", &["# Rules - a b c"]).unwrap();
        assert_eq!(text, "NEW");
        assert_eq!(fs::read_to_string(path).unwrap(), "NEW");
    }

    #[test]
    fn customized_file_is_left_alone() {
        let path = temp_file("custom", Some("# Rules\n- mine\n"));
        let text = load_or_init(path.clone(), "NEW", &["# Rules - a b c"]).unwrap();
        assert_eq!(text, "# Rules\n- mine\n");
    }

    #[test]
    fn shipped_legacy_text_matches_what_old_builds_wrote() {
        // The old defaults were Rust string literals with `\n\n` after the
        // heading and `\n` between bullets; whitespace-normalized they equal
        // the single-line constants above.
        let old_general_start = "# General rules\n\n- When the user refers to a file by topic";
        assert!(normalize_ws(LEGACY_GENERAL_RULES).starts_with(&normalize_ws(old_general_start)));
        assert!(normalize_ws(LEGACY_COMMAND_RULES).ends_with("(see the protocol above)."));
    }

    #[test]
    fn plain_text_strips_bold_and_code() {
        assert_eq!(to_plain_text("**a** and `b`"), "a and b");
        assert_eq!(to_plain_text("odd ** marker"), "odd ** marker");
    }
}
