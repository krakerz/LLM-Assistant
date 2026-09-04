#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chat_cli;
mod chat_log;
mod chat_session;
mod chat_turn;
mod comfyui;
mod config;
mod context;
mod headless;
mod llm;
mod memory;
mod paths;
mod persona;
mod rules;
mod ruleset;
mod sandbox;
mod searxng;
mod server;

use config::{AppConfig, GrantedPath};
use llm::ChatMessage;
use simplelog::{
    ColorChoice, CombinedLogger, Config as LogConfig, ConfigBuilder as LogConfigBuilder,
    LevelFilter, TermLogger, TerminalMode, WriteLogger,
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

/// Local wall-clock time in every log line instead of simplelog's UTC
/// default -- friendlier to read at a glance against "when did I actually
/// send that message" than doing the offset math by hand. Built once and
/// shared by both loggers below so the terminal and the file never disagree
/// with each other. Falls back to UTC (simplelog's own default) if `time`'s
/// local-offset detection fails, which it can on some platforms in a
/// multi-threaded process -- not fatal, just less convenient, and the only
/// place this gets reported is `eprintln!` since the logger itself isn't
/// initialized yet at this point.
fn log_config() -> LogConfig {
    let mut builder = LogConfigBuilder::new();
    if builder.set_time_offset_to_local().is_err() {
        eprintln!("could not determine the local time zone -- log timestamps will be UTC");
    }
    builder.build()
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
        log_config(),
        TerminalMode::Stderr,
        ColorChoice::Auto,
    );

    match log_file {
        Ok(file) => {
            let file_logger = WriteLogger::new(LevelFilter::Debug, log_config(), file);
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
///
/// `.set_parent(&window)` ties the native GTK file chooser to the main
/// window -- without it, the window manager has no relationship between the
/// two, and the picker can end up opening behind the app instead of on top
/// of it.
async fn pick_folder_path(app: &AppHandle) -> Result<Option<PathBuf>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut builder = app.dialog().file();
    if let Some(window) = app.get_webview_window("main") {
        builder = builder.set_parent(&window);
    }
    builder.pick_folder(move |folder| {
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

/// Kept separate from `load_config`/`save_config` -- see `comfyui.rs`'s doc
/// comment for why this lives in its own file instead of `config.toml`.
#[tauri::command]
fn get_comfyui_config() -> Result<comfyui::ComfyUiConfig, String> {
    comfyui::load_or_init().map_err(|e| e.to_string())
}

#[tauri::command]
fn save_comfyui_config(cfg: comfyui::ComfyUiConfig) -> Result<(), String> {
    log::info!("save_comfyui_config: base_url={}", cfg.base_url);
    comfyui::save(&cfg).map_err(|e| e.to_string())
}

/// Same reasoning as `get_comfyui_config`/`save_comfyui_config` -- its own
/// file, not `config.toml`.
#[tauri::command]
fn get_searxng_config() -> Result<searxng::SearxngConfig, String> {
    searxng::load_or_init().map_err(|e| e.to_string())
}

#[tauri::command]
fn save_searxng_config(cfg: searxng::SearxngConfig) -> Result<(), String> {
    log::info!("save_searxng_config: base_url={}", cfg.base_url);
    searxng::save(&cfg).map_err(|e| e.to_string())
}

/// Picker for the ComfyUI output-directory setting; same
/// `pick_folder_path` helper `pick_granted_path` uses, doesn't touch app
/// state.
#[tauri::command]
async fn pick_comfyui_output_dir(app: AppHandle) -> Result<Option<String>, String> {
    Ok(pick_folder_path(&app)
        .await?
        .map(|p| p.display().to_string()))
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
    let probe = vec![ChatMessage::text("user", "Reply with the single word: ok")];
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

/// A short, content-addressed identifier for this exact build -- see
/// `build.rs`'s `build_hash()` doc comment for how it's computed. Shown
/// next to the version number in the Settings UI, so "is this actually the
/// build I just rebuilt" never has to be guessed from a binary's mtime.
#[tauri::command]
fn app_build_hash() -> &'static str {
    env!("BUILD_HASH")
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
    let scratch = cfg.memory_enabled.then(memory::temp_dir);
    let outcome =
        sandbox::run_sandboxed(&root, &shims, &cfg.granted_paths, scratch.as_deref(), &cmd)
            .map_err(|e| e.to_string())?;
    // Here, not the frontend: the only place with both the command and its
    // real exit code.
    memory::record_command(&cfg, &cmd, outcome.exit_code);
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

    let mut messages = vec![ChatMessage::text("system", system_content)];
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
fn start_memory_task(state: State<AppState>, message: String) -> Result<(), String> {
    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    let root = state.root.lock().unwrap().clone();
    memory::start_task(&cfg, root.as_deref(), &message);
    Ok(())
}

/// Separate from `run_command` because these have no exit code or output.
#[tauri::command]
fn record_blocked_command(cmd: String, why: String) -> Result<(), String> {
    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    memory::record_blocked(&cfg, &cmd, &why);
    Ok(())
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

// --- Chat mode: personas ---

#[tauri::command]
fn list_personas() -> Result<Vec<persona::PersonaSummary>, String> {
    persona::list_personas().map_err(|e| e.to_string())
}

/// Native file picker filtered to `.md`, for importing an existing persona.
/// Separate from `pick_granted_path`/`pick_folder_path`: those pick a
/// directory, this picks one file.
#[tauri::command]
async fn pick_persona_file(app: AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut builder = app.dialog().file().add_filter("Markdown", &["md"]);
    if let Some(window) = app.get_webview_window("main") {
        builder = builder.set_parent(&window);
    }
    builder.pick_file(move |file| {
        let _ = tx.send(file);
    });
    let picked = rx.await.map_err(|e| e.to_string())?;
    Ok(picked.map(|p| p.to_string()))
}

#[tauri::command]
fn import_persona(path: String) -> Result<persona::PersonaSummary, String> {
    persona::import_persona(std::path::Path::new(&path)).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_new_persona(name: String, content: String) -> Result<persona::PersonaSummary, String> {
    persona::save_new_persona(&name, &content).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_persona(name: String) -> Result<(), String> {
    persona::delete_persona(&name).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_persona_content(name: String) -> Result<String, String> {
    persona::load_persona(&name).map_err(|e| e.to_string())
}

#[tauri::command]
fn update_persona(name: String, content: String) -> Result<(), String> {
    persona::update_persona(&name, &content).map_err(|e| e.to_string())
}

// --- Chat mode: rulesets ---
//
// Editing only, not full CRUD like personas -- the two rulesets are seeded
// by `ruleset::list_rulesets` itself, so there's no "new"/"import"/"delete"
// to expose; this just lets the user edit their content without opening the
// `.md` files by hand.

#[tauri::command]
fn list_rulesets() -> Result<Vec<ruleset::RulesetSummary>, String> {
    ruleset::list_rulesets().map_err(|e| e.to_string())
}

#[tauri::command]
fn get_ruleset_content(name: String) -> Result<String, String> {
    ruleset::load_ruleset(&name).map_err(|e| e.to_string())
}

#[tauri::command]
fn update_ruleset(name: String, content: String) -> Result<(), String> {
    ruleset::update_ruleset(&name, &content).map_err(|e| e.to_string())
}

/// Backs the ruleset editor's "see an example" link -- `None` (and the
/// link stays hidden) for a ruleset with no example content of its own.
#[tauri::command]
fn get_ruleset_example(name: String) -> Option<String> {
    ruleset::example_for(&name).map(|s| s.to_string())
}

// --- Chat mode: sessions ---

#[tauri::command]
fn list_chat_sessions() -> Result<Vec<chat_session::SessionSummary>, String> {
    chat_session::list_sessions().map_err(|e| e.to_string())
}

#[tauri::command]
fn create_chat_session(persona: Option<String>) -> Result<chat_session::SessionSummary, String> {
    chat_session::create_session(persona.as_deref()).map_err(|e| e.to_string())
}

#[tauri::command]
fn load_chat_session(session_id: String) -> Result<serde_json::Value, String> {
    let (meta, history) = chat_session::load_session(&session_id).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "meta": meta, "history": history }))
}

#[tauri::command]
fn rename_chat_session(session_id: String, title: String) -> Result<(), String> {
    chat_session::rename_session(&session_id, &title).map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_chat_session(session_id: String) -> Result<(), String> {
    chat_session::delete_session(&session_id).map_err(|e| e.to_string())
}

/// Read-only peek at a session's current ` ```state ``` ` snapshot -- the
/// only writer is `chat_turn::run_chat_turn`'s turn-processing path, never
/// this command, so there's nothing to guard against here.
#[tauri::command]
fn get_chat_state(session_id: String) -> String {
    chat_session::read_state(&session_id)
}

/// Same as `get_chat_state`, but the raw `state.json` source of truth
/// instead of the derived `state.md` summary -- the quick-view dialog's
/// "raw" tab, since the compact summary alone can't show precise fields
/// without duplicating what the raw view already does.
#[tauri::command]
fn get_chat_raw_state(session_id: String) -> String {
    chat_session::read_raw_state(&session_id)
}

/// Keeps a session's title on `chat_session::DEFAULT_TITLE` from growing
/// unbounded -- the leading slice of the first message is plenty to
/// recognize a chat in the session list.
#[derive(serde::Serialize, Clone)]
struct SendChatMessageResult {
    reply: String,
    thinking: Option<String>,
    dropped: usize,
    condensed: usize,
    summarized: usize,
    summary: Option<String>,
    rewritten_history: Option<Vec<ChatMessage>>,
}

impl From<chat_turn::ChatTurnOutcome> for SendChatMessageResult {
    fn from(outcome: chat_turn::ChatTurnOutcome) -> Self {
        SendChatMessageResult {
            reply: outcome.reply,
            thinking: outcome.thinking,
            dropped: outcome.dropped,
            condensed: outcome.condensed,
            summarized: outcome.summarized,
            summary: outcome.summary,
            rewritten_history: outcome.rewritten_history,
        }
    }
}

/// Thin wrapper around `chat_turn::run_chat_turn` (turn 1 only -- see its
/// module doc comment), the logic shared with the `--persona-chat` CLI
/// (`chat_cli.rs`) -- this command just loads config and maps the error
/// type Tauri expects. Dispatch and state-update are a separate follow-up
/// call (`run_turn_followup` below) the frontend fires only after showing
/// this reply. Superseded in the GUI by `send_chat_message_streaming`
/// below -- kept as-is since `chat_cli.rs` still needs a plain,
/// non-streaming call.
#[tauri::command]
async fn send_chat_message(
    session_id: String,
    history: Vec<ChatMessage>,
) -> Result<SendChatMessageResult, String> {
    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    let outcome = chat_turn::run_chat_turn(&cfg, &session_id, history)
        .await
        .map_err(|e| e.to_string())?;
    Ok(outcome.into())
}

/// One event sent over `send_chat_message_streaming`'s channel per parsed
/// SSE chunk, plus a final `Done`/`Error` closing the stream -- mirrors
/// `server.rs`'s own `ChatStreamEvent` (independently defined, same
/// DTO-per-transport convention already used throughout this codebase),
/// so `ui/main.js` needs exactly one `onEvent` shape for both transports.
#[derive(serde::Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatStreamEvent {
    Content { text: String },
    Reasoning { text: String },
    Done { result: SendChatMessageResult },
    Error { message: String },
}

/// Streaming sibling of `send_chat_message` -- turn 1 only, same scope as
/// its module doc comment. The `Result` return only covers a failure
/// *before* streaming starts (config load); once `run_chat_turn_streaming`
/// is running, a failure becomes an `Error` channel event instead of an
/// `Err` return, since the frontend is already mid-stream by then and a
/// channel event is the only way left to tell it.
#[tauri::command]
async fn send_chat_message_streaming(
    session_id: String,
    history: Vec<ChatMessage>,
    channel: tauri::ipc::Channel<ChatStreamEvent>,
) -> Result<(), String> {
    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    let mut on_delta = |delta: llm::ChatDelta<'_>| {
        let event = match delta {
            llm::ChatDelta::Content(text) => ChatStreamEvent::Content {
                text: text.to_string(),
            },
            llm::ChatDelta::Reasoning(text) => ChatStreamEvent::Reasoning {
                text: text.to_string(),
            },
        };
        // The webview may already be gone (window closed mid-stream) --
        // the request still completes and persists normally either way,
        // there's just no one left to show it to.
        let _ = channel.send(event);
    };
    match chat_turn::run_chat_turn_streaming(&cfg, &session_id, history, &mut on_delta).await {
        Ok(outcome) => {
            let _ = channel.send(ChatStreamEvent::Done {
                result: outcome.into(),
            });
        }
        Err(e) => {
            let _ = channel.send(ChatStreamEvent::Error {
                message: e.to_string(),
            });
        }
    }
    Ok(())
}

/// Turn 2 as its own round-trip -- called by the frontend only after
/// `send_chat_message` has already returned and shown its reply, so neither
/// dispatch nor the state-update turn it also kicks off (detached, see
/// `chat_turn::run_turn_followup`'s doc comment) can add latency to that.
#[derive(serde::Serialize)]
struct TurnFollowupResult {
    ruleset_loaded: Option<String>,
    ruleset_error: Option<String>,
    image_prompt_requested: Option<comfyui::ImagePromptFields>,
    web_search_requested: Option<String>,
    /// Whether this turn spawned its own state-update turn -- known the
    /// instant it's spawned, not once it finishes (it's a detached
    /// background task, see `chat_turn::spawn_state_update`'s doc comment),
    /// so this is purely "a memory update was triggered for this turn," not
    /// "state has now actually changed." The GUI shows it as a small badge
    /// the moment this result comes back, same spirit as the old
    /// `state_updated` indicator but without waiting on anything.
    state_update_dispatched: bool,
}

impl From<chat_turn::TurnFollowupOutcome> for TurnFollowupResult {
    fn from(outcome: chat_turn::TurnFollowupOutcome) -> Self {
        Self {
            ruleset_loaded: outcome.ruleset_loaded,
            ruleset_error: outcome.ruleset_error,
            image_prompt_requested: outcome.image_prompt_requested,
            web_search_requested: outcome.web_search_requested,
            state_update_dispatched: outcome.state_update_handle.is_some(),
        }
    }
}

#[tauri::command]
async fn run_turn_followup(
    session_id: String,
    last_user_message: String,
    last_assistant_reply: String,
) -> Result<TurnFollowupResult, String> {
    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    let (meta, _) = chat_session::load_session(&session_id).map_err(|e| e.to_string())?;
    let persona_content = match &meta.persona {
        Some(name) => persona::load_persona(name).ok(),
        None => None,
    };
    Ok(chat_turn::run_turn_followup(
        &cfg,
        &session_id,
        persona_content.as_deref(),
        &last_user_message,
        &last_assistant_reply,
    )
    .await
    .into())
}

/// Settings' "Test image generation" button -- runs the real pipeline
/// (`comfyui::generate_image`) with a trivial fixed prompt against whatever
/// config is currently *typed* (not necessarily saved yet, same "tests what's
/// typed" contract as `test_connection`/`test_vision_support`), and returns
/// the actual resulting image as a `data:` URL rather than just a checkmark
/// -- this mapping is easy to get subtly wrong (a stale node id from a
/// previously-pasted workflow, a field mapped to the wrong node), so seeing
/// the real output matters more here than for the other two "Test" buttons.
#[tauri::command]
async fn test_comfyui_generation(cfg: comfyui::ComfyUiConfig) -> Result<String, String> {
    log::info!("test_comfyui_generation: base_url={}", cfg.base_url);
    let fields = comfyui::ImagePromptFields {
        positive: Some("a red circle on a white background".to_string()),
        ..Default::default()
    };
    let image = comfyui::generate_image(&cfg, &fields)
        .await
        .map_err(|e| e.to_string())?;
    let mime = if image.filename.to_lowercase().ends_with(".jpg")
        || image.filename.to_lowercase().ends_with(".jpeg")
    {
        "image/jpeg"
    } else if image.filename.to_lowercase().ends_with(".webp") {
        "image/webp"
    } else {
        "image/png"
    };
    Ok(format!(
        "data:{mime};base64,{}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &image.bytes)
    ))
}

/// The real image-generation path a chat turn's ` ```image-prompt``` `
/// request goes through, once `send_chat_message` has already returned the
/// (fast) text reply -- see `chat_turn::ChatTurnOutcome::image_prompt_requested`'s
/// doc comment for why this is a separate, later call rather than part of
/// that same turn.
///
/// Generation only -- no reaction (turn 3) -- so the GUI can show the
/// finished image the instant it's ready instead of waiting on a second,
/// separate LLM call it has no bearing on. `reaction_pending` tells the
/// caller whether it's worth following up with `run_image_reaction` at all;
/// `false` (`ReactionMode::Never`) means don't bother -- there's nothing
/// coming.
#[derive(serde::Serialize)]
struct GeneratedImageResult {
    path: String,
    data_url: String,
    reaction_pending: bool,
}

#[tauri::command]
async fn generate_comfyui_image(
    session_id: String,
    fields: comfyui::ImagePromptFields,
) -> Result<GeneratedImageResult, String> {
    let comfy_cfg = comfyui::load_or_init().map_err(|e| e.to_string())?;
    let image = chat_turn::generate_and_save_image(&comfy_cfg, &session_id, &fields)
        .await
        .map_err(|e| e.to_string())?;
    Ok(GeneratedImageResult {
        path: image.path.display().to_string(),
        data_url: image.data_url,
        reaction_pending: comfy_cfg.reaction_mode != comfyui::ReactionMode::Never,
    })
}

/// A turn 3/4 reply plus its reasoning, if any -- see
/// `chat_turn::TurnReply`'s doc comment. `thinking` is for the GUI's
/// "thinking" placeholder only, same as turn 1's; never persisted.
#[derive(serde::Serialize)]
struct TurnReplyResult {
    text: Option<String>,
    thinking: Option<String>,
    /// See `TurnFollowupResult::state_update_dispatched`'s doc comment --
    /// same meaning. `false` whenever `text` is `None` (nothing to update
    /// state from) or the reaction/answer turn itself failed outright.
    state_update_dispatched: bool,
}

impl From<chat_turn::TurnReply> for TurnReplyResult {
    fn from(reply: chat_turn::TurnReply) -> Self {
        Self {
            state_update_dispatched: reply.state_update_handle.is_some(),
            text: reply.text,
            thinking: reply.thinking,
        }
    }
}

/// Turn 3, as its own round-trip -- called separately, after
/// `generate_comfyui_image` already resolved, so the image shows up right
/// away and only the reaction (which can take a real chunk of time on its
/// own) sits behind a follow-up "thinking" indicator. `positive_prompt` and
/// `image_data_url` are exactly what `generate_comfyui_image`'s caller
/// already has on hand (the request's own prompt fields, that command's
/// `data_url`), so nothing needs to be persisted or looked up again to
/// bridge the two calls.
#[tauri::command]
async fn run_image_reaction(
    session_id: String,
    positive_prompt: String,
    image_data_url: String,
) -> Result<TurnReplyResult, String> {
    let comfy_cfg = comfyui::load_or_init().map_err(|e| e.to_string())?;
    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    Ok(chat_turn::run_and_persist_image_reaction(
        &cfg,
        &session_id,
        &positive_prompt,
        &image_data_url,
        comfy_cfg.reaction_mode,
    )
    .await
    .into())
}

/// Tests what's typed in the dialog, not what's saved -- same contract as
/// `test_comfyui_generation`. Returns the raw results so a wrong URL/API
/// key is visible immediately rather than only failing silently later.
#[tauri::command]
async fn test_searxng_search(
    cfg: searxng::SearxngConfig,
) -> Result<Vec<searxng::SearchResult>, String> {
    log::info!("test_searxng_search: base_url={}", cfg.base_url);
    searxng::search(&cfg, "test query")
        .await
        .map_err(|e| e.to_string())
}

/// The real web-search path a chat turn's ` ```web-search``` ` request goes
/// through, once `send_chat_message` has already returned -- same reasoning
/// as `generate_comfyui_image`.
///
/// Search only -- no answer (turn 4) -- same split as
/// `generate_comfyui_image`/`run_image_reaction`, and for the same reason:
/// the results are already final the moment the search itself returns, so
/// there's no reason to make them wait on a second LLM call too.
#[derive(serde::Serialize)]
struct WebSearchCommandResult {
    results: Vec<searxng::SearchResult>,
    /// The search itself failing (vs. succeeding with nothing relevant) --
    /// see `chat_turn::WebSearchResult`'s doc comment. Not shown to the
    /// user directly; the answer turn already covers that in character.
    search_error: Option<String>,
}

#[tauri::command]
async fn run_web_search(query: String) -> Result<WebSearchCommandResult, String> {
    let searxng_cfg = searxng::load_or_init().map_err(|e| e.to_string())?;
    let (results, search_error) = match searxng::search(&searxng_cfg, &query).await {
        Ok(results) => (results, None),
        Err(e) => {
            log::warn!("run_web_search: search itself failed: {e}");
            (Vec::new(), Some(e.to_string()))
        }
    };
    Ok(WebSearchCommandResult {
        results,
        search_error,
    })
}

/// Turn 4, as its own round-trip -- called separately, after `run_web_search`
/// already resolved, same reasoning as `run_image_reaction`. `results` and
/// `search_error` are exactly what `run_web_search`'s caller already has on
/// hand from that command's own result.
#[tauri::command]
async fn run_search_answer(
    session_id: String,
    query: String,
    results: Vec<searxng::SearchResult>,
    search_error: Option<String>,
) -> Result<TurnReplyResult, String> {
    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    Ok(chat_turn::run_and_persist_search_answer(
        &cfg,
        &session_id,
        &query,
        &results,
        search_error.as_deref(),
    )
    .await
    .into())
}

/// Redisplays an already-saved generated image -- reopening a session calls
/// this once per `ChatMessage.generated_images` entry.
#[tauri::command]
fn read_generated_image(path: String) -> Result<String, String> {
    comfyui::read_as_data_url(std::path::Path::new(&path)).map_err(|e| e.to_string())
}

/// Backs the image preview popup's "Save" button -- the image is already on
/// disk (under the configured output folder), this just copies it wherever
/// the user actually wants a copy. `Ok(None)` means the user cancelled the
/// dialog, not an error. Same non-blocking picker pattern as
/// `pick_folder_path`; `save_file` needs no `.pick_folder`-vs-`.pick_file`
/// distinction since it always returns a single destination.
#[tauri::command]
async fn save_generated_image_as(app: AppHandle, path: String) -> Result<Option<String>, String> {
    let source = std::path::PathBuf::from(&path);
    let default_name = source
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("image.png")
        .to_string();

    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut builder = app.dialog().file().set_file_name(&default_name);
    if let Some(window) = app.get_webview_window("main") {
        builder = builder.set_parent(&window);
    }
    builder.save_file(move |dest| {
        let _ = tx.send(dest);
    });
    let dest = rx.await.map_err(|e| {
        log::error!("save_generated_image_as: picker channel closed unexpectedly: {e}");
        "Save dialog closed unexpectedly".to_string()
    })?;
    let Some(dest) = dest else {
        return Ok(None);
    };
    let dest_path = std::path::PathBuf::from(dest.to_string());
    fs::copy(&source, &dest_path).map_err(|e| e.to_string())?;
    Ok(Some(dest_path.display().to_string()))
}

/// Passive, best-effort hint for whether the configured model supports
/// vision -- never authoritative (see `llm::probe_vision_capability`'s own
/// doc comment for why). `None` means neither known backend answered, not
/// "no vision."
#[tauri::command]
async fn probe_vision_capability(endpoint: String, model: String) -> Option<bool> {
    llm::probe_vision_capability(&endpoint, &model).await
}

/// A small, fixed, solid-red 2x2 PNG -- deliberately trivial for a model to
/// describe correctly, so a reply that doesn't mention red is as telling as
/// an outright error. Hand-built (raw scanlines + zlib + PNG chunk framing,
/// no image crate needed for something this small) and round-trip verified
/// -- decoded, decompressed, and pixel-checked -- before being embedded here.
pub(crate) const VISION_TEST_IMAGE_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEElEQVR42mP4z8AARAwQCgAf7gP9Y167WwAAAABJRU5ErkJggg==";

/// Sends the test image plus a one-word question, mirroring
/// `test_connection`'s honesty: the raw reply or the raw error, never a
/// guess dressed up as a verdict. A non-vision model/backend either errors
/// outright or answers without ever mentioning red -- both read clearly as
/// "no" without this trying to parse the reply itself beyond that one check.
#[tauri::command]
async fn test_vision_support(
    endpoint: String,
    model: String,
    api_key: String,
) -> Result<String, String> {
    log::info!("test_vision_support: endpoint={endpoint} model={model}");
    let mut probe = ChatMessage::text(
        "user",
        "What color is this image? Answer with just the color name.",
    );
    probe.images = vec![format!(
        "data:image/png;base64,{VISION_TEST_IMAGE_PNG_BASE64}"
    )];
    match llm::send_chat(&endpoint, &model, &api_key, 0.0, &[probe]).await {
        Ok(reply) => {
            let reply = reply.trim();
            log::info!("test_vision_support: model replied {reply:?}");
            if reply.to_lowercase().contains("red") {
                Ok(format!("Vision works. {model} correctly saw a red image."))
            } else if reply.is_empty() {
                Err("The model replied with nothing at all.".into())
            } else {
                Err(format!(
                    "The model replied but never said \"red\" -- it probably can't see the \
                     image. It said: {}",
                    first_line(reply)
                ))
            }
        }
        Err(e) => {
            log::warn!("test_vision_support failed: {e}");
            Err(e.to_string())
        }
    }
}

/// Finds `--flag <value>` anywhere in argv and returns `<value>`. Simple on
/// purpose -- chat mode's CLI has exactly two optional flags, not enough to
/// justify a real argument-parsing dependency.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn print_help() {
    println!(
        "llm-assistant {} -- chat-driven local file assistant, sandboxed to a chosen folder",
        env!("CARGO_PKG_VERSION")
    );
    println!("\nUSAGE:");
    let usage: [(&str, &str); 9] = [
        ("llm-assistant", "Launch the GUI, no folder preloaded"),
        (
            "llm-assistant <folder>",
            "Launch the GUI with <folder> already open",
        ),
        (
            "llm-assistant <folder> <message>",
            "Headless: run one turn against <folder>, print the result, and exit -- no GUI",
        ),
        (
            "llm-assistant <folder> --chat",
            "Interactive file-ops chat against <folder> -- no GUI, Ctrl+D/Ctrl+C to exit",
        ),
        (
            "llm-assistant --persona-chat [--persona <name>] [--session <id>]",
            "Chat mode's own CLI -- no folder, no shell commands, purely conversational",
        ),
        (
            "llm-assistant --list-personas",
            "List saved personas, for use with --persona",
        ),
        (
            "llm-assistant --list-sessions",
            "List chat mode sessions, for use with --session",
        ),
        (
            "llm-assistant --server [--bind <addr>] [--port <n>]",
            "Headless: serve chat mode only over HTTP -- no GUI, no folder, no file ops",
        ),
        ("llm-assistant --help | -h", "Show this help"),
    ];
    for (cmd, desc) in usage {
        println!("    {cmd:<50} {desc}");
    }
    println!("\nConfig, rules, and logs live under $XDG_CONFIG_HOME/llm-assistant");
    println!("(usually ~/.config/llm-assistant):");
    let files: [(&str, &str); 5] = [
        (
            "config.toml",
            "endpoint, model, temperature, granted paths, ...",
        ),
        (
            "rules.md",
            "the working rules read before your system prompt",
        ),
        (
            "server.json",
            "--server's bind/port defaults and password (empty = unauthenticated)",
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
    // WebKitGTK's DMABUF renderer draws a blank white window on many
    // driver/compositor combinations. It bites the AppImage hardest, which
    // ships its own WebKitGTK, so a working host copy doesn't save it. Set
    // before any GTK/WebKit init; an explicit value from the environment wins.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    let args: Vec<String> = std::env::args().collect();
    if args.iter().skip(1).any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }

    init_logging();
    log::info!(
        "LLM Assistant starting, version={} build={}, config dir = {}",
        env!("CARGO_PKG_VERSION"),
        env!("BUILD_HASH"),
        paths::app_config_dir().display()
    );

    // `--persona-chat` is chat mode's own CLI, entirely separate from
    // operation mode's folder-based dispatch below: no folder, ever, and
    // none of operation mode's rules/memory apply. Checked before the
    // memory-init step below, which is irrelevant noise for a pure
    // chat-mode invocation. (The general/command rules dump to app.log no
    // longer needs the same care -- `rules::build_system_content` only logs
    // it lazily, the first time operation mode's system prompt is actually
    // built, which a chat-only invocation never triggers regardless of
    // order.)
    if args.iter().any(|a| a == "--list-personas") {
        chat_cli::list_personas();
        return;
    }
    if args.iter().any(|a| a == "--list-sessions") {
        chat_cli::list_sessions();
        return;
    }
    if args.iter().any(|a| a == "--persona-chat") {
        let persona = flag_value(&args, "--persona");
        let session_id = flag_value(&args, "--session");
        log::info!("persona chat CLI: persona={persona:?} session={session_id:?}");
        chat_cli::run(chat_cli::Options {
            persona,
            session_id,
        });
    }

    // Same reasoning as `--persona-chat` above: chat mode only, headless,
    // no window ever created -- checked before operation mode's memory-init
    // noise. `--bind`/`--port` are ad hoc,
    // per-run overrides (same spirit as `--persona`/`--session` above),
    // never written back to `server.json`; the password is never a CLI
    // flag at all -- see `server::ServerConfig`'s doc comment for why --
    // only ever read from that file.
    if args.iter().any(|a| a == "--server") {
        let cfg = server::load_or_init().unwrap_or_default();
        let bind = flag_value(&args, "--bind").unwrap_or(cfg.bind);
        let port = flag_value(&args, "--port")
            .and_then(|p| p.parse().ok())
            .unwrap_or(cfg.port);
        let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        if let Err(e) = rt.block_on(server::run(bind, port, cfg.password)) {
            log::error!("web server exited with error: {e}");
        }
        return;
    }

    // Before the headless dispatch: a headless run is a session too. Not
    // gated on `memory_enabled` -- config is hot-reloaded, so the setting can
    // come on mid-session, and without a session dir the writes would fall
    // back to a shared one. Costs an empty directory when it stays off.
    match memory::init() {
        Ok(path) => log::info!("session memory: {}", path.display()),
        Err(e) => log::warn!("failed to start session memory: {e}"),
    }

    // `<folder> <message...>` runs headless; `<folder> --chat` starts an
    // interactive terminal session; `<folder>` alone preloads the GUI.
    if args.len() >= 3 {
        let root = PathBuf::from(&args[1]);
        if root.is_dir() {
            if args.len() == 3 && args[2] == "--chat" {
                log::info!("interactive chat mode: root={}", root.display());
                headless::run_chat(root);
            }
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
        .setup(|app| {
            // `bundle.icon` in tauri.conf.json only reaches the window/taskbar
            // icon once the app is actually installed via .deb/AppImage (it's
            // baked into the desktop entry Linux reads the icon from) -- a
            // plain dev binary run directly has no such entry, so the window
            // manager falls back to a generic default. Setting it here at
            // runtime works unconditionally, dev binary or installed package
            // alike, since it talks to the windowing backend directly rather
            // than relying on desktop-entry registration.
            if let Some(window) = app.get_webview_window("main") {
                match tauri::image::Image::from_bytes(include_bytes!("../icons/128x128.png")) {
                    Ok(icon) => {
                        if let Err(e) = window.set_icon(icon) {
                            log::warn!("failed to set window icon: {e}");
                        }
                    }
                    Err(e) => log::warn!("failed to decode window icon: {e}"),
                }
            }
            Ok(())
        })
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
            get_comfyui_config,
            save_comfyui_config,
            get_searxng_config,
            save_searxng_config,
            test_searxng_search,
            run_web_search,
            run_search_answer,
            pick_comfyui_output_dir,
            default_system_prompt,
            test_connection,
            app_version,
            app_build_hash,
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
            list_personas,
            pick_persona_file,
            import_persona,
            save_new_persona,
            delete_persona,
            get_persona_content,
            update_persona,
            list_rulesets,
            get_ruleset_content,
            update_ruleset,
            get_ruleset_example,
            list_chat_sessions,
            create_chat_session,
            load_chat_session,
            rename_chat_session,
            delete_chat_session,
            get_chat_state,
            get_chat_raw_state,
            send_chat_message,
            send_chat_message_streaming,
            run_turn_followup,
            test_comfyui_generation,
            generate_comfyui_image,
            run_image_reaction,
            read_generated_image,
            save_generated_image_as,
            probe_vision_capability,
            test_vision_support,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
