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

/// The immediate-repeat check above only catches a command run twice *in a
/// row* -- an alternating loop (`ls -F`, `cat notes.txt`, `ls -F`, `cat
/// notes.txt`, ...) walks straight past it, and with `max_auto_steps = 0`
/// (unlimited) that reproduced as a genuinely endless chain during testing.
/// This is the second guard: once one exact command string has been run
/// this many times in a single auto-continue chain, regardless of what ran
/// in between, treat it as stuck. Must stay identical to the constant of
/// the same name in `ui/main.js`.
pub const STUCK_LOOP_REPEAT_THRESHOLD: u32 = 4;

/// Must stay identical to the note pushed in `ui/main.js` for the same guard.
pub const STUCK_LOOP_NOTE: &str = "[that exact command has come up too many times in this \
conversation without moving anything forward -- it was not run again. You already have its \
output from earlier -- work from that, or try something genuinely different.]";

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

/// Chat mode's entire mechanical contract -- mechanical because only the app
/// parses the ` ```state ` fence it describes, so only the app can teach the
/// syntax exists at all. What's actually worth tracking is up to whatever
/// persona is loaded (a character sheet might say to track HP, or a
/// relationship level, or nothing) -- this text never says what to track,
/// only how. Always sent in chat mode; there's no `disable_builtin_rules`
/// equivalent here since there's nothing else to conflict with it.
pub const CHAT_PROTOCOL_PROMPT: &str = "If you want something to reliably persist for the rest of \
this conversation -- a stat, a fact, a relationship status, anything your character sheet says to \
keep track of -- put the COMPLETE current version of it in a single fenced ```state code block \
somewhere in your reply, for example:\n\n```state\nHP: 85/100\nTrust in the user: growing\n```\n\n\
This replaces everything you wrote in your last ```state block, so restate everything you still \
want remembered, not just what changed -- anything you leave out is gone. It is never shown to the \
user as part of your reply, so don't reference it as if they can see it. If nothing needs to \
change, you don't need to include one at all.";

/// Forces every reply into the same narration/dialogue split
/// `ui/main.js`'s `renderChatText` renders as separate blocks (see the
/// "Roleplay text formatting" note in the project `CLAUDE.md`) -- most
/// models don't write this way on their own, so without an explicit
/// instruction a reply comes back as one plain paragraph and there's
/// nothing for the renderer to split; the visual feature is only as good as
/// the model actually producing the markers. Always sent in chat mode, same
/// reasoning and no off-switch as `CHAT_PROTOCOL_PROMPT` -- this is how the
/// app displays every reply, not a style the model is free to skip.
///
/// Also states whose POV narration is describing, added after a real
/// session where "you" narration read as the persona's own action rather
/// than the real person's -- with no rule pinning "you" to one side, a
/// model can drift into using it for either. "You" is reserved for the real
/// human, never the persona itself, so a reader is never left guessing who
/// is doing what.
pub const CHAT_NARRATION_PROMPT: &str = "Write your replies as an alternation of spoken dialogue \
and physical narration, like a script. Wrap any narration -- an action, a gesture, a description \
of the scene, anything that isn't actually being spoken aloud -- in a pair of double slashes on \
one line, for example: // she leans back and crosses her arms //. Keep each `//` pair on one \
line; never let one span multiple lines. Leave dialogue as plain text -- no `//`, no quotation \
marks needed. Use this format for every reply, even a short one, and even if it isn't your \
default style. Never use asterisks, or a leading period, or any other marker for narration -- \
`//` on both sides is the only recognized format.\n\n\
Be unambiguous about whose action or perspective each piece of narration describes. \"You\" \
always means the real person you're talking to -- the human actually typing, never yourself or \
your own character. Narrate your own character's actions in the third person by name (or first \
person \"I\" in dialogue), never as \"you\". For example: // Elara leans back and smiles // for \
your own character's action, and // You unzip the satchel // only when narrating something the \
real person did or is doing.";

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

