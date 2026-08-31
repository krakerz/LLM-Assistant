//! `llm-assistant <folder> <message>` runs one turn (and any commands it
//! leads to) without the GUI. Mirrors `ui/main.js`'s orchestration, but
//! anything needing confirmation is reported and stops the loop -- it never
//! runs unattended what the GUI wouldn't have run automatically.

use crate::llm::{self, ChatMessage};
use crate::sandbox::Classification;
use crate::{activate_root, config, context, memory, rules, sandbox};
use std::path::PathBuf;

const PRIVILEGE_ESCALATION_BINARIES: &[&str] = &["sudo", "su", "doas", "pkexec"];

/// Every word, not just the first: `cd /x && sudo ...` has it second.
fn is_privilege_escalation(cmd: &str) -> bool {
    cmd.split_whitespace().any(|word| {
        let bin = word.rsplit('/').next().unwrap_or(word);
        PRIVILEGE_ESCALATION_BINARIES.contains(&bin)
    })
}

pub fn run(root: PathBuf, message: String) -> ! {
    let exit_code = tokio::runtime::Runtime::new()
        .expect("failed to start async runtime")
        .block_on(run_async(root, message));
    std::process::exit(exit_code);
}

async fn run_async(root: PathBuf, message: String) -> i32 {
    if let Err(e) = activate_root(&root) {
        eprintln!("error: {e}");
        return 1;
    }
    let shims = sandbox::default_shim_dir();
    if let Err(e) = sandbox::ensure_shims(&shims) {
        eprintln!("error setting up sandbox shims: {e}");
        return 1;
    }

    let cfg = match config::load_or_init() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error loading config: {e}");
            return 1;
        }
    };
    let general_rules = match rules::load_general_or_init() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error loading general rules: {e}");
            return 1;
        }
    };
    let command_rules = match rules::load_command_or_init() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error loading command rules: {e}");
            return 1;
        }
    };

    // One invocation is one task.
    memory::start_task(Some(root.as_path()), &message);

    let mut history = vec![ChatMessage {
        role: "user".into(),
        content: message,
    }];

    let max_steps = if cfg.max_auto_steps == 0 {
        u32::MAX
    } else {
        cfg.max_auto_steps
    };

    // Mirrors main.js's isImmediateRepeat guard: seen looping ls -F /
    // "organization is complete" until max_steps.
    let mut last_executed: Option<String> = None;

    for step in 0..=max_steps {
        // Rebuilt each step: the session record it carries grows as commands run.
        let system_content =
            rules::build_system_content(&cfg, &general_rules, &command_rules, Some(root.as_path()));
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
            history = rewritten;
        }
        if trimmed.dropped > 0 {
            eprintln!(
                "[dropped {} old message(s) to fit the ~{} token context budget]",
                trimmed.dropped, cfg.max_context_tokens
            );
        }
        let mut messages = vec![ChatMessage {
            role: "system".into(),
            content: system_content.clone(),
        }];
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
                return 1;
            }
        };
        history.push(ChatMessage {
            role: "assistant".into(),
            content: reply.clone(),
        });

        println!("--- step {step} ---");
        println!("{reply}");

        let Some(cmd) = rules::extract_command(&reply) else {
            break; // final answer, no more commands proposed
        };

        if last_executed.as_deref() == Some(cmd.as_str()) {
            println!(
                "\n[skipped a repeat of that command -- it was just run with nothing in between; \
                 wrapping up instead]"
            );
            // Fires when the work is done and it's re-running a listing to
            // show output it has. One no-command turn so it ends on an answer.
            history.push(ChatMessage {
                role: "user".into(),
                content: rules::REPEATED_COMMAND_NOTE.into(),
            });
            history.push(ChatMessage {
                role: "user".into(),
                content: rules::FINAL_ANSWER_PROMPT.into(),
            });
            // Same budget as every other turn: this used to send the whole
            // untrimmed history.
            let trimmed = context::fit_to_budget(
                context::estimate_tokens(&system_content),
                history.clone(),
                cfg.max_context_tokens as usize,
                None,
            )
            .await;
            let mut messages = vec![ChatMessage {
                role: "system".into(),
                content: system_content.clone(),
            }];
            messages.extend(trimmed.messages);
            match llm::send_chat(
                &cfg.endpoint,
                &cfg.model,
                &cfg.api_key,
                cfg.temperature,
                &messages,
            )
            .await
            {
                Ok(final_reply) => println!("\n--- final ---\n{final_reply}"),
                Err(e) => eprintln!("error on final summary: {e}"),
            }
            break;
        }

        // The GUI records these via Tauri commands; headless runs the sandbox
        // directly, so it records them here.
        if is_privilege_escalation(&cmd) {
            println!("\n[needs sudo/root -- this sandbox can never grant that; run it yourself]");
            memory::record_blocked(&cmd, "needs sudo/root, which this sandbox can never grant");
            break;
        }

        let auto_ok = matches!(sandbox::classify_command(&cmd), Classification::ReadOnly)
            || sandbox::is_auto_approved(&cmd, &cfg.auto_approve);
        if !auto_ok {
            println!("\n[needs confirmation -- not run automatically in headless mode]: {cmd}");
            memory::record_blocked(&cmd, "needs confirmation, which headless mode cannot give");
            break;
        }

        match sandbox::run_sandboxed(&root, &shims, &cfg.granted_paths, &cmd) {
            Ok(outcome) => {
                last_executed = Some(cmd.clone());
                memory::record_command(&cmd, outcome.exit_code);
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
                history.push(ChatMessage {
                    role: "user".into(),
                    content: feedback,
                });
            }
            Err(e) => {
                println!("\n[execution error]: {e}");
                break;
            }
        }
    }

    0
}
