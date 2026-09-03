//! Chat mode, ported to a plain HTTP server -- `llm-assistant --server`. No
//! file-ops/sandbox mode here at all: it depends on `bubblewrap` sandboxing
//! tied to a folder picked through the desktop GUI, neither of which means
//! anything to a remote browser client.
//!
//! Every route below is a thin wrapper around the exact same functions
//! `main.rs`'s `#[tauri::command]`s call -- `chat_turn`, `chat_session`,
//! `persona`, `comfyui`, `searxng`, `config`, `llm` are all plain async Rust
//! with no Tauri dependency (only file-ops commands ever touch
//! `State<AppState>`/`AppHandle`), so there is no business logic duplicated
//! here, only its HTTP shape. Like those commands, every handler reloads its
//! config fresh from disk per call rather than caching anything -- the same
//! "never trust a stale copy" pattern the rest of the app already follows.
//!
//! `ui/`'s static assets are compiled directly into the binary
//! (`WebAssets`, via `rust-embed`) rather than read from a loose folder on
//! disk: this mode has no `AppHandle`, so it can't reach Tauri's own asset
//! bundling, and a path like "next to the binary" doesn't exist for an
//! AppImage (mounted from a temporary squashfs). Embedding makes `--server`
//! work identically from a dev build, a `.deb` install, or an AppImage.
//!
//! `ui/main.js`'s `invoke()` forks to a `fetch("/api/<command>")` call when
//! `window.__TAURI__` doesn't exist, POSTing the same camelCase-keyed
//! argument object Tauri's own invoke already sends -- every request DTO
//! below uses `#[serde(rename_all = "camelCase")]` to match that shape
//! exactly, so no frontend payload needed to change.

use crate::llm::ChatMessage;
use crate::paths::app_config_dir;
use crate::{chat_session, chat_turn, comfyui, config, llm, persona, ruleset, searxng};
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

// --- config: same load_or_init/save shape as comfyui.rs/searxng.rs ---

fn config_path() -> PathBuf {
    app_config_dir().join("server.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Never accepted as a CLI flag -- shell history / `ps aux` would leak
    /// it, unlike `bind`/`port`, which are fine to pass ad hoc. Empty means
    /// open, unauthenticated access: a deliberate choice for a trusted
    /// network, not an error `run` blocks startup on -- see its own doc
    /// comment.
    #[serde(default)]
    pub password: String,
}

fn default_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    9333
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: default_port(),
            password: String::new(),
        }
    }
}

pub fn load_or_init() -> anyhow::Result<ServerConfig> {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(_) => {
            let cfg = ServerConfig::default();
            save(&cfg)?;
            Ok(cfg)
        }
    }
}

pub fn save(cfg: &ServerConfig) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

// --- static assets, embedded at compile time (see module doc comment) ---

#[derive(rust_embed::RustEmbed)]
#[folder = "ui/"]
struct WebAssets;

async fn serve_asset(path: &str) -> Response {
    let path = if path.is_empty() { "index.html" } else { path };
    match WebAssets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [(header::CONTENT_TYPE, mime.as_ref().to_string())],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn serve_index() -> Response {
    serve_asset("index.html").await
}

async fn serve_named_asset(AxumPath(path): AxumPath<String>) -> Response {
    serve_asset(&path).await
}

// --- auth: HTTP Basic, applied only when a password is actually set ---

fn check_basic_auth(headers: &HeaderMap, password: &str) -> bool {
    let Some(header_value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(encoded) = header_value.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
    else {
        return false;
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return false;
    };
    // "username:password" -- the username is never checked, only the half
    // after the colon matters, so any username the browser prompts for works.
    decoded.split_once(':').map(|(_, pass)| pass) == Some(password)
}

async fn require_password(
    State(password): State<Arc<str>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if check_basic_auth(&headers, &password) {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"llm-assistant\"")],
            "password required",
        )
            .into_response()
    }
}

// --- small helpers shared by every route below ---

/// Mirrors every `#[tauri::command]` in `main.rs` that returns
/// `Result<T, String>`: `Ok` becomes the plain JSON value (exactly what
/// Tauri's own IPC would have sent back), `Err` becomes a `400` with the
/// same message, still readable by the frontend's `invoke` shim.
fn ok_or_400<T: Serialize>(result: Result<T, String>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
    }
}

