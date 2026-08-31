#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chat_log;
mod config;
mod context;
mod headless;
mod llm;
mod memory;
mod paths;
mod rules;
mod sandbox;

use config::{AppConfig, GrantedPath};
use llm::ChatMessage;
use simplelog::{
    ColorChoice, CombinedLogger, Config as LogConfig, LevelFilter, TermLogger, TerminalMode,
    WriteLogger,
};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;

struct AppState {
    root: Mutex<Option<PathBuf>>,
    /// So `stop_generation` can actually cancel the request, not just stop
    /// the UI proposing further steps.
    current_send: Mutex<Option<tokio::task::AbortHandle>>,
}

fn require_root(state: &State<AppState>) -> Result<PathBuf, String> {
    state
        .root
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "No folder selected yet".to_string())
}

fn shim_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| sandbox::default_shim_dir())
        .join("shims")
}

/// Shared by the picker, CLI arg, and headless: `.temp-trash` must exist.
fn activate_root(root: &std::path::Path) -> Result<(), String> {
    fs::create_dir_all(root.join(".temp-trash")).map_err(|e| e.to_string())
}

/// `llm-assistant /some/folder` starts with that folder already open.
fn resolve_cli_root() -> Option<PathBuf> {
    let arg = std::env::args().nth(1)?;
    let path = PathBuf::from(&arg);
    if path.is_dir() {
        Some(path)
    } else {
        log::warn!("ignoring CLI argument {arg:?}: not a directory");
        None
    }
}

fn init_logging() {
    let log_dir = paths::app_log_dir();
    let _ = fs::create_dir_all(&log_dir);
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("app.log"));

    // Stderr, not Mixed: headless prints its result to stdout.
    let term = TermLogger::new(
        LevelFilter::Info,
        LogConfig::default(),
        TerminalMode::Stderr,
        ColorChoice::Auto,
    );

    match log_file {
        Ok(file) => {
            let file_logger = WriteLogger::new(LevelFilter::Debug, LogConfig::default(), file);
            let _ = CombinedLogger::init(vec![term, file_logger]);
            log::info!("logging to {}", log_dir.join("app.log").display());
        }
        Err(e) => {
            let _ = CombinedLogger::init(vec![term]);
            log::warn!("could not open log file in {}: {e}", log_dir.display());
        }
    }
}

/// Uses the non-blocking `pick_folder` callback API on purpose:
/// `blocking_pick_folder()` inside a Tauri command deadlocks the picker.
async fn pick_folder_path(app: &AppHandle) -> Result<Option<PathBuf>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |folder| {
        let _ = tx.send(folder);
    });
    let picked = rx.await.map_err(|e| {
        log::error!("pick_folder_path: picker channel closed unexpectedly: {e}");
        "Folder picker closed unexpectedly".to_string()
    })?;
    Ok(picked.map(|p| PathBuf::from(p.to_string())))
}

#[tauri::command]
async fn pick_and_set_root(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    log::info!("pick_and_set_root: opening folder picker");
    let Some(root) = pick_folder_path(&app).await? else {
        log::info!("pick_and_set_root: user cancelled the picker");
        return Err("No folder selected".into());
    };

    if !root.is_dir() {
        log::warn!(
            "pick_and_set_root: selected path is not a directory: {}",
            root.display()
        );
        return Err("Selected path is not a directory".into());
    }

    activate_root(&root)?;
    *state.root.lock().unwrap() = Some(root.clone());
    log::info!("pick_and_set_root: root set to {}", root.display());

    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "root": root.display().to_string(), "config": cfg }))
}

/// Picker for the "grant a readable path" flow; doesn't touch app state.
#[tauri::command]
async fn pick_granted_path(app: AppHandle) -> Result<Option<String>, String> {
    log::info!("pick_granted_path: opening folder picker");
    Ok(pick_folder_path(&app)
        .await?
        .map(|p| p.display().to_string()))
}

/// So the frontend can pick up a CLI-preloaded root without the picker.
#[tauri::command]
fn get_current_root(state: State<AppState>) -> Option<String> {
    state
        .root
        .lock()
        .unwrap()
        .as_ref()
        .map(|p| p.display().to_string())
}

/// Back to plain chat with no file access; `run_command` then refuses until
/// a new folder is picked. Returns what was unmounted, for the UI.
#[tauri::command]
fn unmount_root(state: State<AppState>) -> Option<String> {
    let old = state.root.lock().unwrap().take();
    if let Some(root) = &old {
        log::info!("unmount_root: cleared root {}", root.display());
    }
    old.map(|p| p.display().to_string())
}

#[tauri::command]
fn load_config() -> Result<AppConfig, String> {
    config::load_or_init().map_err(|e| e.to_string())
}

