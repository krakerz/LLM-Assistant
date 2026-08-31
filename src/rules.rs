use crate::paths::app_config_dir;
use std::fs;
use std::path::PathBuf;

/// The mechanical contract the app's own code assumes -- always sent, never
/// editable, unaffected by `disable_builtin_rules`. Judgment-call guidance
/// belongs in the two .md files, which can be discarded safely.
pub const PROTOCOL_PROMPT: &str = "# Protocol (always in effect)\n\n\
- When you want to run a command, give a one-line explanation, then put exactly one shell command in \
a single fenced ```sh code block, for example:\n\n  Let's see what's here.\n  ```sh\n  ls -F\n  ```\n\n  \
This applies even to a simple one-off check like `ls` -- never just state a command as plain text \
without the fence, since that does not run it. Only the first ```sh block in a reply ever runs -- \
anything after it is silently ignored, so don't plan multiple commands in one reply.\n\
- Only ever put text inside a ```sh (or ```bash/```shell) fence when it's a literal command to run -- \
never to show file contents or other text; use a plain fence (or none) for that.\n\
- For a task that needs several steps (a loop, a conditional, or more than a couple of sequential \
actions), write the whole thing as one self-contained script inside that single fenced block -- \
checking each step as it goes (e.g. `mkdir -p` before `mv`) -- and run it once, rather than proposing \
many small commands one at a time that can leave things half-done if something goes wrong partway \
through and needs cleaning up.\n\
- Every command's output is shown to the user AND handed back to you automatically. So never re-run a \
command just to display or confirm something you already have: if you just ran `ls` and want to describe \
the result, quote the output you already received rather than proposing that same listing again. In \
particular, don't end a reply with \"here is the current structure:\" (or similar) followed by another \
listing command -- write out the structure from the output you already have. When the work is done, say \
so in plain text with no command at all.\n\
- Only ever report an action as done if a command actually ran and its output shows it worked. A \
command the user denied did NOT run and changed nothing -- say exactly that, and never describe files as \
moved, created, or deleted based on what you intended rather than what you saw happen. If you aren't \
sure a change actually landed, run one listing to check and go by that output; if it only partly worked, \
say which parts succeeded and which didn't.\n\
- The explanation line above a command describes something that has NOT happened yet, so write it that \
way: \"this will move the files into folders\", not \"the files have been organized\". The command runs \
after you finish writing, and it can still be refused or fail. Past tense there is how a report of work \
that never happened gets written one line at a time.\n\
- `sudo`, `su`, `doas`, and `pkexec` will always fail in this sandbox no matter what, even if approved \
-- never propose them as a command to run; tell the user to run it themselves in their own terminal.\n\
- `.temp-trash/` in the working folder is created and managed by this app itself, holding soft-deleted \
files -- it is not part of the user's own content. Ignore it entirely (don't list it, move it, sort it, \
count it, or otherwise touch it) in any command you write, unless the user explicitly asks about \
deleted or trashed files.";

/// Advisory only; nothing here is required for the app to work.
pub const DEFAULT_GENERAL_RULES: &str = "# General rules\n\n\
- When the user refers to a file by topic or description rather than an exact filename (e.g. \"my \
shopping list\"), search for likely matches first (e.g. `find . -iname '*shopping*'`) rather than \
guessing a name; ask which one they mean if more than one could match.\n\
- Don't assume an earlier `ls`/`find` output in this conversation is still accurate if it matters -- \
re-check with a fresh listing. But don't re-run a listing you already have from the turn immediately \
before with nothing in between to have changed it; once you've confirmed something, say so in plain \
text instead of checking again.\n\
- If asked who or what you are, answer for yourself in the first person -- don't just restate these \
rules verbatim.\n\
- If a command fails or comes back empty, don't guess based on the name alone -- retry with a corrected \
(likely absolute) path first, and if it still doesn't work, plainly tell the user you don't have real \
information rather than presenting a guess as fact.";

