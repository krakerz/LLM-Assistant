//! Two non-GUI ways to drive the assistant from a terminal:
//!
//! - `llm-assistant <folder> <message>` -- one turn (and any commands it
//!   leads to), printed to stdout, process exits. For scripting.
//! - `llm-assistant <folder> --chat` -- an interactive REPL, styled like
//!   `ollama run`: a `>>> ` prompt, plain-text replies (Markdown fences and
//!   `**`/`` ` `` markup stripped for display only), the same conversation
//!   history kept across the whole session. Ends on Ctrl+D (EOF) or Ctrl+C
//!   (default SIGINT termination -- nothing here needs cleanup, since every
//!   record is written to disk as it happens, not buffered).
//!
//! Both mirror `ui/main.js`'s orchestration; anything needing confirmation
//! is reported and stops the chain -- neither runs unattended what the GUI
//! wouldn't have run automatically.

use crate::config::AppConfig;
use crate::llm::{self, ChatMessage};
use crate::rules::to_plain_text;
use crate::sandbox::Classification;
use crate::{activate_root, config, context, memory, rules, sandbox};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

const PRIVILEGE_ESCALATION_BINARIES: &[&str] = &["sudo", "su", "doas", "pkexec"];

/// Must stay identical to the constant of the same name in `ui/main.js`:
/// the stand-in shown when a reply is nothing but a fenced command, so the
/// terminal isn't left printing a blank line for that step.
const COMMAND_ONLY_PLACEHOLDER: &str = "(proposed a command, shown below)";

/// Every word, not just the first: `cd /x && sudo ...` has it second.
fn is_privilege_escalation(cmd: &str) -> bool {
    cmd.split_whitespace().any(|word| {
        let bin = word.rsplit('/').next().unwrap_or(word);
        PRIVILEGE_ESCALATION_BINARIES.contains(&bin)
    })
}

/// Everything both entry points need, loaded once and hot-reloaded before
/// each top-level message in chat mode -- the same "config.toml/rules.md
/// edited mid-session take effect on the next message" behavior `main.js`
/// gives the GUI.
struct Setup {
    cfg: AppConfig,
    general_rules: String,
    command_rules: String,
    shims: PathBuf,
}

impl Setup {
    fn load(root: &Path) -> Option<Setup> {
        if let Err(e) = activate_root(root) {
            eprintln!("error: {e}");
            return None;
        }
        let shims = sandbox::default_shim_dir();
        if let Err(e) = sandbox::ensure_shims(&shims) {
            eprintln!("error setting up sandbox shims: {e}");
            return None;
        }
        let cfg = match config::load_or_init() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error loading config: {e}");
                return None;
            }
        };
        let general_rules = match rules::load_general_or_init() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error loading general rules: {e}");
                return None;
            }
        };
        let command_rules = match rules::load_command_or_init() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error loading command rules: {e}");
                return None;
            }
        };
        Some(Setup {
            cfg,
            general_rules,
            command_rules,
            shims,
        })
    }

    fn reload(&mut self) {
        if let Ok(cfg) = config::load_or_init() {
            self.cfg = cfg;
        }
        if let Ok(r) = rules::load_general_or_init() {
            self.general_rules = r;
        }
        if let Ok(r) = rules::load_command_or_init() {
            self.command_rules = r;
        }
    }
}

pub fn run(root: PathBuf, message: String) -> ! {
    let exit_code = tokio::runtime::Runtime::new()
        .expect("failed to start async runtime")
        .block_on(run_async(root, message));
    std::process::exit(exit_code);
}

pub fn run_chat(root: PathBuf) -> ! {
    let exit_code = tokio::runtime::Runtime::new()
        .expect("failed to start async runtime")
        .block_on(run_chat_async(root));
    std::process::exit(exit_code);
}

async fn run_async(root: PathBuf, message: String) -> i32 {
    let Some(setup) = Setup::load(&root) else {
        return 1;
    };

    // One invocation is one task.
    memory::start_task(&setup.cfg, Some(root.as_path()), &message);

    let mut history = vec![ChatMessage::text("user", message)];
    run_turn(&setup, &root, &mut history).await;
    0
}

async fn run_chat_async(root: PathBuf) -> i32 {
    let Some(mut setup) = Setup::load(&root) else {
        return 1;
    };

    println!("llm-assistant -- chatting in {}", root.display());
    println!("Type a message and press Enter. Ctrl+D or Ctrl+C to exit.");

    // Kept for the whole session and only ever appended to, same as
    // `history` in main.js -- this is what makes it "normal chat" rather
    // than a string of unrelated one-shot turns. `run_turn` gets a fresh
    // repeat-guard each call, matching the GUI creating a new
    // `createThinkingTracker()` per submitted message.
    let mut history: Vec<ChatMessage> = Vec::new();
    let stdin = io::stdin();

    loop {
        print!("\n>>> ");
        let _ = io::stdout().flush();

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                println!();
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("error reading input: {e}");
                break;
            }
        }
        let message = line.trim();
        if message.is_empty() {
            continue;
        }

        setup.reload();
        memory::start_task(&setup.cfg, Some(root.as_path()), message);
        history.push(ChatMessage::text("user", message));
        run_turn(&setup, &root, &mut history).await;
    }
    0
}