#[tauri::command]
fn save_config(cfg: AppConfig) -> Result<(), String> {
    log::info!("save_config: endpoint={} model={}", cfg.endpoint, cfg.model);
    config::save(&cfg).map_err(|e| e.to_string())
}

/// Tests what's typed in the dialog, not what's saved. A real completion
/// rather than a `/models` ping, since the failures worth catching are the
/// ones reachability misses: wrong model name, rejected key, wrong URL path.
#[tauri::command]
async fn test_connection(
    endpoint: String,
    model: String,
    api_key: String,
) -> Result<String, String> {
    log::info!("test_connection: endpoint={endpoint} model={model}");
    let probe = vec![ChatMessage {
        role: "user".into(),
        content: "Reply with the single word: ok".into(),
    }];
    match llm::send_chat(&endpoint, &model, &api_key, 0.0, &probe).await {
        Ok(reply) => {
            let reply = reply.trim();
            log::info!("test_connection: ok, model replied {reply:?}");
            // Any reply means URL, auth, model name and response shape work.
            Ok(if reply.is_empty() {
                "Connected, but the model returned an empty reply.".into()
            } else {
                format!("Connected. {model} replied: {}", first_line(reply))
            })
        }
        Err(e) => {
            log::warn!("test_connection failed: {e}");
            Err(e.to_string())
        }
    }
}

/// One line for the dialog; the full reply is in `app.log`.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > 80 {
        format!("{}…", line.chars().take(80).collect::<String>())
    } else {
        line.to_string()
    }
}

#[tauri::command]
fn default_system_prompt() -> &'static str {
    config::DEFAULT_SYSTEM_PROMPT
}

#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[tauri::command]
fn load_general_rules() -> Result<String, String> {
    rules::load_general_or_init().map_err(|e| e.to_string())
}

#[tauri::command]
fn save_general_rules(rules: String) -> Result<(), String> {
    log::info!("save_general_rules: {} bytes", rules.len());
    rules::save_general(&rules).map_err(|e| e.to_string())
}

#[tauri::command]
fn default_general_rules() -> &'static str {
    rules::DEFAULT_GENERAL_RULES
}

#[tauri::command]
fn load_command_rules() -> Result<String, String> {
    rules::load_command_or_init().map_err(|e| e.to_string())
}

#[tauri::command]
fn save_command_rules(rules: String) -> Result<(), String> {
    log::info!("save_command_rules: {} bytes", rules.len());
    rules::save_command(&rules).map_err(|e| e.to_string())
}

#[tauri::command]
fn default_command_rules() -> &'static str {
    rules::DEFAULT_COMMAND_RULES
}

/// Mirrors what the GUI shows, thinking steps included, into this session's
/// `logs/chat-*.log`.
#[tauri::command]
fn append_chat_log(text: String) -> Result<(), String> {
    chat_log::append(&text).map_err(|e| e.to_string())
}

/// `session_approved` is the frontend's fade-out list (`confirm_fade_after`).
/// Evaluated here through the same `is_auto_approved` rather than in JS: a
/// second copy of its metacharacter rule would quietly stop matching.
#[tauri::command]
fn classify_command(
    cmd: String,
    session_approved: Vec<String>,
) -> Result<serde_json::Value, String> {
    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    let mut allowed = cfg.auto_approve.clone();
    allowed.extend(session_approved);
    Ok(serde_json::json!({
        "classification": sandbox::classify_command(&cmd),
        "auto_approved": sandbox::is_auto_approved(&cmd, &allowed),
    }))
}

#[tauri::command]
fn run_command(
    app: AppHandle,
    state: State<AppState>,
    cmd: String,
) -> Result<serde_json::Value, String> {
    let root = require_root(&state)?;
    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    log::info!("run_command: root={} cmd={:?}", root.display(), cmd);
    let shims = shim_dir(&app);
    sandbox::ensure_shims(&shims).map_err(|e| e.to_string())?;
    let outcome = sandbox::run_sandboxed(&root, &shims, &cfg.granted_paths, &cmd)
        .map_err(|e| e.to_string())?;
    // Here, not the frontend: the only place with both the command and its
    // real exit code.
    memory::record_command(&cmd, outcome.exit_code);
    log::debug!(
        "run_command: exit={} stdout={} bytes stderr={} bytes",
        outcome.exit_code,
        outcome.stdout.len(),
        outcome.stderr.len()
    );
    Ok(serde_json::json!({
        "stdout": outcome.stdout,
        "stderr": outcome.stderr,
        "exit_code": outcome.exit_code,
    }))
}

#[tauri::command]
fn add_granted_path(
    path: String,
    note: String,
    read_write: bool,
    recursive: bool,
) -> Result<AppConfig, String> {
    let mut cfg = config::load_or_init().map_err(|e| e.to_string())?;
    log::info!("add_granted_path: {path} (rw={read_write}, recursive={recursive}) -- {note}");
    cfg.granted_paths.push(GrantedPath {
        path,
        note,
        read_write,
        recursive,
    });
    config::save(&cfg).map_err(|e| e.to_string())?;
    Ok(cfg)
}

