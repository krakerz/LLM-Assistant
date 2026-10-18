//! Two non-GUI ways to drive the file-ops agent from a terminal, both on
//! top of `agent::run_task`:
//!
//! - `llm-assistant <folder> <message>` -- one task, printed to stdout, then
//!   the process exits. Anything needing confirmation is reported and denied
//!   (nothing runs unattended that the GUI wouldn't have run on its own).
//! - `llm-assistant <folder> --chat` -- an interactive REPL (`>>> ` prompt),
//!   one conversation across lines, confirmations asked on stdin
//!   (`y` / `N` / `a` = always allow). Ends on Ctrl+D or Ctrl+C.

use crate::agent::{
    self, AgentEvent, AgentHost, AgentSession, BoxFuture, Cancel, ConfirmRequest, Decision,
    DoneReason,
};
use crate::config::{self, AppConfig};
use crate::rules::to_plain_text;
use crate::sandbox;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

/// Result lines shown per tool call; the model still got all of it.
const RESULT_LINES: usize = 40;

struct CliHost {
    /// REPL: ask on stdin. One-shot: deny.
    interactive: bool,
}

fn print_result(output: &str, ok: bool) {
    let lines: Vec<&str> = output.lines().collect();
    if !ok {
        println!("    [error]");
    }
    for line in lines.iter().take(RESULT_LINES) {
        println!("    {line}");
    }
    if lines.len() > RESULT_LINES {
        println!("    [... {} more lines]", lines.len() - RESULT_LINES);
    }
}

impl AgentHost for CliHost {
    fn emit(&self, ev: AgentEvent) {
        match ev {
            AgentEvent::Thinking { .. } | AgentEvent::Confirm { .. } | AgentEvent::Done { .. } => {}
            AgentEvent::AssistantText { text, .. } => println!("\n{}", to_plain_text(&text)),
            AgentEvent::ToolCall { title, .. } => println!("→ {title}"),
            AgentEvent::ToolResult { output, ok, .. } => print_result(&output, ok),
            AgentEvent::Notice { text } => println!("[{text}]"),
            AgentEvent::ManualCommand { command } => {
                println!("\n[this needs administrator rights, which the sandbox never has -- run it yourself:]");
                println!("    {command}");
            }
            AgentEvent::ChangedFiles { files } => {
                println!("\nChanged files:");
                for f in files {
                    match f.from {
                        Some(from) => println!("  {}: {from} -> {}", f.change, f.path),
                        None => println!("  {}: {}", f.change, f.path),
                    }
                }
            }
            AgentEvent::Error { message } => eprintln!("error: {message}"),
        }
        let _ = io::stdout().flush();
    }

    fn confirm<'a>(&'a self, req: ConfirmRequest) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            if !self.interactive {
                println!("[needs confirmation: {}] -- not run (headless)", req.title);
                return Decision::Deny;
            }
            println!("\n{}", req.title);
            match (&req.command, &req.diff) {
                (Some(cmd), _) => println!("    {cmd}"),
                (None, Some(diff)) => println!("{diff}"),
                _ => {}
            }
            let choices = if req.can_always_allow {
                "[y/N/a]"
            } else {
                "[y/N]"
            };
            print!("run it? {choices} ");
            let _ = io::stdout().flush();
            let mut answer = String::new();
            if io::stdin().lock().read_line(&mut answer).unwrap_or(0) == 0 {
                return Decision::Deny;
            }
            match answer.trim().to_lowercase().as_str() {
                "y" | "yes" => Decision::Approve,
                "a" | "always" if req.can_always_allow => Decision::AlwaysAllow,
                _ => Decision::Deny,
            }
        })
    }
}

fn load_config() -> Option<AppConfig> {
    match config::load_or_init() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("error loading config: {e}");
            None
        }
    }
}

/// The folder the agent will be confined to must be one we allow at all.
fn check_root(root: &std::path::Path) {
    if let Err(msg) = sandbox::check_root_allowed(root) {
        eprintln!("llm-assistant: {msg}");
        std::process::exit(2);
    }
}

pub fn run(root: PathBuf, message: String) -> ! {
    check_root(&root);
    let exit_code = tokio::runtime::Runtime::new()
        .expect("failed to start async runtime")
        .block_on(run_async(root, message));
    std::process::exit(exit_code);
}

pub fn run_chat(root: PathBuf) -> ! {
    check_root(&root);
    let exit_code = tokio::runtime::Runtime::new()
        .expect("failed to start async runtime")
        .block_on(run_chat_async(root));
    std::process::exit(exit_code);
}

async fn run_async(root: PathBuf, message: String) -> i32 {
    let Some(cfg) = load_config() else {
        return 1;
    };
    let mut session = match AgentSession::new(&root, &cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let host = CliHost { interactive: false };
    let outcome = agent::run_task(&cfg, &mut session, message, &host, &Cancel::new()).await;
    if outcome.reason == DoneReason::Error {
        1
    } else {
        0
    }
}

async fn run_chat_async(root: PathBuf) -> i32 {
    let Some(cfg) = load_config() else {
        return 1;
    };
    let mut session = match AgentSession::new(&root, &cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    println!("llm-assistant -- chatting in {}", root.display());
    println!("Type a message and press Enter. Ctrl+D or Ctrl+C to exit.");

    let host = CliHost { interactive: true };
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
        // Hot-reload: config.toml edits mid-session apply to the next message.
        let cfg = config::load_or_init().unwrap_or_else(|_| cfg.clone());
        agent::run_task(
            &cfg,
            &mut session,
            message.to_string(),
            &host,
            &Cancel::new(),
        )
        .await;
    }
    0
}