/// Advisory shell mechanics beyond the protocol above.
pub const DEFAULT_COMMAND_RULES: &str = "# Command rules\n\n\
- Read-only commands (ls, cat, grep, find, ...) run automatically; anything else waits for the user to \
approve it -- don't be afraid to propose it, just don't chain unrelated destructive steps together.\n\
- Deletions are not permanent: anything removed is moved into `.temp-trash`, so proposing a delete when \
it's genuinely the right step is fine.\n\
- Always quote file and folder names that contain spaces, e.g. `cat \"unusual name.txt\"`.\n\
- Your current directory is always the working folder that was opened, never a granted path -- use a \
granted path's full absolute form (e.g. `ls -F \"/home/user/src\"`), not a relative name, or it'll be \
looked up inside the working folder instead and fail.\n\
- `mv`/`cp` take multiple sources plus one destination, not source/destination pairs -- `mv a X b Y` \
does NOT mean \"a to X, b to Y\"; everything but the last argument is a source. For a couple of files \
going to different places, chain separate two-argument `mv`/`cp` calls with `&&`; for anything bigger, \
write it as a script instead (see the protocol above).";

/// Must stay identical to the note `runAssistantTurn` pushes in `ui/main.js`.
pub const REPEATED_COMMAND_NOTE: &str = "[you proposed the exact same command again immediately \
after it already ran, with nothing new to justify re-running it -- it was not run again. You \
already have its output above.]";

/// Wrap-up turn after a hard stop, commands off the table. Every clause is
/// load-bearing: "what was done and what the final result is" presupposed
/// success and got a fabricated reorganization; "you already have everything
/// you need" is false after trimming and reads as permission to fill gaps.
///
/// Must stay identical to `finalAnswerTurn` in `ui/main.js` -- headless kept
/// the old wording for a release and reproduced the fabrication.
pub const FINAL_ANSWER_PROMPT: &str = "[don't run anything else. Reply now in plain text, with no \
command and no code fence, describing the CURRENT state strictly from the command output you \
actually received above. If commands were denied, never ran, or failed, say that plainly -- do not \
describe any file as moved, created, or deleted unless output above shows it actually happened. If \
part of this conversation was summarized or dropped to save context, don't reconstruct what was in \
it: say you no longer have it rather than describing file contents you cannot see.]";

/// The read side of `PROTOCOL_PROMPT`'s fence contract. Only an
/// explicitly-tagged fence counts, matching `parseAssistantReply` in
/// `ui/main.js`: a plain ``` fence is the model showing text.
pub fn extract_command(text: &str) -> Option<String> {
    // Earliest fence wins, not the first matching language.
    let mut earliest: Option<usize> = None;
    for lang in ["sh", "bash", "shell"] {
        if let Some(pos) = text.find(&format!("```{lang}\n")) {
            let start = pos + lang.len() + 4; // ``` + lang + \n
            earliest = Some(match earliest {
                Some(e) if e <= start => e,
                _ => start,
            });
        }
    }
    let start = earliest?;
    let end = text[start..].find("```")?;
    Some(text[start..start + end].trim().to_string())
}

fn general_rules_path() -> PathBuf {
    app_config_dir().join("rules.md")
}

fn command_rules_path() -> PathBuf {
    app_config_dir().join("command-rules.md")
}

fn load_or_init(path: PathBuf, default: &str) -> anyhow::Result<String> {
    match fs::read_to_string(&path) {
        Ok(text) => Ok(text),
        Err(_) => {
            save(&path, default)?;
            Ok(default.to_string())
        }
    }
}

fn save(path: &PathBuf, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)?;
    Ok(())
}

pub fn load_general_or_init() -> anyhow::Result<String> {
    load_or_init(general_rules_path(), DEFAULT_GENERAL_RULES)
}

pub fn save_general(text: &str) -> anyhow::Result<()> {
    save(&general_rules_path(), text)
}

pub fn load_command_or_init() -> anyhow::Result<String> {
    load_or_init(command_rules_path(), DEFAULT_COMMAND_RULES)
}

