//! `llm-assistant --persona-chat [--persona <name>] [--session <id>]` --
//! chat mode's own terminal REPL, entirely separate from operation mode's
//! `llm-assistant <folder> --chat` (`headless.rs`): no folder, no sandbox,
//! no shell commands, ever. Same shared turn as the GUI's chat mode
//! (`chat_turn::run_chat_turn`), so the two can't drift.
//!
//! Also handles `--list-personas` and `--list-sessions`, small utility
//! flags for finding out what to pass to `--persona`/`--session`.
//!
//! An `` ```image-prompt``` `` request actually runs here too (same
//! `chat_turn::run_full_image_generation` the GUI uses) -- this is a
//! terminal, so there's nowhere to render the result, but printing the
//! saved file's path is enough to inspect it by hand afterward, and turned
//! out to be a much faster way to debug dispatch-pass reliability than
//! going back and forth through the GUI.

use crate::llm::ChatMessage;
use crate::rules::to_plain_text;
use crate::{chat_session, chat_turn, comfyui, config, persona, searxng};
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

    let persona_content = match &meta.persona {
        Some(name) => persona::load_persona(name).ok(),
        None => None,
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

    // A `tokio::spawn`'d state-update task keeps running even after its
    // `JoinHandle` is dropped -- but not if the *process itself* exits
    // first, which this short-lived CLI does the moment stdin closes.
    // Confirmed live: without this, state.json/state.md silently never got
    // written at all if the update was still in flight at exit. Collected
    // here and drained just before returning, rather than awaited inline
    // per turn, so a slow state-update still doesn't delay the next prompt.
    let mut pending_state_updates: Vec<tokio::task::JoinHandle<()>> = Vec::new();

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
            Ok(turn1) => {
                if let Some(rewritten) = &turn1.rewritten_history {
                    history = rewritten.clone();
                }
                history.push(ChatMessage::text("assistant", turn1.reply.clone()));

                if let Some(thinking) = &turn1.thinking {
                    println!("\n🧠 {}", to_plain_text(thinking));
                }
                println!("\n{}", to_plain_text(&turn1.reply));

                // Turn 2: dispatch + state-update, run only now that the
                // reply above is already shown -- see chat_turn.rs's module
                // doc comment. Blocking here is fine for a terminal (no
                // "thinking" indicator to manage the way the GUI has).
                let outcome = chat_turn::run_turn_followup(
                    &cfg,
                    &session_id,
                    persona_content.as_deref(),
                    text,
                    &turn1.reply,
                )
                .await;
                pending_state_updates.push(outcome.state_update_handle);
                if let Some(name) = &outcome.ruleset_loaded {
                    println!("\n[loaded ruleset: {name}]");
                }
                if let Some(err) = &outcome.ruleset_error {
                    println!("\n[{err}]");
                }
                if let Some(fields) = &outcome.image_prompt_requested {
                    println!("\n[requested an image -- generating now, this may take a while...]");
                    match comfyui::load_or_init() {
                        Ok(comfy_cfg) => {
                            match chat_turn::run_full_image_generation(
                                &cfg,
                                &comfy_cfg,
                                &session_id,
                                fields,
                            )
                            .await
                            {
                                // Doesn't render -- this is a terminal, not a
                                // GUI -- just the saved location, to check by
                                // hand afterward.
                                Ok(result) => {
                                    println!("[image saved to: {}]", result.path.display());
                                    if let Some(handle) = result.state_update_handle {
                                        pending_state_updates.push(handle);
                                    }
                                    // A missing reaction isn't necessarily a
                                    // failure -- `Never` never asks at all,
                                    // and `Optional` can legitimately decide
                                    // not to comment. Only `Always` coming
                                    // back empty is an actual problem worth
                                    // pointing at the log.
                                    match (&result.reaction, comfy_cfg.reaction_mode) {
                                        (Some(reaction), _) => {
                                            println!("\n{}", to_plain_text(reaction))
                                        }
                                        (None, comfyui::ReactionMode::Never) => {}
                                        (None, comfyui::ReactionMode::Optional) => {
                                            println!(
                                                "[no reaction this time -- the persona decided not to comment, or the request failed; see the log]"
                                            );
                                        }
                                        (None, comfyui::ReactionMode::Always) => {
                                            println!("[reaction turn failed -- see the log]");
                                        }
                                    }
                                }
                                Err(e) => println!("[image generation failed: {e}]"),
                            }
                        }
                        Err(e) => println!("[could not load ComfyUI config: {e}]"),
                    }
                }
                if let Some(query) = &outcome.web_search_requested {
                    println!("\n[searching the web for: {query}...]");
                    match searxng::load_or_init() {
                        Ok(searxng_cfg) => {
                            match chat_turn::run_full_web_search(
                                &cfg,
                                &searxng_cfg,
                                &session_id,
                                query,
                            )
                            .await
                            {
                                Ok(result) => {
                                    // The search itself failing still gets
                                    // an answer turn (an in-character
                                    // apology) rather than short-circuiting
                                    // -- see `search_error`'s doc comment.
                                    match &result.search_error {
                                        Some(err) => println!("[search itself failed: {err}]"),
                                        None => {
                                            println!("[{} result(s) found]", result.results.len());
                                            for r in &result.results {
                                                println!("  - {} ({})", r.title, r.url);
                                            }
                                        }
                                    }
                                    match &result.answer {
                                        Some(answer) => println!("\n{}", to_plain_text(answer)),
                                        None => println!("[answer turn failed -- see the log]"),
                                    }
                                    if let Some(handle) = result.state_update_handle {
                                        pending_state_updates.push(handle);
                                    }
                                }
                                Err(e) => println!("[web search failed: {e}]"),
                            }
                        }
                        Err(e) => println!("[could not load SearXNG config: {e}]"),
                    }
                }
                if let Some(summary) = &turn1.summary {
                    eprintln!(
                        "\n[summarized {} old message(s) to fit the context budget]\n{summary}",
                        turn1.summarized
                    );
                }
                if turn1.dropped > 0 {
                    eprintln!(
                        "\n[dropped {} old message(s) to fit the context budget]",
                        turn1.dropped
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

    // See `pending_state_updates`'s doc comment at the top of this
    // function -- without this drain, any state-update still in flight
    // when stdin closes gets silently killed along with the process.
    for handle in pending_state_updates {
        let _ = handle.await;
    }
    0
}