/// Strips every ```sh/```bash/```shell fence out of `text`, mirroring
/// `parseAssistantReply` in `ui/main.js`: only the first fence in a reply
/// ever runs (see `extract_command` above), so a plain terminal client that
/// echoed the rest back would show commands that look pending but never run.
/// Returns the cleaned prose (blank-line runs collapsed, trimmed) plus how
/// many *extra* fences were found beyond the first -- the caller decides
/// whether that's worth telling the model about, same as the GUI does.
pub fn strip_command_fences(text: &str) -> (String, usize) {
    let mut out = text.to_string();
    let mut found: usize = 0;
    loop {
        let mut earliest: Option<(usize, usize)> = None;
        for lang in ["sh", "bash", "shell"] {
            let marker = format!("```{lang}\n");
            if let Some(pos) = out.find(&marker) {
                let body_start = pos + marker.len();
                let is_earlier = match earliest {
                    Some((e, _)) => pos < e,
                    None => true,
                };
                if is_earlier {
                    earliest = Some((pos, body_start));
                }
            }
        }
        let Some((marker_start, body_start)) = earliest else {
            break;
        };
        // An unterminated fence (a truncated reply) is left in place rather
        // than guessed at.
        let Some(end_rel) = out[body_start..].find("```") else {
            break;
        };
        out.replace_range(marker_start..body_start + end_rel + 3, "");
        found += 1;
    }
    (
        collapse_blank_runs(&out).trim().to_string(),
        found.saturating_sub(1),
    )
}

/// `\n{3,}` -> `\n\n`, same as the JS regex `parseAssistantReply` uses --
/// otherwise the gap a removed fence leaves behind reads as several empty
/// paragraphs.
fn collapse_blank_runs(text: &str) -> String {
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

/// The read side of `CHAT_PROTOCOL_PROMPT`'s ` ```state ` contract -- only
/// the first block counts, matching how only the first ` ```sh ` fence ever
/// runs in operation mode. `None` means the model didn't update its state
/// this turn, not that it cleared it.
pub fn extract_state_block(text: &str) -> Option<String> {
    let marker = "```state\n";
    let start = text.find(marker)? + marker.len();
    let end = text[start..].find("```")?;
    Some(text[start..start + end].trim().to_string())
}

/// Removes every ` ```state ` block from `text` for display -- the model is
/// told this content is never shown to the user, so leaving a stray one
/// in the chat bubble would contradict that.
pub fn strip_state_blocks(text: &str) -> String {
    let mut out = text.to_string();
    loop {
        let marker = "```state\n";
        let Some(pos) = out.find(marker) else {
            break;
        };
        let body_start = pos + marker.len();
        let Some(end_rel) = out[body_start..].find("```") else {
            break;
        };
        out.replace_range(pos..body_start + end_rel + 3, "");
    }
    collapse_blank_runs(&out).trim().to_string()
}

/// The read side of the "request a ruleset" contract: a ` ```ruleset <name> ```
/// ` fence, name on the opening line. Unlike `extract_command`'s fixed
/// language-tag set (`sh`/`bash`/`shell`), a ruleset name is open-ended, not
/// one of a few known values, so it can't live in the fence's language slot
/// the way a shell tag does -- it has to be read off the rest of that
/// opening line instead. Only the first request in a reply counts, matching
/// every other fence convention here. There's nothing useful the model
/// could put in the block's body (this is a request, not a payload), so the
/// body itself is never read -- only that a closing fence exists at all,
/// confirming the block is well-formed.
pub fn extract_ruleset_request(text: &str) -> Option<String> {
    let marker = "```ruleset ";
    let start = text.find(marker)? + marker.len();
    let line_end = start + text[start..].find('\n')?;
    let name = text[start..line_end].trim();
    if name.is_empty() {
        return None;
    }
    text[line_end..].find("```")?;
    Some(name.to_string())
}

/// Removes every ` ```ruleset <name> ``` ` request from `text` for display,
/// same reasoning as `strip_state_blocks` -- the model is told this is a
/// request the app answers on the next turn, not something to show as-is.
pub fn strip_ruleset_requests(text: &str) -> String {
    let mut out = text.to_string();
    loop {
        let marker = "```ruleset ";
        let Some(pos) = out.find(marker) else {
            break;
        };
        let line_start = pos + marker.len();
        let Some(line_end_rel) = out[line_start..].find('\n') else {
            break;
        };
        let body_start = line_start + line_end_rel + 1;
        let Some(end_rel) = out[body_start..].find("```") else {
            break;
        };
        out.replace_range(pos..body_start + end_rel + 3, "");
    }
    collapse_blank_runs(&out).trim().to_string()
}

/// The names of rulesets not yet loaded into this conversation, and how to
/// request one -- injected into the system message so the model knows what
/// it can ask for without every ruleset's full content being sent up front.
/// Empty when every existing ruleset is already loaded (or none exist).
pub fn build_ruleset_availability_note(names: &[String]) -> String {
    if names.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "## Optional rulesets available\n\
        Not loaded into this conversation yet. If a task calls for one, request it with \
        ```ruleset <name>``` on its own line, using the exact name below. Once requested it \
        stays available for the rest of this chat.\n\n",
    );
    for name in names {
        out.push_str("- ");
        out.push_str(name);
        out.push('\n');
    }
    out
}