pub fn save_command(text: &str) -> anyhow::Result<()> {
    save(&command_rules_path(), text)
}

/// Protocol plus the advisory rules, unless `disable_builtin_rules`.
pub fn build_system_rules(general: &str, command: &str, disable_builtin_rules: bool) -> String {
    if disable_builtin_rules {
        PROTOCOL_PROMPT.to_string()
    } else {
        format!("{PROTOCOL_PROMPT}\n\n{general}\n\n{command}")
    }
}

/// One turn's whole system message: rules, user prompt, folder note, then the
/// session record last (the only part that grows during a session).
///
/// One place because `send_message` and `headless.rs` have already drifted
/// twice -- once on the rules block, once on the wrap-up prompt.
pub fn build_system_content(
    cfg: &crate::config::AppConfig,
    general: &str,
    command: &str,
    root: Option<&std::path::Path>,
) -> String {
    let mut out = format!(
        "{}\n\n{}\n\n{}",
        build_system_rules(general, command, cfg.disable_builtin_rules),
        cfg.system_prompt,
        crate::config::build_root_note(root, &cfg.granted_paths)
    );
    // No folder, no sandbox, so nothing could write to scratch anyway.
    if root.is_some() {
        if let Some(note) = crate::memory::scratch_note(cfg) {
            out.push_str("\n\n");
            out.push_str(&note);
        }
    }
    if let Some(block) = crate::memory::build_block(cfg) {
        // Every turn, unlike the rules: this is the part that changes, and
        // "what did it know at step 7" is only answerable if each copy is logged.
        log::debug!("session record sent this turn:\n{block}");
        out.push_str("\n\n");
        out.push_str(&block);
    }
    out
}

/// Once at startup, not in `load_*_or_init` (which run every turn), so a
/// stale edit or a reset that didn't take is visible without spamming.
pub fn log_loaded_rules(disable_builtin_rules: bool) {
    log::info!("protocol prompt (always in effect, not editable):\n{PROTOCOL_PROMPT}");
    let sent_or_not = if disable_builtin_rules {
        "on disk, but NOT sent -- disable_builtin_rules is set"
    } else {
        "sent"
    };
    match load_general_or_init() {
        Ok(r) => log::info!("general rules ({} bytes, {sent_or_not}):\n{r}", r.len()),
        Err(e) => log::warn!("failed to load general rules for startup log: {e}"),
    }
    match load_command_or_init() {
        Ok(r) => log::info!("command rules ({} bytes, {sent_or_not}):\n{r}", r.len()),
        Err(e) => log::warn!("failed to load command rules for startup log: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_system_rules_includes_advisory_rules_by_default() {
        let combined = build_system_rules("GENERAL", "COMMAND", false);
        assert!(combined.contains(PROTOCOL_PROMPT));
        assert!(combined.contains("GENERAL"));
        assert!(combined.contains("COMMAND"));
    }

    #[test]
    fn build_system_rules_drops_advisory_rules_when_disabled() {
        let combined = build_system_rules("GENERAL", "COMMAND", true);
        assert_eq!(combined, PROTOCOL_PROMPT);
        assert!(!combined.contains("GENERAL"));
        assert!(!combined.contains("COMMAND"));
    }

    #[test]
    fn extract_command_reads_a_tagged_fence() {
        let reply = "Let's look.\n```sh\nls -F\n```\n";
        assert_eq!(extract_command(reply).as_deref(), Some("ls -F"));
    }

    #[test]
    fn extract_command_ignores_an_untagged_fence() {
        let reply = "Here's the file:\n```\nnot a command\n```\n";
        assert_eq!(extract_command(reply), None);
    }

    #[test]
    fn extract_command_takes_the_earliest_fence_not_the_first_language() {
        let reply = "```bash\nfirst\n```\nand\n```sh\nsecond\n```";
        assert_eq!(extract_command(reply).as_deref(), Some("first"));
    }
}