/// The final no-command turn after a loop guard fires: appends `note` (the
/// specific reason) plus `rules::FINAL_ANSWER_PROMPT`, then asks once more
/// for a plain-text answer with nothing left to run. Shared by both loop
/// guards in `run_turn` below -- they differ only in which note applies.
async fn wrap_up(setup: &Setup, history: &mut Vec<ChatMessage>, system_content: &str, note: &str) {
    history.push(ChatMessage::text("user", note));
    history.push(ChatMessage::text("user", rules::FINAL_ANSWER_PROMPT));
    // Same budget as every other turn: this used to send the whole
    // untrimmed history.
    let trimmed = context::fit_to_budget(
        context::estimate_tokens(system_content),
        history.clone(),
        setup.cfg.max_context_tokens as usize,
        None,
    )
    .await;
    let mut messages = vec![ChatMessage::text("system", system_content.to_string())];
    messages.extend(trimmed.messages);
    match llm::send_chat(
        &setup.cfg.endpoint,
        &setup.cfg.model,
        &setup.cfg.api_key,
        setup.cfg.temperature,
        &messages,
    )
    .await
    {
        Ok(final_reply) => {
            let (stripped, _) = rules::strip_command_fences(&final_reply);
            println!("\n{}", to_plain_text(&stripped));
            history.push(ChatMessage::text("assistant", final_reply));
        }
        Err(e) => eprintln!("error on final summary: {e}"),
    }
}

