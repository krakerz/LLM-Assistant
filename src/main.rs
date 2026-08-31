#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod llm;
mod paths;
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

    let term = TermLogger::new(
        LevelFilter::Info,
        LogConfig::default(),
        TerminalMode::Mixed,
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
fn add_granted_path(path: String, note: String, read_write: bool) -> Result<AppConfig, String> {
    let mut cfg = config::load_or_init().map_err(|e| e.to_string())?;
    log::info!("add_granted_path: {path} (rw={read_write}) -- {note}");
    cfg.granted_paths.push(GrantedPath {
        path,
        note,
        read_write,
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
    let root = state.root.lock().unwrap().clone();
    log::info!(
        "send_message: endpoint={} model={} root={:?} history_len={}",
        cfg.endpoint,
        cfg.model,
        root,
        history.len()
    );

    let root_note = match &root {
        Some(r) => format!(
            "You currently have this folder open and can propose shell commands confined to it: {}",
            r.display()
        ),
        None => "No folder is open right now, so don't propose shell commands -- just chat \
                 normally, and if the user wants file operations, tell them to select a folder \
                 first."
            .to_string(),
    };

    let mut messages = vec![ChatMessage {
        role: "system".into(),
        content: format!("{}\n\n{}", cfg.system_prompt, root_note),
    }];
    messages.extend(history);
    match llm::send_chat(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        cfg.temperature,
        &messages,
    )
    .await
    {
        Ok(reply) => {
            log::debug!("send_message: reply {} bytes", reply.len());
            Ok(reply)
        }
        Err(e) => {
            log::error!("send_message failed: {e}");
            Err(e.to_string())
        }
    }
}

fn main() {
    init_logging();
    log::info!(
        "LLM Assistant starting, config dir = {}",
        paths::app_config_dir().display()
    );

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
        })
        .invoke_handler(tauri::generate_handler![
            pick_and_set_root,
            pick_granted_path,
            get_current_root,
            unmount_root,
            load_config,
            save_config,
            default_system_prompt,
            classify_command,
            run_command,
            add_granted_path,
            remove_granted_path,
            add_auto_approve,
            send_message,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
