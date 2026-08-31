#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chat_log;
mod config;
mod headless;
mod llm;
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
    /// Handle to whatever `send_chat` task is currently in flight, if any,
    /// so `stop_generation` can actually cancel it (not just stop the UI
    /// from proposing further steps) -- this is what makes the Stop button
    /// a real emergency stop for a model stuck repeating itself.
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

/// Shared setup for a newly-selected root, whether it came from the folder
/// picker or a CLI argument: make sure `.temp-trash` exists up front.
fn activate_root(root: &std::path::Path) -> Result<(), String> {
    fs::create_dir_all(root.join(".temp-trash")).map_err(|e| e.to_string())
}

/// Reads an optional folder path from argv (`llm-assistant /some/folder`) so
/// the app can start with a working directory already open -- handy for
/// testing without clicking through the picker each time.
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

    // Stderr, not Mixed: headless mode prints its actual result to stdout,
    // which needs to stay clean of log noise.
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

/// Opens the native folder picker and waits for the result via a channel.
/// Deliberately uses the non-blocking `pick_folder` callback API rather than
/// `blocking_pick_folder()` -- calling the blocking variant from inside a
/// Tauri command is a known way to deadlock the picker on some platforms.
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

/// Opens the same native folder picker, but just returns the chosen path for
/// the "grant a readable path" flow in Settings -- doesn't touch app state.
#[tauri::command]
async fn pick_granted_path(app: AppHandle) -> Result<Option<String>, String> {
    log::info!("pick_granted_path: opening folder picker");
    Ok(pick_folder_path(&app)
        .await?
        .map(|p| p.display().to_string()))
}

/// Lets the frontend check on load whether a root is already active (e.g.
/// preloaded from a CLI argument) without having to go through the picker.
#[tauri::command]
fn get_current_root(state: State<AppState>) -> Option<String> {
    state
        .root
        .lock()
        .unwrap()
        .as_ref()
        .map(|p| p.display().to_string())
}

/// Drops the selected folder so the app goes back to being a plain chat
/// client with no file access -- the sandbox has nothing to bind to `None`,
/// so `run_command` simply refuses with "No folder selected" until a new one
/// is picked. Returns the path that was unmounted, if any, for the UI to
/// report back to the user.
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

#[tauri::command]
fn default_system_prompt() -> &'static str {
    config::DEFAULT_SYSTEM_PROMPT
}

#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[tauri::command]
fn load_rules() -> Result<String, String> {
    rules::load_or_init().map_err(|e| e.to_string())
}

#[tauri::command]
fn save_rules(rules: String) -> Result<(), String> {
    log::info!("save_rules: {} bytes", rules.len());
    rules::save(&rules).map_err(|e| e.to_string())
}

/// Appends one entry to `<app-config-dir>/last-chat.log`, mirroring exactly
/// what the GUI shows (including collapsed "thinking" steps, since a flat
/// log file has no collapse concept) -- for debugging without driving the
/// window. Cleared at the start of every GUI launch, see `main()`.
#[tauri::command]
fn append_chat_log(text: String) -> Result<(), String> {
    chat_log::append(&text).map_err(|e| e.to_string())
}

#[tauri::command]
fn default_rules() -> &'static str {
    rules::DEFAULT_RULES
}

#[tauri::command]
fn classify_command(cmd: String) -> Result<serde_json::Value, String> {
    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "classification": sandbox::classify_command(&cmd),
        "auto_approved": sandbox::is_auto_approved(&cmd, &cfg.auto_approve),
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

/// Chat works with no folder open too (a plain assistant) -- the system
/// prompt gets a note appended about whether a folder is currently open, so
/// the model knows whether proposing shell commands makes sense right now.
#[tauri::command]
async fn send_message(
    state: State<'_, AppState>,
    history: Vec<ChatMessage>,
) -> Result<String, String> {
    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    let rules = rules::load_or_init().map_err(|e| e.to_string())?;
    let root = state.root.lock().unwrap().clone();
    log::info!(
        "send_message: endpoint={} model={} root={:?} history_len={}",
        cfg.endpoint,
        cfg.model,
        root,
        history.len()
    );

    let root_note = config::build_root_note(root.as_deref(), &cfg.granted_paths);

    // Rules first (mechanical/protocol, rarely edited), then the user's own
    // customizable system prompt, then the per-turn folder-state note.
    let mut messages = vec![ChatMessage {
        role: "system".into(),
        content: format!("{}\n\n{}\n\n{}", rules, cfg.system_prompt, root_note),
    }];
    messages.extend(history);

    // Spawned (rather than just awaited) so `stop_generation` has something
    // to abort -- an emergency stop needs to actually cancel the in-flight
    // request, not just stop the UI from proposing another one afterward.
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
            Ok(reply)
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
            "last-chat.log",
            "mirror of the GUI conversation, cleared each launch",
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

    // `llm-assistant <folder> <message...>` runs headless: one turn (plus
    // any commands it leads to) printed to stdout, no GUI. `llm-assistant
    // <folder>` alone (no message) is the existing GUI-preload behavior.
    if args.len() >= 3 {
        let root = PathBuf::from(&args[1]);
        if root.is_dir() {
            let message = args[2..].join(" ");
            log::info!("headless mode: root={} message={message:?}", root.display());
            headless::run(root, message);
        }
        log::warn!("ignoring CLI arguments: {:?} is not a directory", args[1]);
    }

    if let Err(e) = chat_log::clear() {
        log::warn!("failed to clear last-chat.log: {e}");
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
            app_version,
            load_rules,
            save_rules,
            default_rules,
            classify_command,
            run_command,
            add_granted_path,
            remove_granted_path,
            add_auto_approve,
            send_message,
            stop_generation,
            append_chat_log,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