fn load_cfg() -> Result<config::AppConfig, String> {
    config::load_or_init().map_err(|e| e.to_string())
}

// --- routes: one handler per chat-mode Tauri command, same body as its
// `main.rs` counterpart. Commands needing a native OS dialog have no
// equivalent here: `pick_persona_file` is replaced client-side by reading
// the chosen file with `FileReader` and calling `save_new_persona` with its
// text directly; `pick_comfyui_output_dir` becomes a plain text field with
// no "browse" button; `save_generated_image_as` needs nothing server-side,
// since the browser already offers "save image" on the `data:` URL itself.
// ---

async fn h_app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

async fn h_app_build_hash() -> &'static str {
    env!("BUILD_HASH")
}

async fn h_default_system_prompt() -> &'static str {
    config::DEFAULT_SYSTEM_PROMPT
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestConnectionRequest {
    endpoint: String,
    model: String,
    api_key: String,
}

async fn h_test_connection(Json(req): Json<TestConnectionRequest>) -> Response {
    let probe = vec![ChatMessage::text("user", "Reply with the single word: ok")];
    let result = llm::send_chat(&req.endpoint, &req.model, &req.api_key, 0.0, &probe)
        .await
        .map(|reply| reply.trim().to_string())
        .map_err(|e| e.to_string());
    ok_or_400(result)
}

async fn h_load_config() -> Response {
    ok_or_400(load_cfg())
}

#[derive(Deserialize)]
struct SaveConfigRequest {
    cfg: config::AppConfig,
}

async fn h_save_config(Json(req): Json<SaveConfigRequest>) -> Response {
    ok_or_400(config::save(&req.cfg).map_err(|e| e.to_string()))
}

async fn h_get_comfyui_config() -> Response {
    ok_or_400(comfyui::load_or_init().map_err(|e| e.to_string()))
}

#[derive(Deserialize)]
struct SaveComfyuiConfigRequest {
    cfg: comfyui::ComfyUiConfig,
}

async fn h_save_comfyui_config(Json(req): Json<SaveComfyuiConfigRequest>) -> Response {
    ok_or_400(comfyui::save(&req.cfg).map_err(|e| e.to_string()))
}

#[derive(Deserialize)]
struct TestComfyuiGenerationRequest {
    cfg: comfyui::ComfyUiConfig,
}

async fn h_test_comfyui_generation(Json(req): Json<TestComfyuiGenerationRequest>) -> Response {
    let fields = comfyui::ImagePromptFields {
        positive: Some("a red circle on a white background".to_string()),
        ..Default::default()
    };
    let result = comfyui::generate_image(&req.cfg, &fields)
        .await
        .map(|image| {
            let lower = image.filename.to_lowercase();
            let mime = if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
                "image/jpeg"
            } else if lower.ends_with(".webp") {
                "image/webp"
            } else {
                "image/png"
            };
            format!(
                "data:{mime};base64,{}",
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &image.bytes)
            )
        })
        .map_err(|e| e.to_string());
    ok_or_400(result)
}

