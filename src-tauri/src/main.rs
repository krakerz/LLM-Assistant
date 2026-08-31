#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
mod llm;
mod sandbox;

use config::{AppConfig, GrantedPath};
use llm::ChatMessage;
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

#[tauri::command]
fn pick_and_set_root(app: AppHandle, state: State<AppState>) -> Result<serde_json::Value, String> {
    let picked = app.dialog().file().blocking_pick_folder();
    let path = picked.ok_or("No folder selected")?;
    let root = PathBuf::from(path.to_string());
    if !root.is_dir() {
        return Err("Selected path is not a directory".into());
    }
    let cfg = config::load_or_init(&root).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(root.join(".temp-trash")).map_err(|e| e.to_string())?;
    *state.root.lock().unwrap() = Some(root.clone());
    Ok(serde_json::json!({ "root": root.display().to_string(), "config": cfg }))
}

#[tauri::command]
fn load_config(state: State<AppState>) -> Result<AppConfig, String> {
    let root = require_root(&state)?;
    config::load_or_init(&root).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_config(state: State<AppState>, cfg: AppConfig) -> Result<(), String> {
    let root = require_root(&state)?;
    config::save(&root, &cfg).map_err(|e| e.to_string())
}

#[tauri::command]
fn classify_command(cmd: String, state: State<AppState>) -> Result<serde_json::Value, String> {
    let root = require_root(&state)?;
    let cfg = config::load_or_init(&root).map_err(|e| e.to_string())?;
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
    let cfg = config::load_or_init(&root).map_err(|e| e.to_string())?;
    let shims = shim_dir(&app);
    sandbox::ensure_shims(&shims).map_err(|e| e.to_string())?;
    let outcome = sandbox::run_sandboxed(&root, &shims, &cfg.granted_paths, &cmd)
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "stdout": outcome.stdout,
        "stderr": outcome.stderr,
        "exit_code": outcome.exit_code,
    }))
}

#[tauri::command]
fn add_granted_path(
    state: State<AppState>,
    path: String,
    note: String,
    read_write: bool,
) -> Result<AppConfig, String> {
    let root = require_root(&state)?;
    let mut cfg = config::load_or_init(&root).map_err(|e| e.to_string())?;
    cfg.granted_paths.push(GrantedPath {
        path,
        note,
        read_write,
    });
    config::save(&root, &cfg).map_err(|e| e.to_string())?;
    Ok(cfg)
}

#[tauri::command]
fn remove_granted_path(state: State<AppState>, path: String) -> Result<AppConfig, String> {
    let root = require_root(&state)?;
    let mut cfg = config::load_or_init(&root).map_err(|e| e.to_string())?;
    cfg.granted_paths.retain(|g| g.path != path);
    config::save(&root, &cfg).map_err(|e| e.to_string())?;
    Ok(cfg)
}

#[tauri::command]
fn add_auto_approve(state: State<AppState>, binary: String) -> Result<AppConfig, String> {
    let root = require_root(&state)?;
    let mut cfg = config::load_or_init(&root).map_err(|e| e.to_string())?;
    if !cfg.auto_approve.iter().any(|b| b == &binary) {
        cfg.auto_approve.push(binary);
    }
    config::save(&root, &cfg).map_err(|e| e.to_string())?;
    Ok(cfg)
}

#[tauri::command]
async fn send_message(
    state: State<'_, AppState>,
    history: Vec<ChatMessage>,
) -> Result<String, String> {
    let root = require_root(&state)?;
    let cfg = config::load_or_init(&root).map_err(|e| e.to_string())?;
    let mut messages = vec![ChatMessage {
        role: "system".into(),
        content: cfg.system_prompt.clone(),
    }];
    messages.extend(history);
    llm::send_chat(&cfg.endpoint, &cfg.model, cfg.temperature, &messages)
        .await
        .map_err(|e| e.to_string())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            root: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            pick_and_set_root,
            load_config,
            save_config,
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
