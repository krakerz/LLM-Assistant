//! Tauri commands for app-wide settings and metadata.

use crate::config::{self, AppConfig};
use crate::llm::{self, ChatMessage};

#[tauri::command]
pub fn load_config() -> Result<AppConfig, String> {
    config::load_or_init().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_config(cfg: AppConfig) -> Result<(), String> {
    log::info!("save_config: endpoint={} model={}", cfg.endpoint, cfg.model);
    let on_disk = config::load_or_init().map_err(|e| e.to_string())?;
    config::save(&keep_command_owned(cfg, on_disk)).map_err(|e| e.to_string())
}

/// `auto_approve` and `granted_paths` are written only by their own commands
/// (Always allow, add/remove), which save at once. Settings and the chat
/// toggles send a whole config that may predate those, so the file wins.
fn keep_command_owned(mut cfg: AppConfig, on_disk: AppConfig) -> AppConfig {
    cfg.auto_approve = on_disk.auto_approve;
    cfg.granted_paths = on_disk.granted_paths;
    cfg
}

/// Tests what's typed in the dialog, not what's saved. A real completion
/// rather than a `/models` ping, since the failures worth catching are the
/// ones reachability misses: wrong model name, rejected key, wrong URL path.
#[tauri::command]
pub async fn test_connection(
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
pub fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > 80 {
        format!("{}…", line.chars().take(80).collect::<String>())
    } else {
        line.to_string()
    }
}

#[tauri::command]
pub fn default_system_prompt() -> &'static str {
    config::DEFAULT_SYSTEM_PROMPT
}

#[tauri::command]
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A short, content-addressed identifier for this exact build -- see
/// `build.rs`'s `build_hash()` doc comment for how it's computed. Shown
/// next to the version number in the Settings UI, so "is this actually the
/// build I just rebuilt" never has to be guessed from a binary's mtime.
#[tauri::command]
pub fn app_build_hash() -> &'static str {
    env!("BUILD_HASH")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_dialog_save_keeps_command_owned_fields() {
        let grant: config::GrantedPath =
            serde_json::from_str(r#"{"path":"/data","note":"","read_write":false}"#).unwrap();
        let on_disk = AppConfig {
            auto_approve: vec!["ls".into(), "cat".into()],
            granted_paths: vec![grant],
            ..Default::default()
        };
        let stale = AppConfig {
            endpoint: "http://new/v1".into(),
            auto_approve: vec!["ls".into()],
            ..Default::default()
        };
        let saved = keep_command_owned(stale, on_disk);
        assert_eq!(saved.endpoint, "http://new/v1");
        assert_eq!(
            saved.auto_approve,
            vec!["ls".to_string(), "cat".to_string()]
        );
        assert_eq!(saved.granted_paths.len(), 1);
        assert_eq!(saved.granted_paths[0].path, "/data");
    }
}
