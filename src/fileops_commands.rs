//! Tauri commands for operation mode (file ops): the open folder, rules,
//! grants, and the agent task (`agent_send` and friends).

use crate::agent::{
    self, AgentEvent, AgentHost, AgentSession, BoxFuture, Cancel, ConfirmRequest, Decision,
};
use crate::config::{self, AppConfig, GrantedPath};
use crate::{chat_log, memory, rules, sandbox};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::oneshot;

pub struct AppState {
    pub root: Mutex<Option<PathBuf>>,
    /// The open folder's agent session; `None` until the first message.
    pub session: tokio::sync::Mutex<Option<AgentSession>>,
    /// Cancel handle of the running task (only one runs at a time).
    pub running: Mutex<Option<Cancel>>,
    /// Confirmations the running task is waiting on, by tool-call id.
    pub pending_confirms: Mutex<HashMap<String, oneshot::Sender<Decision>>>,
}

impl AppState {
    pub fn new(root: Option<PathBuf>) -> Self {
        Self {
            root: Mutex::new(root),
            session: tokio::sync::Mutex::new(None),
            running: Mutex::new(None),
            pending_confirms: Mutex::new(HashMap::new()),
        }
    }

    /// Cancels any running task and drops the session once it has let go.
    async fn stop_and_drop_session(&self) {
        if let Some(cancel) = self.running.lock().unwrap().as_ref() {
            cancel.cancel();
        }
        *self.session.lock().await = None;
    }
}

/// Uses the non-blocking `pick_folder` callback API on purpose:
/// `blocking_pick_folder()` inside a Tauri command deadlocks the picker.
///
/// `.set_parent(&window)` ties the native GTK file chooser to the main
/// window -- without it, the window manager has no relationship between the
/// two, and the picker can end up opening behind the app instead of on top
/// of it.
pub async fn pick_folder_path(app: &AppHandle) -> Result<Option<PathBuf>, String> {
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
pub async fn pick_and_set_root(
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

    sandbox::check_root_allowed(&root)?;

    state.stop_and_drop_session().await;
    let previous = state.root.lock().unwrap().replace(root.clone());
    if previous.as_ref() != Some(&root) {
        // The old folder's session record must not leak into the new one.
        memory::clear_session();
    }
    log::info!("pick_and_set_root: root set to {}", root.display());

    let cfg = config::load_or_init().map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "root": root.display().to_string(), "config": cfg }))
}

/// Picker for the "grant a readable path" flow; doesn't touch app state.
#[tauri::command]
pub async fn pick_granted_path(app: AppHandle) -> Result<Option<String>, String> {
    log::info!("pick_granted_path: opening folder picker");
    Ok(pick_folder_path(&app)
        .await?
        .map(|p| p.display().to_string()))
}

/// So the frontend can pick up a CLI-preloaded root without the picker.
#[tauri::command]
pub fn get_current_root(state: State<AppState>) -> Option<String> {
    state
        .root
        .lock()
        .unwrap()
        .as_ref()
        .map(|p| p.display().to_string())
}

/// Back to plain chat with no file access. Returns what was unmounted, for
/// the UI. Stops any running task and drops its session.
#[tauri::command]
pub async fn unmount_root(state: State<'_, AppState>) -> Result<Option<String>, String> {
    state.stop_and_drop_session().await;
    let old = state.root.lock().unwrap().take();
    if let Some(root) = &old {
        log::info!("unmount_root: cleared root {}", root.display());
    }
    // The record of the unmounted folder must not leak into whatever opens next.
    memory::clear_session();
    Ok(old.map(|p| p.display().to_string()))
}

