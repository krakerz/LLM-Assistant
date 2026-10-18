//! Chat-mode commands. `api` holds one plain implementation per command
//! (returning `Result<T, String>`), shared by the Tauri wrappers below and
//! the axum handlers in `server.rs`; both are thin adapters over it. Only the
//! native-dialog commands exist solely here.

use crate::app_commands::first_line;
use crate::fileops_commands::pick_folder_path;
use crate::llm::ChatMessage;
use crate::{chat_session, chat_turn, comfyui, gpu, persona, ruleset, searxng};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

/// One event of a streamed turn 1, on both transports: a Tauri `Channel` and
/// the server's SSE body.
#[derive(serde::Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatStreamEvent {
    Delta {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
    },
    Done {
        result: chat_turn::TurnOneResult,
    },
    Error {
        message: String,
    },
}

pub mod api {
    use super::*;
    use crate::config;

    fn err(e: impl std::fmt::Display) -> String {
        e.to_string()
    }

    // --- config ---

    pub fn get_comfyui_config() -> Result<comfyui::ComfyUiConfig, String> {
        comfyui::load_or_init().map_err(err)
    }

    pub fn save_comfyui_config(cfg: comfyui::ComfyUiConfig) -> Result<(), String> {
        log::info!("save_comfyui_config: base_url={}", cfg.base_url);
        comfyui::save(&cfg).map_err(err)
    }

    pub fn get_searxng_config() -> Result<searxng::SearxngConfig, String> {
        searxng::load_or_init().map_err(err)
    }

    pub fn save_searxng_config(cfg: searxng::SearxngConfig) -> Result<(), String> {
        log::info!("save_searxng_config: base_url={}", cfg.base_url);
        searxng::save(&cfg).map_err(err)
    }

    /// Tests what's typed in the dialog, not what's saved.
    pub async fn test_searxng_search(
        cfg: searxng::SearxngConfig,
    ) -> Result<Vec<searxng::SearchResult>, String> {
        log::info!("test_searxng_search: base_url={}", cfg.base_url);
        searxng::search(&cfg, "test query").await.map_err(err)
    }

    /// Renders a trivial prompt with the config as typed and returns the real
    /// image as a `data:` URL: a stale node id or a mis-mapped field only
    /// shows up in the output, not in a checkmark.
    pub async fn test_comfyui_generation(cfg: comfyui::ComfyUiConfig) -> Result<String, String> {
        log::info!("test_comfyui_generation: base_url={}", cfg.base_url);
        let fields = comfyui::ImagePromptFields {
            positive: Some("a red circle on a white background".to_string()),
            ..Default::default()
        };
        let app_cfg = config::load_or_init().map_err(err)?;
        let image = gpu::with_image_slot(&app_cfg, &cfg, || comfyui::generate_image(&cfg, &fields))
            .await
            .map_err(err)?;
        let name = image.filename.to_lowercase();
        let mime = if name.ends_with(".jpg") || name.ends_with(".jpeg") {
            "image/jpeg"
        } else if name.ends_with(".webp") {
            "image/webp"
        } else {
            "image/png"
        };
        Ok(format!(
            "data:{mime};base64,{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &image.bytes)
        ))
    }

    // --- personas and rulesets ---

    pub fn list_personas() -> Result<Vec<persona::PersonaSummary>, String> {
        persona::list_personas().map_err(err)
    }

    pub fn save_new_persona(
        name: String,
        content: String,
    ) -> Result<persona::PersonaSummary, String> {
        persona::save_new_persona(&name, &content).map_err(err)
    }

    pub fn delete_persona(name: String) -> Result<(), String> {
        persona::delete_persona(&name).map_err(err)
    }

    pub fn get_persona_content(name: String) -> Result<String, String> {
        persona::load_persona(&name).map_err(err)
    }

    pub fn update_persona(name: String, content: String) -> Result<(), String> {
        persona::update_persona(&name, &content).map_err(err)
    }

    /// Editing only: the two rulesets are seeded by `list_rulesets` itself.
    pub fn list_rulesets() -> Result<Vec<ruleset::RulesetSummary>, String> {
        ruleset::list_rulesets().map_err(err)
    }

    pub fn get_ruleset_content(name: String) -> Result<String, String> {
        ruleset::load_ruleset(&name).map_err(err)
    }

    pub fn update_ruleset(name: String, content: String) -> Result<(), String> {
        ruleset::update_ruleset(&name, &content).map_err(err)
    }

    /// Backs the editor's "see an example" link; `None` hides it.
    pub fn get_ruleset_example(name: String) -> Option<String> {
        ruleset::example_for(&name).map(|s| s.to_string())
    }

