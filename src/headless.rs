//! `llm-assistant <folder> <message>` runs one conversation turn (and any
//! commands it leads to) without launching the GUI, printing the result to
//! stdout and exiting -- for scripting and for testing the propose/classify/
//! execute loop directly. Mirrors `ui/main.js`'s orchestration logic, but
//! anything that would need a confirmation dialog (not auto-approved) just
//! gets reported and stops the loop, since there's no one here to click
//! "Run it" -- headless mode never runs a command unattended that the GUI
//! wouldn't have run automatically either.

use crate::llm::{self, ChatMessage};
use crate::sandbox::Classification;
use crate::{activate_root, config, rules, sandbox};
use std::path::PathBuf;

const PRIVILEGE_ESCALATION_BINARIES: &[&str] = &["sudo", "su", "doas", "pkexec"];

/// Checks every word, not just the first -- a compound command like
/// `cd /x && sudo pacman -Syu` has `sudo` as its second word, and missing
/// that would let it fall through to the (doomed) normal confirm flow.
fn is_privilege_escalation(cmd: &str) -> bool {
    cmd.split_whitespace().any(|word| {
        let bin = word.rsplit('/').next().unwrap_or(word);
        PRIVILEGE_ESCALATION_BINARIES.contains(&bin)
    })
}

/// Only an explicitly-tagged fence counts, matching the frontend's rule: a
/// plain ``` fence is just the model showing text, not proposing a command.
fn extract_sh_command(text: &str) -> Option<String> {
    for lang in ["sh", "bash", "shell"] {
        let marker = format!("```{lang}\n");
        if let Some(pos) = text.find(&marker) {
            let start = pos + marker.len();
            if let Some(end) = text[start..].find("```") {
                return Some(text[start..start + end].trim().to_string());
            }
        }
    }
    None
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

    let root_note = config::build_root_note(Some(root.as_path()), &cfg.granted_paths);
    let rules_block =
        rules::build_system_rules(&general_rules, &command_rules, cfg.disable_builtin_rules);
    let system_content = format!("{}\n\n{}\n\n{}", rules_block, cfg.system_prompt, root_note);

    let mut history = vec![ChatMessage {
        role: "user".into(),
        content: message,
    }];

    let max_steps = if cfg.max_auto_steps == 0 {
        u32::MAX
    } else {
        cfg.max_auto_steps
    };

    // Mirrors main.js's isImmediateRepeat guard: if the model proposes the
    // exact same command again right after it ran, with nothing else having
    // run in between, stop instead of letting it spin -- seen in practice as
    // an ls -F / "organization is complete" loop that ran until max_steps
    // (which is unbounded when configured to 0).
    let mut last_executed: Option<String> = None;

    for step in 0..=max_steps {
        let mut messages = vec![ChatMessage {
            role: "system".into(),
            content: system_content.clone(),
        }];
        messages.extend(history.clone());

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

        let Some(cmd) = extract_sh_command(&reply) else {
            break; // final answer, no more commands proposed
        };

        if last_executed.as_deref() == Some(cmd.as_str()) {
            println!(
                "\n[stopping -- that exact command was just run and nothing happened in between, \
                 repeating it won't produce new information]"
            );
            break;
        }

        if is_privilege_escalation(&cmd) {
            println!("\n[needs sudo/root -- this sandbox can never grant that; run it yourself]");
            break;
        }

        let auto_ok = matches!(sandbox::classify_command(&cmd), Classification::ReadOnly)
            || sandbox::is_auto_approved(&cmd, &cfg.auto_approve);
        if !auto_ok {
            println!("\n[needs confirmation -- not run automatically in headless mode]: {cmd}");
            break;
        }

        match sandbox::run_sandboxed(&root, &shims, &cfg.granted_paths, &cmd) {
            Ok(outcome) => {
                last_executed = Some(cmd.clone());
                println!("\n$ {cmd}  (exit {})", outcome.exit_code);
                if !outcome.stdout.is_empty() {
                    print!("{}", outcome.stdout);
                }
                if !outcome.stderr.is_empty() {
                    eprint!("{}", outcome.stderr);
                }
                let combined = format!("{}{}", outcome.stdout, outcome.stderr);
                let mut feedback = format!(
                    "[command output, exit {}]\n{}",
                    outcome.exit_code,
                    combined.trim()
                );
                // See main.js's identical hint -- reinforcing the
                // absolute-path rule right at the point of failure works
                // better than a rule stated once at the top of the prompt.
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
