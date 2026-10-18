//! `llm-assistant --persona-chat [--persona <name>] [--session <id>]` --
//! chat mode's own terminal REPL, entirely separate from operation mode's
//! `llm-assistant <folder> --chat` (`headless.rs`): no folder, no sandbox,
//! no shell commands, ever. It drives the same library functions as the GUI
//! (`chat_turn`), and history always lives on disk, so nothing here can
//! overwrite what a follow-up wrote. A decided photo is rendered too; a
//! terminal can't show it, so the saved path is printed.
//!
//! Also handles `--list-personas` and `--list-sessions`, small utility
//! flags for finding out what to pass to `--persona`/`--session`.

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
    let (meta, history) = match chat_session::load_session(&session_id) {
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
    println!(
        "Type a message (or /pic [hint] for a photo) and press Enter. Ctrl+D or Ctrl+C to exit."
    );

    // Replay scrollback when resuming an existing session.
    for m in &history {
        match m.role.as_str() {
            "user" => println!("\n>>> {}", m.content),
            "assistant" => {
                if let Some(t) = &m.thinking {
                    println!("\n🧠 {}", to_plain_text(t));
                }
                println!("\n{}", to_plain_text(&m.content));
                for path in &m.generated_images {
                    println!("[image: {path}]");
                }
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

        // Hot-reload, matching every other long-running entry point: a
        // config.toml edited mid-session takes effect on the next turn.
        if let Ok(reloaded) = config::load_or_init() {
            cfg = reloaded;
        }
        run_turn(&cfg, &session_id, text).await;
    }

    // A short-lived process would otherwise exit before the background state
    // update finishes, silently dropping it.
    chat_turn::flush_background(&session_id).await;
    0
}

/// One user message, then its follow-ups, printed in order.
async fn run_turn(cfg: &config::AppConfig, session_id: &str, text: &str) {
    let input = chat_turn::SendInput {
        session_id: session_id.to_string(),
        text: text.to_string(),
        images: Vec::new(),
    };
    let turn1 = match chat_turn::send_user_message(cfg, input, |_| {}).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("\nerror: {e}");
            return;
        }
    };
    if let Some(thinking) = &turn1.thinking {
        println!("\n🧠 {}", to_plain_text(thinking));
    }
    println!("\n{}", to_plain_text(&turn1.reply));
    if let Some(summary) = &turn1.summary {
        eprintln!("\n[summarized old messages to fit the context budget]\n{summary}");
    }
    if turn1.dropped > 0 {
        eprintln!(
            "\n[dropped {} old message(s) to fit the context budget]",
            turn1.dropped
        );
    }
    if turn1.assistant_message_id.is_empty() {
        return;
    }

    let plan = match chat_turn::run_followup(
        cfg,
        session_id,
        &turn1.assistant_message_id,
        turn1.photo_forced,
        turn1.photo_hint.clone(),
    )
    .await
    {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("\n[follow-up failed: {e}]");
            return;
        }
    };

    if let Some(image) = &plan.image {
        println!("\n[sending a photo -- generating now, this may take a while...]");
        match chat_turn::generate_image(
            cfg,
            session_id,
            &turn1.assistant_message_id,
            &image.fields,
            None,
        )
        .await
        {
            Ok(result) => {
                println!("[image saved to: {}]", result.image_path);
                if let Some(reaction) = &result.reaction {
                    println!("\n{}", to_plain_text(&reaction.text));
                }
            }
            Err(e) => println!("[image generation failed: {e}]"),
        }
    }
    if let Some(query) = &plan.search_query {
        println!("\n[searching the web for: {query}...]");
        match chat_turn::web_search(cfg, session_id, query).await {
            Ok(result) => {
                println!("[{}]", result.results_summary.replace('\n', "\n "));
                match &result.answer {
                    Some(answer) => println!("\n{}", to_plain_text(&answer.text)),
                    None => println!(
                        "[no answer -- the results didn't help, or the answer turn failed]"
                    ),
                }
            }
            Err(e) => println!("[web search failed: {e}]"),
        }
    }
}