    // --- sessions ---

    pub fn list_chat_sessions() -> Result<Vec<chat_session::SessionSummary>, String> {
        chat_session::list_sessions().map_err(err)
    }

    pub fn create_chat_session(
        persona: Option<String>,
    ) -> Result<chat_session::SessionSummary, String> {
        chat_session::create_session(persona.as_deref()).map_err(err)
    }

    /// `{meta, history}`; every message carries an `id`.
    pub fn load_chat_session(session_id: String) -> Result<serde_json::Value, String> {
        let (meta, history) = chat_session::load_session(&session_id).map_err(err)?;
        Ok(serde_json::json!({ "meta": meta, "history": history }))
    }

    pub async fn rename_chat_session(session_id: String, title: String) -> Result<(), String> {
        chat_session::rename_session(&session_id, &title)
            .await
            .map_err(err)
    }

    pub async fn delete_chat_session(session_id: String) -> Result<(), String> {
        chat_session::delete_session(&session_id).await.map_err(err)
    }

    /// The derived `state.md` summary.
    pub fn get_chat_state(session_id: String) -> String {
        chat_session::read_state(&session_id)
    }

    /// The raw `state.json` source of truth.
    pub fn get_chat_raw_state(session_id: String) -> String {
        chat_session::read_raw_state(&session_id)
    }

    // --- the chat flow ---

    /// Turn 1, streamed through `emit`, ending with exactly one `Done` or
    /// `Error` event: once streaming has started a failure can only be an
    /// event, never a return value.
    pub async fn stream_chat_message(
        input: chat_turn::SendInput,
        mut emit: impl FnMut(ChatStreamEvent) + Send,
    ) {
        let result = match config::load_or_init() {
            Ok(cfg) => chat_turn::send_user_message(&cfg, input, |delta| {
                emit(match delta {
                    crate::llm::ChatDelta::Content(t) => ChatStreamEvent::Delta {
                        content: Some(t.to_string()),
                        thinking: None,
                    },
                    crate::llm::ChatDelta::Reasoning(t) => ChatStreamEvent::Delta {
                        content: None,
                        thinking: Some(t.to_string()),
                    },
                })
            })
            .await
            .map_err(err),
            Err(e) => Err(err(e)),
        };
        emit(match result {
            Ok(result) => ChatStreamEvent::Done { result },
            Err(message) => ChatStreamEvent::Error { message },
        });
    }

    pub fn stop_chat_generation(session_id: String) -> bool {
        chat_turn::stop_generation(&session_id)
    }

    pub async fn run_chat_followup(
        session_id: String,
        assistant_message_id: String,
        photo_forced: bool,
        photo_hint: Option<String>,
    ) -> Result<chat_turn::FollowupPlan, String> {
        let cfg = config::load_or_init().map_err(err)?;
        chat_turn::run_followup(
            &cfg,
            &session_id,
            &assistant_message_id,
            photo_forced,
            photo_hint,
        )
        .await
        .map_err(err)
    }

    pub async fn generate_chat_image(
        session_id: String,
        message_id: String,
        fields: comfyui::ImagePromptFields,
        vision: Option<bool>,
    ) -> Result<chat_turn::GeneratedImageResult, String> {
        let cfg = config::load_or_init().map_err(err)?;
        chat_turn::generate_image(&cfg, &session_id, &message_id, &fields, vision)
            .await
            .map_err(err)
    }

    pub async fn run_chat_web_search(
        session_id: String,
        query: String,
    ) -> Result<chat_turn::WebSearchResult, String> {
        let cfg = config::load_or_init().map_err(err)?;
        chat_turn::web_search(&cfg, &session_id, &query)
            .await
            .map_err(err)
    }

    /// Redisplays a saved generated image. Only files under the output folder
    /// can be read (`comfyui::validate_generated_path`).
    pub fn read_generated_image(path: String) -> Result<String, String> {
        comfyui::read_as_data_url(std::path::Path::new(&path)).map_err(err)
    }

    /// Sends the test image plus a one-word question: the raw reply or the raw
    /// error, never a guess dressed up as a verdict.
    pub async fn test_vision_support(
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
        match crate::llm::send_chat(&endpoint, &model, &api_key, 0.0, &[probe]).await {
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
                Err(err(e))
            }
        }
    }
}

// --- Tauri wrappers ---

#[tauri::command]
pub fn get_comfyui_config() -> Result<comfyui::ComfyUiConfig, String> {
    api::get_comfyui_config()
}

#[tauri::command]
pub fn save_comfyui_config(cfg: comfyui::ComfyUiConfig) -> Result<(), String> {
    api::save_comfyui_config(cfg)
}