#[tauri::command]
pub fn load_general_rules() -> Result<String, String> {
    rules::load_general_or_init().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_general_rules(rules: String) -> Result<(), String> {
    log::info!("save_general_rules: {} bytes", rules.len());
    rules::save_general(&rules).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn default_general_rules() -> &'static str {
    rules::DEFAULT_GENERAL_RULES
}

#[tauri::command]
pub fn load_command_rules() -> Result<String, String> {
    rules::load_command_or_init().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_command_rules(rules: String) -> Result<(), String> {
    log::info!("save_command_rules: {} bytes", rules.len());
    rules::save_command(&rules).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn default_command_rules() -> &'static str {
    rules::DEFAULT_COMMAND_RULES
}

/// Mirrors what the GUI shows, thinking steps included, into this session's
/// `logs/chat-*.log`.
#[tauri::command]
pub fn append_chat_log(text: String) -> Result<(), String> {
    chat_log::append(&text).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn add_granted_path(
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
pub fn remove_granted_path(path: String) -> Result<AppConfig, String> {
    let mut cfg = config::load_or_init().map_err(|e| e.to_string())?;
    log::info!("remove_granted_path: {path}");
    cfg.granted_paths.retain(|g| g.path != path);
    config::save(&cfg).map_err(|e| e.to_string())?;
    Ok(cfg)
}

#[tauri::command]
pub fn add_auto_approve(binary: String) -> Result<AppConfig, String> {
    let mut cfg = config::load_or_init().map_err(|e| e.to_string())?;
    if !cfg.auto_approve.iter().any(|b| b == &binary) {
        log::info!("add_auto_approve: {binary}");
        cfg.auto_approve.push(binary);
    }
    config::save(&cfg).map_err(|e| e.to_string())?;
    Ok(cfg)
}

#[tauri::command]
pub fn remove_auto_approve(binary: String) -> Result<AppConfig, String> {
    let mut cfg = config::load_or_init().map_err(|e| e.to_string())?;
    log::info!("remove_auto_approve: {binary}");
    cfg.auto_approve.retain(|b| b != &binary);
    config::save(&cfg).map_err(|e| e.to_string())?;
    Ok(cfg)
}

// --- Agent task ---

/// Routes a task's events to the webview and holds confirmations open until
/// `agent_respond` answers them.
struct GuiHost<'a> {
    channel: Channel<AgentEvent>,
    pending: &'a Mutex<HashMap<String, oneshot::Sender<Decision>>>,
    cancel: Cancel,
}

impl AgentHost for GuiHost<'_> {
    fn emit(&self, ev: AgentEvent) {
        if let Err(e) = self.channel.send(ev) {
            log::debug!("agent event not delivered: {e}");
        }
    }

    fn confirm<'a>(&'a self, req: ConfirmRequest) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            self.pending.lock().unwrap().insert(req.id.clone(), tx);
            self.emit(AgentEvent::Confirm {
                id: req.id.clone(),
                title: req.title,
                command: req.command,
                diff: req.diff,
                can_always_allow: req.can_always_allow,
            });
            tokio::select! {
                d = rx => d.unwrap_or(Decision::Deny),
                _ = self.cancel.cancelled() => {
                    self.pending.lock().unwrap().remove(&req.id);
                    Decision::Deny
                }
            }
        })
    }
}

/// Runs one task for the open folder; returns when it ends. Progress arrives
/// on `on_event`.
#[tauri::command]
pub async fn agent_send(
    state: State<'_, AppState>,
    message: String,
    on_event: Channel<AgentEvent>,
) -> Result<(), String> {
    let root = state
        .root
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "no folder open".to_string())?;
    let cancel = {
        let mut running = state.running.lock().unwrap();
        if running.is_some() {
            return Err("a task is already running".into());
        }
        let cancel = Cancel::new();
        *running = Some(cancel.clone());
        cancel
    };

    // Cleared on every exit, a panic or a dropped future included, or the
    // UI would stay stuck on "a task is already running".
    let _running = RunningGuard(&state);
    let result = async {
        let cfg = config::load_or_init().map_err(|e| e.to_string())?;
        let mut guard = state.session.lock().await;
        if !matches!(guard.as_ref(), Some(s) if s.root() == root) {
            sandbox::check_root_allowed(&root)?;
            *guard = Some(AgentSession::new(&root, &cfg).map_err(|e| e.to_string())?);
        }
        let session = guard.as_mut().expect("just created");
        log::info!(
            "agent_send: root={} model={} message={} bytes",
            root.display(),
            cfg.model,
            message.len()
        );
        let host = GuiHost {
            channel: on_event,
            pending: &state.pending_confirms,
            cancel: cancel.clone(),
        };
        let outcome = agent::run_task(&cfg, session, message, &host, &cancel).await;
        log::info!("agent_send: finished ({:?})", outcome.reason);
        Ok::<(), String>(())
    }
    .await;
    result
}

struct RunningGuard<'a>(&'a AppState);

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut running) = self.0.running.lock() {
            *running = None;
        }
        if let Ok(mut pending) = self.0.pending_confirms.lock() {
            pending.clear();
        }
    }
}

/// Answers a pending `confirm` event: "approve", "always" or "deny".
#[tauri::command]
pub fn agent_respond(state: State<AppState>, id: String, decision: String) -> Result<(), String> {
    let decision = match decision.as_str() {
        "approve" => Decision::Approve,
        "always" => Decision::AlwaysAllow,
        "deny" => Decision::Deny,
        other => return Err(format!("unknown decision '{other}'")),
    };
    let tx = state
        .pending_confirms
        .lock()
        .unwrap()
        .remove(&id)
        .ok_or_else(|| "that confirmation is no longer pending".to_string())?;
    let _ = tx.send(decision);
    Ok(())
}

/// Cancels the running task, if any.
#[tauri::command]
pub fn agent_stop(state: State<AppState>) -> bool {
    match state.running.lock().unwrap().as_ref() {
        Some(cancel) => {
            cancel.cancel();
            log::info!("agent_stop: cancelling the running task");
            true
        }
        None => false,
    }
}

/// Backs the Clear button: forgets the conversation and the session record
/// (see `memory::clear_session` for why both).
#[tauri::command]
pub async fn agent_reset(state: State<'_, AppState>) -> Result<(), String> {
    state.stop_and_drop_session().await;
    memory::clear_session();
    Ok(())
}