/// One auto-continue chain against `history`, printing as it goes: repeated
/// turns until the model answers with no command, hits the repeat guard,
/// proposes sudo, needs confirmation, or the step cap is reached. Doesn't
/// call `memory::start_task` itself -- the caller owns the task boundary,
/// since chat mode fires it once per user message while sharing the same
/// `history` across the whole session.
async fn run_turn(setup: &Setup, root: &Path, history: &mut Vec<ChatMessage>) {
    let Setup {
        cfg,
        general_rules,
        command_rules,
        shims,
    } = setup;

    let max_steps = if cfg.max_auto_steps == 0 {
        u32::MAX
    } else {
        cfg.max_auto_steps
    };

    // Two guards against a stuck chain, mirroring `ui/main.js`'s tracker.
    // `last_executed` catches the exact pattern seen in practice (`ls -F`,
    // "organization is complete", `ls -F`, ... repeated 20+ times) --
    // immediately identical, back to back. `executed_counts` catches the
    // pattern that guard alone doesn't: an alternating loop (`ls -F`, `cat
    // notes.txt`, `ls -F`, ...) never repeats *immediately*, and with
    // `max_auto_steps = 0` reproduced as a genuinely endless chain while
    // testing this. Both reset per top-level message, same as the GUI's
    // fresh `createThinkingTracker()` per submit.
    let mut last_executed: Option<String> = None;
    let mut executed_counts: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();

    for _step in 0..=max_steps {
        // Rebuilt each step: the session record it carries grows as commands run.
        let system_content =
            rules::build_system_content(cfg, general_rules, command_rules, Some(root));
        let summarizer = cfg.summarize_before_dropping.then(|| context::Summarizer {
            endpoint: &cfg.endpoint,
            model: &cfg.model,
            api_key: &cfg.api_key,
        });
        let trimmed = context::fit_to_budget(
            context::estimate_tokens(&system_content),
            history.clone(),
            cfg.max_context_tokens as usize,
            summarizer,
        )
        .await;
        if trimmed.condensed > 0 {
            eprintln!(
                "[condensed {} finished step(s) to command + output to fit the ~{} token context \
                 budget]",
                trimmed.condensed, cfg.max_context_tokens
            );
        }
        if let Some(summary) = &trimmed.summary {
            // In full, as the GUI shows it: an unseen model-written record is
            // the hazard.
            eprintln!(
                "[summarized {} old message(s) to fit the ~{} token context budget]\n{summary}",
                trimmed.summarized, cfg.max_context_tokens
            );
        }
        // Adopted so the summary is written once, not regenerated per step.
        if let Some(rewritten) = trimmed.rewritten_history {
            *history = rewritten;
        }
        if trimmed.dropped > 0 {
            eprintln!(
                "[dropped {} old message(s) to fit the ~{} token context budget]",
                trimmed.dropped, cfg.max_context_tokens
            );
        }
        let mut messages = vec![ChatMessage::text("system", system_content.clone())];
        messages.extend(trimmed.messages);

        let reply = match llm::send_chat(
            &cfg.endpoint,
            &cfg.model,
            &cfg.api_key,
            cfg.temperature,
            &messages,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: {e}");
                return;
            }
        };
        history.push(ChatMessage::text("assistant", reply.clone()));

        let extracted_cmd = rules::extract_command(&reply);
        let (stripped, extra_commands) = rules::strip_command_fences(&reply);
        let display = if stripped.is_empty() && extracted_cmd.is_some() {
            COMMAND_ONLY_PLACEHOLDER.to_string()
        } else {
            to_plain_text(&stripped)
        };
        println!("{display}");

        if extra_commands > 0 {
            // Mirrors main.js: only the first fence ever runs, so the model
            // needs to know the rest didn't -- otherwise it reports them as
            // done next turn (observed: a second `mv` chain sat unrun while
            // the model went on to claim the files were moved).
            println!(
                "\n[that reply included {} commands; only the first one ran]",
                extra_commands + 1
            );
            history.push(ChatMessage::text(
                "user",
                format!(
                    "[you included {} commands in fenced blocks in that reply -- only the first \
                     one ran, since only one command runs per reply. Don't assume the others \
                     happened; if they're still needed, propose the next one now that you have \
                     the first one's result.]",
                    extra_commands + 1
                ),
            ));
        }

        let Some(cmd) = extracted_cmd else {
            return; // final answer, no more commands proposed
        };

        let is_immediate_repeat = last_executed.as_deref() == Some(cmd.as_str());
        let is_stuck_cycle = !is_immediate_repeat
            && executed_counts.get(&cmd).copied().unwrap_or(0)
                >= rules::STUCK_LOOP_REPEAT_THRESHOLD;

        if is_immediate_repeat || is_stuck_cycle {
            if is_immediate_repeat {
                println!(
                    "\n[skipped a repeat of that command -- it was just run with nothing in \
                     between; wrapping up instead]"
                );
            } else {
                println!(
                    "\n[that command has come up {} times in this chain without moving anything \
                     forward -- stopping the automatic steps]",
                    rules::STUCK_LOOP_REPEAT_THRESHOLD
                );
            }
            let note = if is_immediate_repeat {
                rules::REPEATED_COMMAND_NOTE
            } else {
                rules::STUCK_LOOP_NOTE
            };
            wrap_up(setup, history, &system_content, note).await;
            return;
        }

        // The GUI records these via Tauri commands; headless runs the sandbox
        // directly, so it records them here.
        if is_privilege_escalation(&cmd) {
            // The raw reply (with the command visible in its fence) is no
            // longer printed above, so it has to be repeated here or the
            // user never sees what was actually proposed.
            println!(
                "\n[needs sudo/root -- this sandbox can never grant that; run it yourself]\n  $ {cmd}"
            );
            memory::record_blocked(
                cfg,
                &cmd,
                "needs sudo/root, which this sandbox can never grant",
            );
            return;
        }

        let auto_ok = matches!(sandbox::classify_command(&cmd), Classification::ReadOnly)
            || sandbox::is_auto_approved(&cmd, &cfg.auto_approve);
        if !auto_ok {
            println!("\n[needs confirmation -- not run automatically outside the GUI]: {cmd}");
            memory::record_blocked(cfg, &cmd, "needs confirmation, which this mode cannot give");
            return;
        }

        let scratch = cfg.memory_enabled.then(memory::temp_dir);
        match sandbox::run_sandboxed(root, shims, &cfg.granted_paths, scratch.as_deref(), &cmd) {
            Ok(outcome) => {
                last_executed = Some(cmd.clone());
                *executed_counts.entry(cmd.clone()).or_insert(0) += 1;
                memory::record_command(cfg, &cmd, outcome.exit_code);
                println!("\n$ {cmd}  (exit {})", outcome.exit_code);
                if !outcome.stdout.is_empty() {
                    print!("{}", outcome.stdout);
                }
                if !outcome.stderr.is_empty() {
                    eprint!("{}", outcome.stderr);
                }
                let combined = format!("{}{}", outcome.stdout, outcome.stderr);
                // From the shared prefix: `context.rs` recognizes a finished
                // step by exactly this shape.
                let mut feedback = format!(
                    "{}{}]\n{}",
                    context::COMMAND_OUTPUT_PREFIX,
                    outcome.exit_code,
                    combined.trim()
                );
                // Same hint as main.js: the rule lands better at the point of
                // failure than once at the top of the prompt.
                let lower = combined.to_lowercase();
                if outcome.exit_code != 0
                    && (lower.contains("no such file or directory")
                        || lower.contains("cannot access"))
                    && !cfg.granted_paths.is_empty()
                {
                    feedback.push_str(
                        "\n\n(hint: if you were trying to reach a granted path, use its full \
                         absolute path -- your current directory is always the working folder, \
                         never a granted path.)",
                    );
                }
                history.push(ChatMessage::text("user", feedback));
            }
            Err(e) => {
                println!("\n[execution error]: {e}");
                return;
            }
        }
    }
}
