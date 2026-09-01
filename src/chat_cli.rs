//! `llm-assistant --persona-chat [--persona <name>] [--session <id>]` --
//! chat mode's own terminal REPL, entirely separate from operation mode's
//! `llm-assistant <folder> --chat` (`headless.rs`): no folder, no sandbox,
//! no shell commands, ever. Same shared turn as the GUI's chat mode
//! (`chat_turn::run_chat_turn`), so the two can't drift.
//!
//! Also handles `--list-personas` and `--list-sessions`, small utility
//! flags for finding out what to pass to `--persona`/`--session`.

use crate::llm::ChatMessage;
use crate::rules::to_plain_text;
use crate::{chat_session, chat_turn, config, persona};
use std::io::{self, BufRead, Write};

pub fn list_personas() {
    match persona::list_personas() {
        Ok(personas) if personas.is_empty() => {
            println!(
                "No personas yet. Add one under the personas folder, or use the GUI's chat mode."
            )
        }
        Ok(personas) => {
            for p in personas {
                println!("{}", p.name);
            }
        }
        Err(e) => eprintln!("error listing personas: {e}"),
    }
}

pub fn list_sessions() {
    match chat_session::list_sessions() {
        Ok(sessions) if sessions.is_empty() => {
            println!("No chat sessions yet. Start one with --persona-chat.")
        }
        Ok(sessions) => {
            for s in sessions {
                let persona = s.persona.as_deref().unwrap_or("(no persona)");
                println!("{}  {:<30}  {}", s.id, s.title, persona);
            }
        }
        Err(e) => eprintln!("error listing sessions: {e}"),
    }
}

pub struct Options {
    pub persona: Option<String>,
    pub session_id: Option<String>,
}

pub fn run(opts: Options) -> ! {
    let exit_code = tokio::runtime::Runtime::new()
        .expect("failed to start async runtime")
        .block_on(run_async(opts));
    std::process::exit(exit_code);
}

/// Resumes `session_id` if given (erroring if it doesn't exist -- a typo'd
/// ID should never silently start a brand new chat instead), otherwise
/// starts a fresh session with `persona` (which may itself be `None`, a
/// plain persona-less chat).
fn resolve_session(opts: &Options) -> anyhow::Result<String> {
    if let Some(id) = &opts.session_id {
        chat_session::load_session(id)?; // errors if it doesn't exist
        return Ok(id.clone());
    }
    Ok(chat_session::create_session(opts.persona.as_deref())?.id)
}

async fn run_async(opts: Options) -> i32 {
    let mut cfg = match config::load_or_init() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error loading config: {e}");
            return 1;
        }
    };

    let session_id = match resolve_session(&opts) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let (meta, mut history) = match chat_session::load_session(&session_id) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error loading session: {e}");
            return 1;
        }
    };

    println!("llm-assistant -- persona chat: {}", meta.title);
    if let Some(p) = &meta.persona {
        println!("persona: {p}");
    }
    println!("session: {session_id}");
    println!("Type a message and press Enter. Ctrl+D or Ctrl+C to exit.");

    // Replay scrollback when resuming an existing session -- otherwise the
    // conversation would look like it started from nothing.
    for m in &history {
        match m.role.as_str() {
            "user" => println!("\n>>> {}", m.content),
            "assistant" => {
                if let Some(t) = &m.thinking {
                    println!("\n🧠 {}", to_plain_text(t));
                }
                println!("\n{}", to_plain_text(&m.content));
            }
            _ => {}
        }
    }

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
        let text = line.trim();
        if text.is_empty() {
            continue;
        }

        // Hot-reload, matching every other long-running entry point in this
        // app: config.toml edited mid-session takes effect on the next turn.
        if let Ok(reloaded) = config::load_or_init() {
            cfg = reloaded;
        }

        history.push(ChatMessage::text("user", text));
        match chat_turn::run_chat_turn(&cfg, &session_id, history.clone()).await {
            Ok(outcome) => {
                if let Some(rewritten) = outcome.rewritten_history {
                    history = rewritten;
                }
                history.push(ChatMessage::text("assistant", outcome.reply.clone()));

                if let Some(thinking) = &outcome.thinking {
                    println!("\n🧠 {}", to_plain_text(thinking));
                }
                println!("\n{}", to_plain_text(&outcome.reply));
                if outcome.state_updated {
                    println!("\n[memories updated]");
                }
                if let Some(name) = &outcome.ruleset_loaded {
                    println!("\n[loaded ruleset: {name}]");
                }
                if let Some(err) = &outcome.ruleset_error {
                    println!("\n[{err}]");
                }
                if outcome.image_prompt_requested.is_some() {
                    println!(
                        "\n[requested an image -- generation only happens through the GUI, not this CLI]"
                    );
                }
                if let Some(summary) = &outcome.summary {
                    eprintln!(
                        "\n[summarized {} old message(s) to fit the context budget]\n{summary}",
                        outcome.summarized
                    );
                }
                if outcome.dropped > 0 {
                    eprintln!(
                        "\n[dropped {} old message(s) to fit the context budget]",
                        outcome.dropped
                    );
                }
            }
            Err(e) => {
                // The failed turn's user message was already pushed onto
                // `history` above -- pop it back off, or the next successful
                // turn would silently resend a message the model never saw
                // a reply persisted for.
                history.pop();
                eprintln!("\nerror: {e}");
            }
        }
    }
    0
}