/// The two tags reasoning-capable models are actually seen using in the
/// wild for their own chain-of-thought, wrapped around it in the plain
/// `content` string rather than in some separate API field -- which is the
/// only place this app, talking to an arbitrary OpenAI-compatible endpoint,
/// can look. Not something the app teaches the model to do (unlike
/// ` ```state ``` `/` ```sh ``` `) -- this only reads what a model already
/// produces unprompted, so there's no protocol text for it.
const THINKING_TAGS: &[&str] = &["think", "thinking"];

/// The model's own reasoning, if it wrapped any in a recognized tag --
/// `None` either because the model didn't, or because it's a plain
/// non-reasoning model. Only the first block is read, matching every other
/// fence/tag convention in this file.
pub fn extract_thinking_block(text: &str) -> Option<String> {
    for tag in THINKING_TAGS {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        if let Some(start) = text.find(&open) {
            let body_start = start + open.len();
            if let Some(end_rel) = text[body_start..].find(&close) {
                return Some(text[body_start..body_start + end_rel].trim().to_string());
            }
        }
    }
    None
}

/// Removes every recognized thinking tag from `text` for display -- shown
/// separately (or not at all, per `chat_show_thinking`), never left inline
/// where it would read as part of the answer itself.
pub fn strip_thinking_blocks(text: &str) -> String {
    let mut out = text.to_string();
    loop {
        let mut removed_any = false;
        for tag in THINKING_TAGS {
            let open = format!("<{tag}>");
            let close = format!("</{tag}>");
            let Some(start) = out.find(&open) else {
                continue;
            };
            let body_start = start + open.len();
            let Some(end_rel) = out[body_start..].find(&close) else {
                continue;
            };
            out.replace_range(start..body_start + end_rel + close.len(), "");
            removed_any = true;
        }
        if !removed_any {
            break;
        }
    }
    collapse_blank_runs(&out).trim().to_string()
}

/// Cosmetic, for terminal display only (both CLI entry points use this --
/// `headless.rs` and `chat_cli.rs`) -- stored/re-sent history always keeps
/// the raw reply, markup included, since that's what the model itself
/// wrote and re-reads. Strips `**bold**` and `` `code` `` markers so prose
/// reads as plain text instead of raw Markdown source; newlines are left
/// alone.
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
            // Unmatched marker -- leave the rest verbatim rather than guess.
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