#[tauri::command]
pub async fn test_comfyui_generation(cfg: comfyui::ComfyUiConfig) -> Result<String, String> {
    api::test_comfyui_generation(cfg).await
}

#[tauri::command]
pub fn get_searxng_config() -> Result<searxng::SearxngConfig, String> {
    api::get_searxng_config()
}

#[tauri::command]
pub fn save_searxng_config(cfg: searxng::SearxngConfig) -> Result<(), String> {
    api::save_searxng_config(cfg)
}

#[tauri::command]
pub async fn test_searxng_search(
    cfg: searxng::SearxngConfig,
) -> Result<Vec<searxng::SearchResult>, String> {
    api::test_searxng_search(cfg).await
}

/// Same `pick_folder_path` helper `pick_granted_path` uses; doesn't touch app state.
#[tauri::command]
pub async fn pick_comfyui_output_dir(app: AppHandle) -> Result<Option<String>, String> {
    Ok(pick_folder_path(&app)
        .await?
        .map(|p| p.display().to_string()))
}

#[tauri::command]
pub fn list_personas() -> Result<Vec<persona::PersonaSummary>, String> {
    api::list_personas()
}

/// Native file picker filtered to `.md`, for importing an existing persona.
#[tauri::command]
pub async fn pick_persona_file(app: AppHandle) -> Result<Option<String>, String> {
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
pub fn import_persona(path: String) -> Result<persona::PersonaSummary, String> {
    persona::import_persona(std::path::Path::new(&path)).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_new_persona(name: String, content: String) -> Result<persona::PersonaSummary, String> {
    api::save_new_persona(name, content)
}

#[tauri::command]
pub fn delete_persona(name: String) -> Result<(), String> {
    api::delete_persona(name)
}

#[tauri::command]
pub fn get_persona_content(name: String) -> Result<String, String> {
    api::get_persona_content(name)
}

#[tauri::command]
pub fn update_persona(name: String, content: String) -> Result<(), String> {
    api::update_persona(name, content)
}

#[tauri::command]
pub fn list_rulesets() -> Result<Vec<ruleset::RulesetSummary>, String> {
    api::list_rulesets()
}

#[tauri::command]
pub fn get_ruleset_content(name: String) -> Result<String, String> {
    api::get_ruleset_content(name)
}

#[tauri::command]
pub fn update_ruleset(name: String, content: String) -> Result<(), String> {
    api::update_ruleset(name, content)
}

#[tauri::command]
pub fn get_ruleset_example(name: String) -> Option<String> {
    api::get_ruleset_example(name)
}

#[tauri::command]
pub fn list_chat_sessions() -> Result<Vec<chat_session::SessionSummary>, String> {
    api::list_chat_sessions()
}

#[tauri::command]
pub fn create_chat_session(
    persona: Option<String>,
) -> Result<chat_session::SessionSummary, String> {
    api::create_chat_session(persona)
}

#[tauri::command]
pub fn load_chat_session(session_id: String) -> Result<serde_json::Value, String> {
    api::load_chat_session(session_id)
}

#[tauri::command]
pub async fn rename_chat_session(session_id: String, title: String) -> Result<(), String> {
    api::rename_chat_session(session_id, title).await
}

#[tauri::command]
pub async fn delete_chat_session(session_id: String) -> Result<(), String> {
    api::delete_chat_session(session_id).await
}

#[tauri::command]
pub fn get_chat_state(session_id: String) -> String {
    api::get_chat_state(session_id)
}

#[tauri::command]
pub fn get_chat_raw_state(session_id: String) -> String {
    api::get_chat_raw_state(session_id)
}

/// Always `Ok`: failures arrive as an `error` event on `channel`, since the
/// frontend is already mid-stream by then. A closed webview doesn't stop the
/// turn; it still persists.
#[tauri::command]
pub async fn send_chat_message_streaming(
    session_id: String,
    text: String,
    images: Vec<String>,
    channel: tauri::ipc::Channel<ChatStreamEvent>,
) -> Result<(), String> {
    let input = chat_turn::SendInput {
        session_id,
        text,
        images,
    };
    api::stream_chat_message(input, |event| {
        let _ = channel.send(event);
    })
    .await;
    Ok(())
}

#[tauri::command]
pub fn stop_chat_generation(session_id: String) -> bool {
    api::stop_chat_generation(session_id)
}

#[tauri::command]
pub async fn run_chat_followup(
    session_id: String,
    assistant_message_id: String,
    photo_forced: bool,
    photo_hint: Option<String>,
) -> Result<chat_turn::FollowupPlan, String> {
    api::run_chat_followup(session_id, assistant_message_id, photo_forced, photo_hint).await
}

#[tauri::command]
pub async fn generate_chat_image(
    session_id: String,
    message_id: String,
    fields: comfyui::ImagePromptFields,
    vision: Option<bool>,
) -> Result<chat_turn::GeneratedImageResult, String> {
    api::generate_chat_image(session_id, message_id, fields, vision).await
}

#[tauri::command]
pub async fn run_chat_web_search(
    session_id: String,
    query: String,
) -> Result<chat_turn::WebSearchResult, String> {
    api::run_chat_web_search(session_id, query).await
}

#[tauri::command]
pub fn read_generated_image(path: String) -> Result<String, String> {
    api::read_generated_image(path)
}

/// Backs the image preview popup's "Save" button: copies the file wherever
/// the user picks. `Ok(None)` means the dialog was cancelled.
#[tauri::command]
pub async fn save_generated_image_as(
    app: AppHandle,
    path: String,
) -> Result<Option<String>, String> {
    // The path comes from the webview: only files under the generated-image
    // output folder may be copied out.
    let source =
        comfyui::validate_generated_path(std::path::Path::new(&path)).map_err(|e| e.to_string())?;
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
    std::fs::copy(&source, &dest_path).map_err(|e| e.to_string())?;
    Ok(Some(dest_path.display().to_string()))
}

/// A small, fixed, solid-red 2x2 PNG -- trivial for a model to describe, so a
/// reply that doesn't mention red is as telling as an outright error.
pub const VISION_TEST_IMAGE_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAEElEQVR42mP4z8AARAwQCgAf7gP9Y167WwAAAABJRU5ErkJggg==";

#[tauri::command]
pub async fn test_vision_support(
    endpoint: String,
    model: String,
    api_key: String,
) -> Result<String, String> {
    api::test_vision_support(endpoint, model, api_key).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_events_match_the_contract() {
        let json = |e: &ChatStreamEvent| serde_json::to_value(e).unwrap();
        assert_eq!(
            json(&ChatStreamEvent::Delta {
                content: Some("hi".into()),
                thinking: None
            }),
            serde_json::json!({"type": "delta", "content": "hi"})
        );
        assert_eq!(
            json(&ChatStreamEvent::Delta {
                content: None,
                thinking: Some("hm".into())
            }),
            serde_json::json!({"type": "delta", "thinking": "hm"})
        );
        assert_eq!(
            json(&ChatStreamEvent::Error {
                message: "boom".into()
            }),
            serde_json::json!({"type": "error", "message": "boom"})
        );
        let done = json(&ChatStreamEvent::Done {
            result: chat_turn::TurnOneResult::default(),
        });
        assert_eq!(done["type"], "done");
        for key in [
            "userMessageId",
            "assistantMessageId",
            "reply",
            "thinking",
            "photoForced",
            "photoHint",
            "dropped",
            "summarized",
            "summary",
            "title",
            "stopped",
        ] {
            assert!(done["result"].get(key).is_some(), "missing {key}");
        }
    }

    #[test]
    fn followup_and_generation_dtos_are_camel_case() {
        let plan = chat_turn::FollowupPlan {
            image: Some(chat_turn::ImagePlan {
                fields: Default::default(),
                reason: "r".into(),
            }),
            search_query: Some("q".into()),
            state_updated: true,
        };
        let v = serde_json::to_value(&plan).unwrap();
        assert!(v["image"]["fields"].is_object() && v["image"]["reason"] == "r");
        assert!(v["searchQuery"] == "q" && v["stateUpdated"] == true);

        let g = serde_json::to_value(chat_turn::GeneratedImageResult {
            image_path: "p".into(),
            image_data_url: "d".into(),
            reaction: Some(chat_turn::ReplyMsg {
                id: "i".into(),
                text: "t".into(),
                thinking: None,
            }),
        })
        .unwrap();
        assert!(g["imagePath"] == "p" && g["imageDataUrl"] == "d" && g["reaction"]["id"] == "i");

        let w = serde_json::to_value(chat_turn::WebSearchResult {
            results_summary: "s".into(),
            answer: None,
        })
        .unwrap();
        assert!(w["resultsSummary"] == "s" && w["answer"].is_null());
    }

    #[test]
    fn send_input_reads_the_frontend_shape() {
        let input: chat_turn::SendInput =
            serde_json::from_str(r#"{"sessionId":"s","text":"hi","images":["data:x"]}"#).unwrap();
        assert_eq!((input.session_id.as_str(), input.images.len()), ("s", 1));
        let bare: chat_turn::SendInput =
            serde_json::from_str(r#"{"sessionId":"s","text":"/pic"}"#).unwrap();
        assert!(bare.images.is_empty());
    }
}