#[tauri::command]
fn remove_granted_path(path: String) -> Result<AppConfig, String> {
    let mut cfg = config::load_or_init().map_err(|e| e.to_string())?;
    log::info!("remove_granted_path: {path}");
    cfg.granted_paths.retain(|g| g.path != path);
    config::save(&cfg).map_err(|e| e.to_string())?;
    Ok(cfg)
}

#[tauri::command]
fn add_auto_approve(binary: String) -> Result<AppConfig, String> {
    let mut cfg = config::load_or_init().map_err(|e| e.to_string())?;
    if !cfg.auto_approve.iter().any(|b| b == &binary) {
        log::info!("add_auto_approve: {binary}");
        cfg.auto_approve.push(binary);
    }
    config::save(&cfg).map_err(|e| e.to_string())?;
    Ok(cfg)
}

#[tauri::command]
fn remove_auto_approve(binary: String) -> Result<AppConfig, String> {
    let mut cfg = config::load_or_init().map_err(|e| e.to_string())?;
    log::info!("remove_auto_approve: {binary}");
    cfg.auto_approve.retain(|b| b != &binary);
    config::save(&cfg).map_err(|e| e.to_string())?;
    Ok(cfg)
}

/// More than the reply, so the UI can report trimming rather than let it
/// happen silently.
#[derive(serde::Serialize)]
struct SendMessageResult {
    reply: String,
    dropped: usize,
    /// Reported separately from `dropped`: nothing factual was lost.
    condensed: usize,
    summarized: usize,
    /// Handed back so the UI can show and edit it -- a model-written record
    /// nobody sees is the risk this feature carries.
    summary: Option<String>,
    /// For the frontend to adopt as its new history.
    rewritten_history: Option<Vec<ChatMessage>>,
    estimated_tokens: usize,
}

/// Works with no folder open; the system prompt says which it is.
#[tauri::command]
async fn send_message(
    state: State<'_, AppState>,
    history: Vec<ChatMessage>,
) -> Result<SendMessageResult, String> {
    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    let general_rules = rules::load_general_or_init().map_err(|e| e.to_string())?;
    let command_rules = rules::load_command_or_init().map_err(|e| e.to_string())?;
    let root = state.root.lock().unwrap().clone();
    log::info!(
        "send_message: endpoint={} model={} root={:?} history_len={}",
        cfg.endpoint,
        cfg.model,
        root,
        history.len()
    );

    let system_content =
        rules::build_system_content(&cfg, &general_rules, &command_rules, root.as_deref());

    // The system block is never trimmed -- it's the contract the app's own
    // parsing depends on.
    let summarizer = cfg.summarize_before_dropping.then(|| context::Summarizer {
        endpoint: &cfg.endpoint,
        model: &cfg.model,
        api_key: &cfg.api_key,
    });
    let trimmed = context::fit_to_budget(
        context::estimate_tokens(&system_content),
        history,
        cfg.max_context_tokens as usize,
        summarizer,
    )
    .await;
    if trimmed.condensed > 0 {
        log::info!(
            "send_message: condensed {} finished step(s) to command + output",
            trimmed.condensed
        );
    }
    if let Some(summary) = &trimmed.summary {
        // In full: it becomes the record, so it must be checkable later.
        log::warn!(
            "send_message: summarized {} old message(s) into:\n{summary}",
            trimmed.summarized
        );
    }
    if trimmed.dropped > 0 {
        log::warn!(
            "send_message: dropped {} old message(s) to fit ~{} token budget (~{} sent)",
            trimmed.dropped,
            cfg.max_context_tokens,
            trimmed.estimated_tokens
        );
    } else {
        log::debug!(
            "send_message: ~{} estimated tokens",
            trimmed.estimated_tokens
        );
    }
    let dropped = trimmed.dropped;
    let condensed = trimmed.condensed;
    let summarized = trimmed.summarized;
    let summary = trimmed.summary;
    let rewritten_history = trimmed.rewritten_history;
    let estimated_tokens = trimmed.estimated_tokens;

    let mut messages = vec![ChatMessage {
        role: "system".into(),
        content: system_content,
    }];
    messages.extend(trimmed.messages);

    // Spawned, not awaited, so `stop_generation` has something to abort.
    let handle = tokio::spawn(async move {
        llm::send_chat(
            &cfg.endpoint,
            &cfg.model,
            &cfg.api_key,
            cfg.temperature,
            &messages,
        )
        .await
    });
    *state.current_send.lock().unwrap() = Some(handle.abort_handle());
    let result = handle.await;
    *state.current_send.lock().unwrap() = None;

    match result {
        Ok(Ok(reply)) => {
            log::debug!("send_message: reply {} bytes", reply.len());
            Ok(SendMessageResult {
                reply,
                dropped,
                condensed,
                summarized,
                summary,
                rewritten_history,
                estimated_tokens,
            })
        }
        Ok(Err(e)) => {
            log::error!("send_message failed: {e}");
            Err(e.to_string())
        }
        Err(join_err) if join_err.is_cancelled() => {
            log::info!("send_message: cancelled by user");
            Err("Cancelled".to_string())
        }
        Err(join_err) => {
            log::error!("send_message task panicked: {join_err}");
            Err(join_err.to_string())
        }
    }
}