/// Chat mode's whole system message: the mechanical `CHAT_PROTOCOL_PROMPT`
/// and `CHAT_NARRATION_PROMPT`, then the persona's own content (if one is
/// loaded), then the session's current ` ```state ` snapshot (if it's ever
/// written one). No root note, no rules.md/command-rules.md -- those are
/// operation-mode concepts with nothing to say here.
///
/// `state` is capped at `state_max_tokens` (0 = no cap) with an explicit
/// truncation note, same reasoning as `memory::build_block`'s cap: this goes
/// in the system message, which `context::fit_to_budget` never trims, so
/// nothing else is in a position to keep it in check.
pub fn build_chat_system_content(
    persona: Option<&str>,
    state: &str,
    state_max_tokens: u32,
    available_rulesets: &[String],
    loaded_rulesets: &[(String, String)],
) -> String {
    let mut out = CHAT_PROTOCOL_PROMPT.to_string();
    out.push_str("\n\n");
    out.push_str(CHAT_NARRATION_PROMPT);
    if let Some(persona) = persona {
        out.push_str("\n\n");
        out.push_str(persona);
    }
    let availability_note = build_ruleset_availability_note(available_rulesets);
    if !availability_note.is_empty() {
        out.push_str("\n\n");
        out.push_str(&availability_note);
    }
    for (name, content) in loaded_rulesets {
        out.push_str("\n\n## Loaded ruleset: ");
        out.push_str(name);
        out.push('\n');
        out.push_str(content);
    }
    let state = state.trim();
    if !state.is_empty() {
        let capped = if state_max_tokens == 0 {
            state.to_string()
        } else {
            crate::context::truncate_with_note(state, (state_max_tokens as usize) * 4, "state")
        };
        out.push_str("\n\n## Your persistent state from earlier in this conversation\n");
        out.push_str(&capped);
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

    #[test]
    fn strip_command_fences_removes_the_fence_and_reports_no_extras() {
        let reply = "Let's look.\n```sh\nls -F\n```";
        let (display, extra) = strip_command_fences(reply);
        assert_eq!(display, "Let's look.");
        assert_eq!(extra, 0);
    }

    #[test]
    fn strip_command_fences_counts_every_extra_fence() {
        let reply = "one\n```sh\ncmd1\n```\ntwo\n```bash\ncmd2\n```\nthree";
        let (display, extra) = strip_command_fences(reply);
        assert_eq!(display, "one\n\ntwo\n\nthree");
        assert_eq!(extra, 1, "two fences means one extra beyond the first");
    }

    #[test]
    fn strip_command_fences_leaves_an_untagged_fence_alone() {
        let reply = "Here's the file:\n```\nnot a command\n```";
        let (display, extra) = strip_command_fences(reply);
        assert_eq!(display, reply, "no tagged fence means nothing to strip");
        assert_eq!(extra, 0);
    }

    #[test]
    fn strip_command_fences_leaves_an_unterminated_fence_in_place() {
        let reply = "starting a command\n```sh\nls -F";
        let (display, extra) = strip_command_fences(reply);
        assert!(
            display.contains("```sh"),
            "a truncated reply should not have its only fence silently eaten: {display:?}"
        );
        assert_eq!(extra, 0);
    }

    #[test]
    fn extract_state_block_reads_the_content() {
        let reply = "Sure!\n```state\nHP: 85/100\n```\nDone.";
        assert_eq!(extract_state_block(reply).as_deref(), Some("HP: 85/100"));
    }

    #[test]
    fn extract_state_block_is_none_when_absent() {
        assert_eq!(extract_state_block("just a normal reply"), None);
    }

    #[test]
    fn strip_state_blocks_hides_it_from_display() {
        let reply = "Here's what happened.\n```state\nHP: 85/100\n```\nAnything else?";
        let display = strip_state_blocks(reply);
        assert!(!display.contains("```state"), "{display}");
        assert!(!display.contains("HP: 85/100"), "{display}");
        assert!(display.contains("Here's what happened."));
        assert!(display.contains("Anything else?"));
    }

    #[test]
    fn strip_state_blocks_is_a_no_op_without_one() {
        let reply = "just a normal reply";
        assert_eq!(strip_state_blocks(reply), reply);
    }

    #[test]
    fn build_chat_system_content_includes_persona_and_state() {
        let out =
            build_chat_system_content(Some("You are Aria, a shopkeeper."), "HP: 90", 0, &[], &[]);
        assert!(out.contains(CHAT_PROTOCOL_PROMPT));
        assert!(out.contains(CHAT_NARRATION_PROMPT));
        assert!(out.contains("You are Aria, a shopkeeper."));
        assert!(out.contains("HP: 90"));
    }

    #[test]
    fn build_chat_system_content_always_includes_the_narration_prompt() {
        // No persona, no state -- there's still no off-switch for this one.
        let out = build_chat_system_content(None, "", 0, &[], &[]);
        assert!(out.contains(CHAT_NARRATION_PROMPT));
    }

    #[test]
    fn build_chat_system_content_omits_empty_state() {
        let out = build_chat_system_content(None, "   ", 0, &[], &[]);
        assert!(!out.contains("persistent state"));
    }

    #[test]
    fn build_chat_system_content_caps_a_long_state() {
        let long_state = "x".repeat(10_000);
        let out = build_chat_system_content(None, &long_state, 50, &[], &[]);
        assert!(
            out.len() < long_state.len(),
            "expected the state to be truncated"
        );
        assert!(
            out.contains("condensed away"),
            "truncation must be stated, not silent: {out}"
        );
    }

    #[test]
    fn build_chat_system_content_lists_available_rulesets_but_not_their_content() {
        let out = build_chat_system_content(
            None,
            "",
            0,
            &[
                "image-generation-prompt".to_string(),
                "other-tools".to_string(),
            ],
            &[],
        );
        assert!(out.contains("image-generation-prompt"));
        assert!(out.contains("other-tools"));
        assert!(out.contains("```ruleset"));
    }

    #[test]
    fn build_chat_system_content_omits_the_availability_note_when_none_are_available() {
        let out = build_chat_system_content(None, "", 0, &[], &[]);
        assert!(!out.contains("Optional rulesets available"));
    }

    #[test]
    fn build_chat_system_content_includes_loaded_ruleset_content() {
        let out = build_chat_system_content(
            None,
            "",
            0,
            &[],
            &[(
                "other-tools".to_string(),
                "SearXNG URL: http://example".to_string(),
            )],
        );
        assert!(out.contains("Loaded ruleset: other-tools"));
        assert!(out.contains("SearXNG URL: http://example"));
    }

    #[test]
    fn extract_ruleset_request_reads_the_name() {
        let reply = "Sure, let me check.\n```ruleset other-tools\n```\nOne moment.";
        assert_eq!(
            extract_ruleset_request(reply).as_deref(),
            Some("other-tools")
        );
    }

    #[test]
    fn extract_ruleset_request_is_none_when_absent() {
        assert_eq!(extract_ruleset_request("just a normal reply"), None);
    }

    #[test]
    fn strip_ruleset_requests_hides_it_from_display() {
        let reply = "Here's what happened.\n```ruleset other-tools\n```\nAnything else?";
        let display = strip_ruleset_requests(reply);
        assert!(!display.contains("```ruleset"), "{display}");
        assert!(display.contains("Here's what happened."));
        assert!(display.contains("Anything else?"));
    }

    #[test]
    fn strip_ruleset_requests_is_a_no_op_without_one() {
        let reply = "just a normal reply";
        assert_eq!(strip_ruleset_requests(reply), reply);
    }

    #[test]
    fn extract_thinking_block_reads_a_think_tag() {
        let reply = "<think>The user wants the weather.</think>It's sunny.";
        assert_eq!(
            extract_thinking_block(reply).as_deref(),
            Some("The user wants the weather.")
        );
    }

    #[test]
    fn extract_thinking_block_reads_a_thinking_tag_too() {
        let reply = "<thinking>hmm</thinking>ok then";
        assert_eq!(extract_thinking_block(reply).as_deref(), Some("hmm"));
    }

    #[test]
    fn extract_thinking_block_is_none_for_a_plain_reply() {
        assert_eq!(extract_thinking_block("just an answer"), None);
    }

    #[test]
    fn strip_thinking_blocks_removes_it_from_display() {
        let reply = "<think>secret reasoning</think>The answer is 4.";
        let display = strip_thinking_blocks(reply);
        assert!(!display.contains("secret reasoning"), "{display}");
        assert!(!display.contains("<think>"), "{display}");
        assert_eq!(display, "The answer is 4.");
    }

    #[test]
    fn strip_thinking_blocks_is_a_no_op_without_one() {
        let reply = "just an answer";
        assert_eq!(strip_thinking_blocks(reply), reply);
    }
}