#[derive(Serialize)]
struct GeneratedImageResponse {
    path: String,
    data_url: String,
    reaction_pending: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateComfyuiImageRequest {
    session_id: String,
    fields: comfyui::ImagePromptFields,
}

async fn h_generate_comfyui_image(Json(req): Json<GenerateComfyuiImageRequest>) -> Response {
    let result: Result<GeneratedImageResponse, String> = async {
        let comfy_cfg = comfyui::load_or_init().map_err(|e| e.to_string())?;
        let image = chat_turn::generate_and_save_image(&comfy_cfg, &req.session_id, &req.fields)
            .await
            .map_err(|e| e.to_string())?;
        Ok(GeneratedImageResponse {
            path: image.path.display().to_string(),
            data_url: image.data_url,
            reaction_pending: comfy_cfg.reaction_mode != comfyui::ReactionMode::Never,
        })
    }
    .await;
    ok_or_400(result)
}

#[derive(Serialize)]
struct TurnReplyResponse {
    text: Option<String>,
    thinking: Option<String>,
}

impl From<chat_turn::TurnReply> for TurnReplyResponse {
    fn from(reply: chat_turn::TurnReply) -> Self {
        Self {
            text: reply.text,
            thinking: reply.thinking,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunImageReactionRequest {
    session_id: String,
    positive_prompt: String,
    image_data_url: String,
}

async fn h_run_image_reaction(Json(req): Json<RunImageReactionRequest>) -> Response {
    let result: Result<TurnReplyResponse, String> = async {
        let comfy_cfg = comfyui::load_or_init().map_err(|e| e.to_string())?;
        let cfg = load_cfg()?;
        Ok(chat_turn::run_and_persist_image_reaction(
            &cfg,
            &req.session_id,
            &req.positive_prompt,
            &req.image_data_url,
            comfy_cfg.reaction_mode,
        )
        .await
        .into())
    }
    .await;
    ok_or_400(result)
}

async fn h_get_searxng_config() -> Response {
    ok_or_400(searxng::load_or_init().map_err(|e| e.to_string()))
}

#[derive(Deserialize)]
struct SaveSearxngConfigRequest {
    cfg: searxng::SearxngConfig,
}

async fn h_save_searxng_config(Json(req): Json<SaveSearxngConfigRequest>) -> Response {
    ok_or_400(searxng::save(&req.cfg).map_err(|e| e.to_string()))
}

#[derive(Deserialize)]
struct TestSearxngSearchRequest {
    cfg: searxng::SearxngConfig,
}

async fn h_test_searxng_search(Json(req): Json<TestSearxngSearchRequest>) -> Response {
    let result = searxng::search(&req.cfg, "test query")
        .await
        .map_err(|e| e.to_string());
    ok_or_400(result)
}

#[derive(Serialize)]
struct WebSearchResponse {
    results: Vec<searxng::SearchResult>,
    search_error: Option<String>,
}

#[derive(Deserialize)]
struct RunWebSearchRequest {
    query: String,
}

async fn h_run_web_search(Json(req): Json<RunWebSearchRequest>) -> Response {
    let searxng_cfg = match searxng::load_or_init() {
        Ok(cfg) => cfg,
        Err(e) => return ok_or_400::<()>(Err(e.to_string())),
    };
    let (results, search_error) = match searxng::search(&searxng_cfg, &req.query).await {
        Ok(results) => (results, None),
        Err(e) => {
            log::warn!("h_run_web_search: search itself failed: {e}");
            (Vec::new(), Some(e.to_string()))
        }
    };
    Json(WebSearchResponse {
        results,
        search_error,
    })
    .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunSearchAnswerRequest {
    session_id: String,
    query: String,
    results: Vec<searxng::SearchResult>,
    search_error: Option<String>,
}

async fn h_run_search_answer(Json(req): Json<RunSearchAnswerRequest>) -> Response {
    let result: Result<TurnReplyResponse, String> = async {
        let cfg = load_cfg()?;
        Ok(chat_turn::run_and_persist_search_answer(
            &cfg,
            &req.session_id,
            &req.query,
            &req.results,
            req.search_error.as_deref(),
        )
        .await
        .into())
    }
    .await;
    ok_or_400(result)
}

async fn h_list_personas() -> Response {
    ok_or_400(persona::list_personas().map_err(|e| e.to_string()))
}

#[derive(Deserialize)]
struct SaveNewPersonaRequest {
    name: String,
    content: String,
}

async fn h_save_new_persona(Json(req): Json<SaveNewPersonaRequest>) -> Response {
    ok_or_400(persona::save_new_persona(&req.name, &req.content).map_err(|e| e.to_string()))
}

#[derive(Deserialize)]
struct NamedRequest {
    name: String,
}

async fn h_delete_persona(Json(req): Json<NamedRequest>) -> Response {
    ok_or_400(persona::delete_persona(&req.name).map_err(|e| e.to_string()))
}

async fn h_get_persona_content(Json(req): Json<NamedRequest>) -> Response {
    ok_or_400(persona::load_persona(&req.name).map_err(|e| e.to_string()))
}

#[derive(Deserialize)]
struct UpdatePersonaRequest {
    name: String,
    content: String,
}

async fn h_update_persona(Json(req): Json<UpdatePersonaRequest>) -> Response {
    ok_or_400(persona::update_persona(&req.name, &req.content).map_err(|e| e.to_string()))
}

async fn h_list_rulesets() -> Response {
    ok_or_400(ruleset::list_rulesets().map_err(|e| e.to_string()))
}

async fn h_get_ruleset_content(Json(req): Json<NamedRequest>) -> Response {
    ok_or_400(ruleset::load_ruleset(&req.name).map_err(|e| e.to_string()))
}

#[derive(Deserialize)]
struct UpdateRulesetRequest {
    name: String,
    content: String,
}

async fn h_update_ruleset(Json(req): Json<UpdateRulesetRequest>) -> Response {
    ok_or_400(ruleset::update_ruleset(&req.name, &req.content).map_err(|e| e.to_string()))
}

async fn h_get_ruleset_example(Json(req): Json<NamedRequest>) -> Response {
    Json(ruleset::example_for(&req.name).map(|s| s.to_string())).into_response()
}

async fn h_list_chat_sessions() -> Response {
    ok_or_400(chat_session::list_sessions().map_err(|e| e.to_string()))
}

#[derive(Deserialize)]
struct CreateChatSessionRequest {
    persona: Option<String>,
}

async fn h_create_chat_session(Json(req): Json<CreateChatSessionRequest>) -> Response {
    ok_or_400(chat_session::create_session(req.persona.as_deref()).map_err(|e| e.to_string()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionIdRequest {
    session_id: String,
}

async fn h_load_chat_session(Json(req): Json<SessionIdRequest>) -> Response {
    let result = chat_session::load_session(&req.session_id)
        .map(|(meta, history)| serde_json::json!({ "meta": meta, "history": history }))
        .map_err(|e| e.to_string());
    ok_or_400(result)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameChatSessionRequest {
    session_id: String,
    title: String,
}

async fn h_rename_chat_session(Json(req): Json<RenameChatSessionRequest>) -> Response {
    ok_or_400(chat_session::rename_session(&req.session_id, &req.title).map_err(|e| e.to_string()))
}

async fn h_delete_chat_session(Json(req): Json<SessionIdRequest>) -> Response {
    ok_or_400(chat_session::delete_session(&req.session_id).map_err(|e| e.to_string()))
}

async fn h_get_chat_state(Json(req): Json<SessionIdRequest>) -> Response {
    Json(chat_session::read_state(&req.session_id)).into_response()
}

#[derive(Serialize)]
struct SendChatMessageResponse {
    reply: String,
    thinking: Option<String>,
    state_updated: bool,
    ruleset_loaded: Option<String>,
    ruleset_error: Option<String>,
    image_prompt_requested: Option<comfyui::ImagePromptFields>,
    web_search_requested: Option<String>,
    dropped: usize,
    condensed: usize,
    summarized: usize,
    summary: Option<String>,
    rewritten_history: Option<Vec<ChatMessage>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendChatMessageRequest {
    session_id: String,
    history: Vec<ChatMessage>,
}

async fn h_send_chat_message(Json(req): Json<SendChatMessageRequest>) -> Response {
    let result: Result<SendChatMessageResponse, String> = async {
        let cfg = load_cfg()?;
        let outcome = chat_turn::run_chat_turn(&cfg, &req.session_id, req.history)
            .await
            .map_err(|e| e.to_string())?;
        Ok(SendChatMessageResponse {
            reply: outcome.reply,
            thinking: outcome.thinking,
            state_updated: outcome.state_updated,
            ruleset_loaded: outcome.ruleset_loaded,
            ruleset_error: outcome.ruleset_error,
            image_prompt_requested: outcome.image_prompt_requested,
            web_search_requested: outcome.web_search_requested,
            dropped: outcome.dropped,
            condensed: outcome.condensed,
            summarized: outcome.summarized,
            summary: outcome.summary,
            rewritten_history: outcome.rewritten_history,
        })
    }
    .await;
    ok_or_400(result)
}

#[derive(Deserialize)]
struct ReadGeneratedImageRequest {
    path: String,
}

async fn h_read_generated_image(Json(req): Json<ReadGeneratedImageRequest>) -> Response {
    ok_or_400(comfyui::read_as_data_url(std::path::Path::new(&req.path)).map_err(|e| e.to_string()))
}

#[derive(Deserialize)]
struct ProbeVisionCapabilityRequest {
    endpoint: String,
    model: String,
}

async fn h_probe_vision_capability(Json(req): Json<ProbeVisionCapabilityRequest>) -> Response {
    Json(llm::probe_vision_capability(&req.endpoint, &req.model).await).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestVisionSupportRequest {
    endpoint: String,
    model: String,
    api_key: String,
}

async fn h_test_vision_support(Json(req): Json<TestVisionSupportRequest>) -> Response {
    let mut probe = ChatMessage::text(
        "user",
        "What color is this image? Answer with just the color name.",
    );
    probe.images = vec![format!(
        "data:image/png;base64,{}",
        crate::VISION_TEST_IMAGE_PNG_BASE64
    )];
    let result = llm::send_chat(&req.endpoint, &req.model, &req.api_key, 0.0, &[probe])
        .await
        .map_err(|e| e.to_string())
        .and_then(|reply| {
            let reply = reply.trim();
            if reply.to_lowercase().contains("red") {
                Ok(format!(
                    "Vision works. {} correctly saw a red image.",
                    req.model
                ))
            } else if reply.is_empty() {
                Err("The model replied with nothing at all.".to_string())
            } else {
                Err(format!(
                    "The model replied but never said \"red\" -- it probably can't see the \
                     image. It said: {reply}"
                ))
            }
        });
    ok_or_400(result)
}

// --- wiring ---

fn api_router() -> Router {
    Router::new()
        .route("/app_version", post(h_app_version))
        .route("/app_build_hash", post(h_app_build_hash))
        .route("/default_system_prompt", post(h_default_system_prompt))
        .route("/test_connection", post(h_test_connection))
        .route("/load_config", post(h_load_config))
        .route("/save_config", post(h_save_config))
        .route("/get_comfyui_config", post(h_get_comfyui_config))
        .route("/save_comfyui_config", post(h_save_comfyui_config))
        .route("/test_comfyui_generation", post(h_test_comfyui_generation))
        .route("/generate_comfyui_image", post(h_generate_comfyui_image))
        .route("/run_image_reaction", post(h_run_image_reaction))
        .route("/get_searxng_config", post(h_get_searxng_config))
        .route("/save_searxng_config", post(h_save_searxng_config))
        .route("/test_searxng_search", post(h_test_searxng_search))
        .route("/run_web_search", post(h_run_web_search))
        .route("/run_search_answer", post(h_run_search_answer))
        .route("/list_personas", post(h_list_personas))
        .route("/save_new_persona", post(h_save_new_persona))
        .route("/delete_persona", post(h_delete_persona))
        .route("/get_persona_content", post(h_get_persona_content))
        .route("/update_persona", post(h_update_persona))
        .route("/list_rulesets", post(h_list_rulesets))
        .route("/get_ruleset_content", post(h_get_ruleset_content))
        .route("/update_ruleset", post(h_update_ruleset))
        .route("/get_ruleset_example", post(h_get_ruleset_example))
        .route("/list_chat_sessions", post(h_list_chat_sessions))
        .route("/create_chat_session", post(h_create_chat_session))
        .route("/load_chat_session", post(h_load_chat_session))
        .route("/rename_chat_session", post(h_rename_chat_session))
        .route("/delete_chat_session", post(h_delete_chat_session))
        .route("/get_chat_state", post(h_get_chat_state))
        .route("/send_chat_message", post(h_send_chat_message))
        .route("/read_generated_image", post(h_read_generated_image))
        .route("/probe_vision_capability", post(h_probe_vision_capability))
        .route("/test_vision_support", post(h_test_vision_support))
}

/// Builds the whole app (static assets + `/api/*`) and serves it forever.
/// An empty `password` serves everything unauthenticated -- "auto mode",
/// a deliberate choice for a trusted network rather than an error to block
/// startup on, but always logged at `warn` so it's never silently the case.
pub async fn run(bind: String, port: u16, password: String) -> anyhow::Result<()> {
    let router = Router::new()
        .route("/", get(serve_index))
        .route("/{*path}", get(serve_named_asset))
        .nest("/api", api_router());

    let router = if password.is_empty() {
        log::warn!("server.json has no password set -- serving without authentication");
        router
    } else {
        let password: Arc<str> = Arc::from(password.as_str());
        router.layer(middleware::from_fn_with_state(password, require_password))
    };

    let addr: std::net::SocketAddr = format!("{bind}:{port}").parse()?;
    log::info!("web server listening on http://{addr} (chat mode only)");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}