/// A new top-level user message is this app's task boundary.
#[tauri::command]
fn start_memory_task(state: State<AppState>, message: String) {
    let root = state.root.lock().unwrap().clone();
    memory::start_task(root.as_deref(), &message);
}

/// Separate from `run_command` because these have no exit code or output.
#[tauri::command]
fn record_blocked_command(cmd: String, why: String) {
    memory::record_blocked(&cmd, &why);
}

#[tauri::command]
fn stop_generation(state: State<AppState>) -> bool {
    if let Some(handle) = state.current_send.lock().unwrap().take() {
        handle.abort();
        log::info!("stop_generation: aborted in-flight request");
        true
    } else {
        false
    }
}

fn print_help() {
    println!(
        "llm-assistant {} -- chat-driven local file assistant, sandboxed to a chosen folder",
        env!("CARGO_PKG_VERSION")
    );
    println!("\nUSAGE:");
    let usage: [(&str, &str); 4] = [
        ("llm-assistant", "Launch the GUI, no folder preloaded"),
        (
            "llm-assistant <folder>",
            "Launch the GUI with <folder> already open",
        ),
        (
            "llm-assistant <folder> <message>",
            "Headless: run one turn against <folder>, print the result, and exit -- no GUI",
        ),
        ("llm-assistant --help | -h", "Show this help"),
    ];
    for (cmd, desc) in usage {
        println!("    {cmd:<34} {desc}");
    }
    println!("\nConfig, rules, and logs live under $XDG_CONFIG_HOME/llm-assistant");
    println!("(usually ~/.config/llm-assistant):");
    let files: [(&str, &str); 4] = [
        (
            "config.toml",
            "endpoint, model, temperature, granted paths, ...",
        ),
        (
            "rules.md",
            "the working rules read before your system prompt",
        ),
        ("logs/app.log", "internal debug log"),
        (
            "logs/chat-*.log",
            "mirror of the GUI conversation, one per launch, up to 5 kept",
        ),
    ];
    for (name, desc) in files {
        println!("    {name:<18} {desc}");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().skip(1).any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }

    init_logging();
    log::info!(
        "LLM Assistant starting, config dir = {}",
        paths::app_config_dir().display()
    );
    // Once at startup, so app.log shows what's in effect without spamming.
    let startup_cfg = config::load_or_init().unwrap_or_default();
    rules::log_loaded_rules(startup_cfg.disable_builtin_rules);

    // Before the headless dispatch: a headless run is a session too.
    match memory::init() {
        Ok(path) => log::info!("session memory: {}", path.display()),
        Err(e) => log::warn!("failed to start session memory: {e}"),
    }

    // `<folder> <message...>` runs headless; `<folder>` alone preloads the GUI.
    if args.len() >= 3 {
        let root = PathBuf::from(&args[1]);
        if root.is_dir() {
            let message = args[2..].join(" ");
            log::info!("headless mode: root={} message={message:?}", root.display());
            headless::run(root, message);
        }
        log::warn!("ignoring CLI arguments: {:?} is not a directory", args[1]);
    }

    match chat_log::init() {
        Ok(path) => log::info!("chat log for this session: {}", path.display()),
        Err(e) => log::warn!("failed to start a new chat log: {e}"),
    }

    let cli_root = resolve_cli_root();
    if let Some(root) = &cli_root {
        if let Err(e) = activate_root(root) {
            log::warn!(
                "failed to activate CLI-provided root {}: {e}",
                root.display()
            );
        } else {
            log::info!("preloaded root from CLI argument: {}", root.display());
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            root: Mutex::new(cli_root),
            current_send: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            pick_and_set_root,
            pick_granted_path,
            get_current_root,
            unmount_root,
            load_config,
            save_config,
            default_system_prompt,
            test_connection,
            app_version,
            load_general_rules,
            save_general_rules,
            default_general_rules,
            load_command_rules,
            save_command_rules,
            default_command_rules,
            classify_command,
            run_command,
            add_granted_path,
            remove_granted_path,
            add_auto_approve,
            remove_auto_approve,
            send_message,
            stop_generation,
            start_memory_task,
            record_blocked_command,
            append_chat_log,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
