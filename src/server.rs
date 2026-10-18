//! Chat mode, ported to a plain HTTP server -- `llm-assistant --server`. No
//! file-ops/sandbox mode here at all: it depends on `bubblewrap` sandboxing
//! tied to a folder picked through the desktop GUI, neither of which means
//! anything to a remote browser client.
//!
//! Every chat route is a thin adapter over `chat_commands::api`, the same
//! implementation the `#[tauri::command]` wrappers call, so there is no
//! business logic duplicated here, only its HTTP shape. Every handler reloads
//! its config fresh from disk per call rather than caching anything.
//!
//! `ui/`'s static assets are compiled directly into the binary
//! (`WebAssets`, via `rust-embed`) rather than read from a loose folder on
//! disk: this mode has no `AppHandle`, so it can't reach Tauri's own asset
//! bundling, and a path like "next to the binary" doesn't exist for an
//! AppImage (mounted from a temporary squashfs). Embedding makes `--server`
//! work identically from a dev build, a `.deb` install, or an AppImage.
//!
//! The frontend's `invoke()` forks to `fetch("/api/<command>")` when
//! `window.__TAURI__` doesn't exist, POSTing the same camelCase-keyed
//! argument object Tauri's own invoke sends, so every request DTO below uses
//! `#[serde(rename_all = "camelCase")]`.

use crate::chat_commands::{api, ChatStreamEvent};
use crate::llm::ChatMessage;
use crate::paths::{app_config_dir, write_atomic};
use crate::{chat_turn, comfyui, config, llm, searxng};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// open, unauthenticated access, which `run` only allows on a loopback
    /// bind (or with `allow_no_auth`).
    #[serde(default)]
    pub password: String,
    /// Explicit opt-in to serve without a password on a non-loopback bind.
    #[serde(default)]
    pub allow_no_auth: bool,
    /// Extra `Host` header values accepted beyond localhost and the bind
    /// address -- e.g. a LAN hostname or reverse-proxy domain. Ports ignored.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
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
            allow_no_auth: false,
            allowed_hosts: Vec::new(),
        }
    }
}

/// A parse (or any non-"missing") error is an error, never defaults: the
/// defaults have an empty password, so a typo in `server.json` must not
/// silently turn authentication off. Defaults are written only when the file
/// doesn't exist.
pub fn load_or_init() -> anyhow::Result<ServerConfig> {
    load_from(&config_path())
}

fn load_from(path: &Path) -> anyhow::Result<ServerConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{} is not valid ({e})", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cfg = ServerConfig::default();
            save_to(path, &cfg)?;
            Ok(cfg)
        }
        Err(e) => Err(anyhow::anyhow!("can't read {}: {e}", path.display())),
    }
}

/// 0600: the file holds the password.
fn save_to(path: &Path, cfg: &ServerConfig) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(
        path,
        serde_json::to_string_pretty(cfg)?.as_bytes(),
        Some(0o600),
    )?;
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

/// The single top-level fallback for anything that doesn't match `/` or a
/// registered `/api/*` route -- deliberately not a nested `.fallback()` on
/// the `/api` sub-router. A `Router::nest()`'d sub-router is flattened into
/// the same route tree as everything else at build time; its own
/// `.fallback()` is not honored the way a plain, non-nested router's is; an
/// unmatched `/api/...` request instead falls through to whatever *other*
/// pattern in the whole tree happens to match that path, which used to be a
/// static-asset wildcard registered on `/` -- reporting a confusing `405
/// Method Not Allowed` (that pattern exists, just only for `GET`) on a
/// `POST` to a command that was never ported to `--server`, instead of the
/// `404` this actually is. One fallback, with an explicit prefix check,
/// sidesteps relying on nested-fallback semantics at all.
async fn fallback(uri: axum::http::Uri) -> Response {
    match uri.path().strip_prefix("/api/") {
        Some(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such command" })),
        )
            .into_response(),
        None => serve_asset(uri.path().trim_start_matches('/')).await,
    }
}

// --- auth: a cookie-backed session behind a custom login form in `ui/`,
// not the browser's own HTTP Basic Auth prompt. An earlier version used
// Basic Auth -- simpler on the server, since the browser owns the whole
// credential UI -- but that UI turned out to be exactly the problem: a
// plain top-level navigation to a password-protected `--server` sometimes
// rendered the raw 401 body ("password required") as the page instead of
// ever prompting, with no way for this app to control or retry that. A
// form the app itself owns removes that dependency entirely.

/// One shared, in-memory map of currently-valid session tokens to when each
/// was issued. Cleared on every server restart by construction (nothing
/// persists it) -- losing all sessions on restart is an acceptable, simple
/// default for a personal server; logging back in costs one password entry.
/// `std::time::Instant` rather than a wall-clock timestamp since nothing
/// here is ever serialized -- monotonic and immune to clock changes, and
/// simpler than reasoning about a clock jump mid-run.
#[derive(Clone)]
struct AuthState {
    /// Empty means no password is configured -- see `ServerConfig::password`'s
    /// doc comment. Checked directly by `h_login`/`h_auth_check`; `run`
    /// decides whether to apply `require_session` at all based on this.
    password: Arc<str>,
    sessions: Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>>,
    logins: Arc<std::sync::Mutex<LoginGuard>>,
}

const LOGIN_MAX_FAILURES: usize = 10;
const LOGIN_FAILURE_WINDOW: Duration = Duration::from_secs(600);
const LOGIN_LOCKOUT: Duration = Duration::from_secs(300);
const LOGIN_FAILURE_DELAY: Duration = Duration::from_secs(1);

/// Global (not per-IP: behind a reverse proxy every client shares one
/// address anyway) failed-login tracker: too many failures inside the window
/// locks all logins out for a while.
#[derive(Default)]
struct LoginGuard {
    failures: std::collections::VecDeque<Instant>,
    locked_until: Option<Instant>,
}

impl LoginGuard {
    fn is_locked(&mut self, now: Instant) -> bool {
        match self.locked_until {
            Some(until) if now < until => true,
            Some(_) => {
                self.locked_until = None;
                false
            }
            None => false,
        }
    }

    fn record_failure(&mut self, now: Instant) {
        while self
            .failures
            .front()
            .is_some_and(|&t| now.duration_since(t) > LOGIN_FAILURE_WINDOW)
        {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
        if self.failures.len() >= LOGIN_MAX_FAILURES {
            self.locked_until = Some(now + LOGIN_LOCKOUT);
            self.failures.clear();
        }
    }
}

/// Compares over the longer length with no early exit, so timing doesn't
/// reveal how much of the password matched (length still leaks, which is
/// fine).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    std::hint::black_box(diff) == 0
}

/// `AppConfig::server_session_expiry_days`, read fresh (not cached) so a
/// change in Settings takes effect on the next check/sweep without a
/// restart -- same "never trust a stale copy" convention every other route
/// in this file already follows. Falls back to the field's own default if
/// config can't be loaded at all, rather than failing a session check over
/// an unrelated disk error. `Duration::ZERO` means "never expire."
fn session_expiry() -> std::time::Duration {
    let days = config::load_or_init()
        .map(|cfg| cfg.server_session_expiry_days)
        .unwrap_or_else(|_| config::AppConfig::default().server_session_expiry_days);
    std::time::Duration::from_secs(u64::from(days) * 86400)
}

const SESSION_COOKIE_NAME: &str = "llm_session";

fn session_token_from_cookies(headers: &HeaderMap) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie_header.split(';').find_map(|kv| {
        kv.trim()
            .strip_prefix(SESSION_COOKIE_NAME)?
            .strip_prefix('=')
            .map(str::to_string)
    })
}

/// Self-cleaning: a token found but past its expiry is removed right here,
/// not just reported invalid -- the request that finally notices an old
/// token is expired is also the one that takes it out of the map, rather
/// than leaving that to the periodic sweep alone (`spawn_session_sweep`
/// below still exists for a token nobody ever presents again).
fn session_is_valid(headers: &HeaderMap, auth: &AuthState) -> bool {
    let Some(token) = session_token_from_cookies(headers) else {
        return false;
    };
    // Config read (disk IO) must happen before taking the mutex.
    let expiry = session_expiry();
    let mut sessions = auth.sessions.lock().unwrap();
    let Some(&issued_at) = sessions.get(&token) else {
        return false;
    };
    if expiry.is_zero() || issued_at.elapsed() < expiry {
        true
    } else {
        sessions.remove(&token);
        false
    }
}

/// Catches a session nobody ever comes back to present again (an abandoned
/// login, a one-off test), which `session_is_valid`'s own self-cleaning
/// can't reach since it only ever runs when that specific token shows up on
/// a request. An hourly sweep is frequent enough that even the shortest
/// sane expiry setting doesn't linger for long, and cheap enough (one
/// `HashMap::retain` over what's realistically at most a handful of live
/// sessions for a personal server) not to matter at any interval this
/// coarse. Not spawned at all when no password is configured -- `h_login`
/// is unreachable then, so `auth.sessions` never gets an entry to sweep.
fn spawn_session_sweep(auth: AuthState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            let expiry = session_expiry();
            if expiry.is_zero() {
                continue;
            }
            auth.sessions
                .lock()
                .unwrap()
                .retain(|_, issued_at| issued_at.elapsed() < expiry);
        }
    });
}

/// 256 bits from the OS RNG, hex-encoded.
fn generate_session_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("OS RNG failed: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// `max_age_secs: None` makes a session cookie (dropped when the browser
/// closes); `Some(0)` clears it.
fn session_cookie(value: &str, max_age_secs: Option<u64>) -> String {
    let mut cookie = format!("{SESSION_COOKIE_NAME}={value}; HttpOnly; Path=/; SameSite=Lax");
    if let Some(secs) = max_age_secs {
        cookie.push_str(&format!("; Max-Age={secs}"));
    }
    cookie
}

async fn require_session(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if session_is_valid(&headers, &auth) {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "unauthorized" })),
        )
            .into_response()
    }
}

/// Public even when a password is set -- this is what the login overlay
/// itself calls, both to decide whether to show up at all (no password
/// configured, or this tab already has a valid session) and, on a fresh
/// visit, whether it needs to.
#[derive(Serialize)]
struct AuthCheckResponse {
    required: bool,
    authenticated: bool,
}

async fn h_auth_check(State(auth): State<AuthState>, headers: HeaderMap) -> Response {
    let required = !auth.password.is_empty();
    let authenticated = required && session_is_valid(&headers, &auth);
    Json(AuthCheckResponse {
        required,
        authenticated,
    })
    .into_response()
}

#[derive(Deserialize)]
struct LoginRequest {
    password: String,
}

/// Public even when a password is set -- see `AuthCheckResponse`'s doc
/// comment; this is the one gated action a session-less request is still
/// allowed to take.
async fn h_login(State(auth): State<AuthState>, Json(req): Json<LoginRequest>) -> Response {
    let unauthorized = || {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "wrong password" })),
        )
            .into_response()
    };
    if auth.password.is_empty() {
        return unauthorized();
    }
    if auth.logins.lock().unwrap().is_locked(Instant::now()) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "error": "too many failed logins -- try again later" })),
        )
            .into_response();
    }
    if !ct_eq(req.password.as_bytes(), auth.password.as_bytes()) {
        auth.logins.lock().unwrap().record_failure(Instant::now());
        tokio::time::sleep(LOGIN_FAILURE_DELAY).await;
        return unauthorized();
    }
    let token = match generate_session_token() {
        Ok(token) => token,
        Err(e) => {
            log::error!("login: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "could not create a session" })),
            )
                .into_response();
        }
    };
    // Read before taking the sessions mutex (disk IO).
    let expiry = session_expiry();
    auth.logins.lock().unwrap().failures.clear();
    auth.sessions
        .lock()
        .unwrap()
        .insert(token.clone(), Instant::now());
    let max_age = (!expiry.is_zero()).then_some(expiry.as_secs());
    (
        [(header::SET_COOKIE, session_cookie(&token, max_age))],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

/// Public like `h_login`: logging out with an already-expired session should
/// still clear the cookie.
async fn h_logout(State(auth): State<AuthState>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token_from_cookies(&headers) {
        auth.sessions.lock().unwrap().remove(&token);
    }
    (
        [(header::SET_COOKIE, session_cookie("", Some(0)))],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

// --- host allowlist (DNS-rebinding guard) and response headers ---

struct HostPolicy {
    bind: String,
    allowed: Vec<String>,
}

/// Lowercased host with port, brackets and any trailing dot removed. A bare
/// IPv6 literal (several colons, no brackets -- only valid in config, never
/// in a `Host` header) is kept whole.
fn normalize_host(raw: &str) -> String {
    let h = raw.trim().to_ascii_lowercase();
    let host = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else if h.matches(':').count() > 1 {
        h.as_str()
    } else {
        h.split(':').next().unwrap_or("")
    };
    host.trim_end_matches('.').to_string()
}

impl HostPolicy {
    /// A rebinding attack needs a *name* the attacker controls, so when the
    /// server listens on a wildcard address any IP-literal `Host` is fine
    /// (that is how a LAN client reaches it); names still need listing.
    fn allows(&self, host_header: &str) -> bool {
        let host = normalize_host(host_header);
        if host.is_empty() {
            return false;
        }
        let bind = normalize_host(&self.bind);
        let wildcard = bind
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_unspecified());
        matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1")
            || (!wildcard && host == bind)
            || (wildcard && host.parse::<std::net::IpAddr>().is_ok())
            || self.allowed.iter().any(|a| normalize_host(a) == host)
    }
}

async fn host_guard(
    State(policy): State<Arc<HostPolicy>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let allowed = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| policy.allows(h));
    if allowed {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "host not allowed").into_response()
    }
}

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; img-src 'self' data: blob:; \
    style-src 'self' 'unsafe-inline'; script-src 'self'; connect-src 'self'; frame-ancestors 'none'";

async fn security_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
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

// Wrapped in `Json` rather than returned bare: axum's `IntoResponse` for a
// plain `&str` sends it as an unquoted `text/plain` body, but the frontend's
// `invoke` shim always calls `res.json()` on the response (matching what
// Tauri's own `invoke` already does for every return type, strings
// included -- it's real IPC, always JSON-serialized regardless of shape).
// An unquoted body like `1.14.1` isn't valid JSON on its own (two decimal
// points aren't a valid JSON number), so `res.json()` threw a SyntaxError
// and the whole call silently failed -- exactly why neither the version nor
// the rail badge ever appeared outside Tauri.
async fn h_app_version() -> Json<&'static str> {
    Json(env!("CARGO_PKG_VERSION"))
}

async fn h_app_build_hash() -> Json<&'static str> {
    Json(env!("BUILD_HASH"))
}

async fn h_default_system_prompt() -> Json<&'static str> {
    Json(config::DEFAULT_SYSTEM_PROMPT)
}

/// The browser never sees the stored key (see `h_load_config`), so its
/// Settings "test" buttons send an empty one. Fills it in from disk, but only
/// when the request targets the already-saved endpoint -- otherwise a client
/// could point `endpoint` at its own server and collect the key.
fn effective_api_key(endpoint: &str, sent: &str) -> String {
    if !sent.is_empty() {
        return sent.to_string();
    }
    match config::load_or_init() {
        Ok(cfg) if cfg.endpoint.trim() == endpoint.trim() => cfg.api_key,
        _ => String::new(),
    }
}

/// Same text as `main.rs`'s `first_line` (kept separate: `main.rs` isn't
/// reachable from here without a shared module).
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > 80 {
        format!("{}…", line.chars().take(80).collect::<String>())
    } else {
        line.to_string()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestConnectionRequest {
    endpoint: String,
    model: String,
    api_key: String,
}

/// Same replies as `main.rs`'s `test_connection`.
async fn h_test_connection(Json(req): Json<TestConnectionRequest>) -> Response {
    let probe = vec![ChatMessage::text("user", "Reply with the single word: ok")];
    let api_key = effective_api_key(&req.endpoint, &req.api_key);
    let result = llm::send_chat(&req.endpoint, &req.model, &api_key, 0.0, &probe)
        .await
        .map(|reply| {
            let reply = reply.trim();
            if reply.is_empty() {
                "Connected, but the model returned an empty reply.".to_string()
            } else {
                format!("Connected. {} replied: {}", req.model, first_line(reply))
            }
        })
        .map_err(|e| e.to_string());
    ok_or_400(result)
}

/// The API key never leaves the server: `api_key` is blanked and
/// `apiKeySet` says whether one is stored.
async fn h_load_config() -> Response {
    let result = load_cfg().and_then(|cfg| {
        let key_set = !cfg.api_key.is_empty();
        let mut value = serde_json::to_value(&cfg).map_err(|e| e.to_string())?;
        value["api_key"] = serde_json::Value::String(String::new());
        value["apiKeySet"] = serde_json::Value::Bool(key_set);
        Ok(value)
    });
    ok_or_400(result)
}

/// The chat-relevant subset of `AppConfig` a browser may change. Anything not
/// listed -- `granted_paths`, `auto_approve`, `disable_builtin_rules`,
/// `sandbox_trash_dir`, `max_auto_steps`, `confirm_fade_after`, `memory_*` --
/// is file-ops policy that only the desktop app may touch, and is dropped
/// here no matter what the client sends.
#[derive(Deserialize, Default)]
struct ServerSaveConfig {
    endpoint: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    system_prompt: Option<String>,
    temperature: Option<f32>,
    chat_temperature: Option<f32>,
    max_context_tokens: Option<u32>,
    summarize_before_dropping: Option<bool>,
    chat_state_max_tokens: Option<u32>,
    chat_show_thinking: Option<bool>,
    chat_persist_thinking: Option<bool>,
    chat_hide_narration: Option<bool>,
    chat_stream_replies: Option<bool>,
    chat_auto_images: Option<bool>,
    chat_recall: Option<bool>,
    chat_recall_scope: Option<String>,
    chat_recall_embedding_model: Option<String>,
    chat_recall_max: Option<u32>,
    chat_recall_min_similarity: Option<f32>,
    server_session_expiry_days: Option<u32>,
}

impl ServerSaveConfig {
    fn merge_into(self, cfg: &mut config::AppConfig) {
        macro_rules! set {
            ($($field:ident),*) => {
                $(if let Some(v) = self.$field { cfg.$field = v; })*
            };
        }
        set!(
            endpoint,
            model,
            system_prompt,
            temperature,
            chat_temperature,
            max_context_tokens,
            summarize_before_dropping,
            chat_state_max_tokens,
            chat_show_thinking,
            chat_persist_thinking,
            chat_hide_narration,
            chat_stream_replies,
            chat_auto_images,
            chat_recall,
            chat_recall_scope,
            chat_recall_embedding_model,
            chat_recall_max,
            chat_recall_min_similarity,
            server_session_expiry_days
        );
        // `h_load_config` blanks the key, so the browser echoes "" back on
        // every save -- that must keep the stored key, not wipe it.
        if let Some(key) = self.api_key.filter(|k| !k.is_empty()) {
            cfg.api_key = key;
        }
    }
}

#[derive(Deserialize)]
struct SaveConfigRequest {
    cfg: ServerSaveConfig,
}

async fn h_save_config(Json(req): Json<SaveConfigRequest>) -> Response {
    let result = load_cfg().and_then(|mut cfg| {
        req.cfg.merge_into(&mut cfg);
        config::save(&cfg).map_err(|e| e.to_string())
    });
    ok_or_400(result)
}

async fn h_get_comfyui_config() -> Response {
    ok_or_400(api::get_comfyui_config())
}

#[derive(Deserialize)]
struct SaveComfyuiConfigRequest {
    cfg: comfyui::ComfyUiConfig,
}

/// `output_dir` stays whatever is on disk: it decides where generated files
/// are written and read back from, so only the desktop app may change it.
async fn h_save_comfyui_config(Json(req): Json<SaveComfyuiConfigRequest>) -> Response {
    let result = comfyui::load_or_init()
        .map_err(|e| e.to_string())
        .and_then(|on_disk| {
            let mut cfg = req.cfg;
            cfg.output_dir = on_disk.output_dir;
            api::save_comfyui_config(cfg)
        });
    ok_or_400(result)
}

#[derive(Deserialize)]
struct TestComfyuiGenerationRequest {
    cfg: comfyui::ComfyUiConfig,
}

async fn h_test_comfyui_generation(Json(req): Json<TestComfyuiGenerationRequest>) -> Response {
    ok_or_400(api::test_comfyui_generation(req.cfg).await)
}

async fn h_get_searxng_config() -> Response {
    ok_or_400(api::get_searxng_config())
}

#[derive(Deserialize)]
struct SaveSearxngConfigRequest {
    cfg: searxng::SearxngConfig,
}

async fn h_save_searxng_config(Json(req): Json<SaveSearxngConfigRequest>) -> Response {
    ok_or_400(api::save_searxng_config(req.cfg))
}

#[derive(Deserialize)]
struct TestSearxngSearchRequest {
    cfg: searxng::SearxngConfig,
}

async fn h_test_searxng_search(Json(req): Json<TestSearxngSearchRequest>) -> Response {
    ok_or_400(api::test_searxng_search(req.cfg).await)
}

async fn h_list_personas() -> Response {
    ok_or_400(api::list_personas())
}

#[derive(Deserialize)]
struct SaveNewPersonaRequest {
    name: String,
    content: String,
}

async fn h_save_new_persona(Json(req): Json<SaveNewPersonaRequest>) -> Response {
    ok_or_400(api::save_new_persona(req.name, req.content))
}

#[derive(Deserialize)]
struct NamedRequest {
    name: String,
}

async fn h_delete_persona(Json(req): Json<NamedRequest>) -> Response {
    ok_or_400(api::delete_persona(req.name))
}

async fn h_get_persona_content(Json(req): Json<NamedRequest>) -> Response {
    ok_or_400(api::get_persona_content(req.name))
}

#[derive(Deserialize)]
struct UpdatePersonaRequest {
    name: String,
    content: String,
}

async fn h_update_persona(Json(req): Json<UpdatePersonaRequest>) -> Response {
    ok_or_400(api::update_persona(req.name, req.content))
}

async fn h_list_rulesets() -> Response {
    ok_or_400(api::list_rulesets())
}

async fn h_get_ruleset_content(Json(req): Json<NamedRequest>) -> Response {
    ok_or_400(api::get_ruleset_content(req.name))
}

#[derive(Deserialize)]
struct UpdateRulesetRequest {
    name: String,
    content: String,
}

async fn h_update_ruleset(Json(req): Json<UpdateRulesetRequest>) -> Response {
    ok_or_400(api::update_ruleset(req.name, req.content))
}

async fn h_get_ruleset_example(Json(req): Json<NamedRequest>) -> Response {
    Json(api::get_ruleset_example(req.name)).into_response()
}

async fn h_list_chat_sessions() -> Response {
    ok_or_400(api::list_chat_sessions())
}

#[derive(Deserialize)]
struct CreateChatSessionRequest {
    persona: Option<String>,
}

async fn h_create_chat_session(Json(req): Json<CreateChatSessionRequest>) -> Response {
    ok_or_400(api::create_chat_session(req.persona))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionIdRequest {
    session_id: String,
}

async fn h_load_chat_session(Json(req): Json<SessionIdRequest>) -> Response {
    ok_or_400(api::load_chat_session(req.session_id))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenameChatSessionRequest {
    session_id: String,
    title: String,
}

async fn h_rename_chat_session(Json(req): Json<RenameChatSessionRequest>) -> Response {
    ok_or_400(api::rename_chat_session(req.session_id, req.title).await)
}

async fn h_delete_chat_session(Json(req): Json<SessionIdRequest>) -> Response {
    ok_or_400(api::delete_chat_session(req.session_id).await)
}

async fn h_get_chat_state(Json(req): Json<SessionIdRequest>) -> Response {
    Json(api::get_chat_state(req.session_id)).into_response()
}

async fn h_get_chat_raw_state(Json(req): Json<SessionIdRequest>) -> Response {
    Json(api::get_chat_raw_state(req.session_id)).into_response()
}

fn sse_event_for(event: &ChatStreamEvent) -> axum::response::sse::Event {
    axum::response::sse::Event::default()
        .json_data(event)
        .unwrap_or_else(|e| {
            axum::response::sse::Event::default().data(format!(
                r#"{{"type":"error","message":"serialize failed: {e}"}}"#
            ))
        })
}

/// Unlike every other handler this can't use `ok_or_400`: once the SSE
/// response is returned the 200 status is committed, so any failure has to be
/// an `error` event inside the stream. The turn runs in its own task, so a
/// client that disconnects mid-stream doesn't lose it.
async fn h_send_chat_message_streaming(Json(req): Json<chat_turn::SendInput>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<
        Result<axum::response::sse::Event, std::convert::Infallible>,
    >();
    tokio::spawn(async move {
        api::stream_chat_message(req, |event| {
            let _ = tx.send(Ok(sse_event_for(&event)));
        })
        .await;
    });
    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
    axum::response::sse::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

async fn h_stop_chat_generation(Json(req): Json<SessionIdRequest>) -> Response {
    Json(api::stop_chat_generation(req.session_id)).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunChatFollowupRequest {
    session_id: String,
    assistant_message_id: String,
    photo_forced: bool,
    photo_hint: Option<String>,
}

async fn h_run_chat_followup(Json(req): Json<RunChatFollowupRequest>) -> Response {
    ok_or_400(
        api::run_chat_followup(
            req.session_id,
            req.assistant_message_id,
            req.photo_forced,
            req.photo_hint,
        )
        .await,
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateChatImageRequest {
    session_id: String,
    message_id: String,
    fields: comfyui::ImagePromptFields,
    vision: Option<bool>,
}

async fn h_generate_chat_image(Json(req): Json<GenerateChatImageRequest>) -> Response {
    ok_or_400(
        api::generate_chat_image(req.session_id, req.message_id, req.fields, req.vision).await,
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunChatWebSearchRequest {
    session_id: String,
    query: String,
}

async fn h_run_chat_web_search(Json(req): Json<RunChatWebSearchRequest>) -> Response {
    ok_or_400(api::run_chat_web_search(req.session_id, req.query).await)
}

#[derive(Deserialize)]
struct ReadGeneratedImageRequest {
    path: String,
}

async fn h_read_generated_image(Json(req): Json<ReadGeneratedImageRequest>) -> Response {
    ok_or_400(api::read_generated_image(req.path))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestVisionSupportRequest {
    endpoint: String,
    model: String,
    api_key: String,
}

async fn h_test_vision_support(Json(req): Json<TestVisionSupportRequest>) -> Response {
    let api_key = effective_api_key(&req.endpoint, &req.api_key);
    ok_or_400(api::test_vision_support(req.endpoint, req.model, api_key).await)
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
        .route("/get_searxng_config", post(h_get_searxng_config))
        .route("/save_searxng_config", post(h_save_searxng_config))
        .route("/test_searxng_search", post(h_test_searxng_search))
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
        .route("/get_chat_raw_state", post(h_get_chat_raw_state))
        .route(
            "/send_chat_message_streaming",
            post(h_send_chat_message_streaming),
        )
        .route("/stop_chat_generation", post(h_stop_chat_generation))
        .route("/run_chat_followup", post(h_run_chat_followup))
        .route("/generate_chat_image", post(h_generate_chat_image))
        .route("/run_chat_web_search", post(h_run_chat_web_search))
        .route("/read_generated_image", post(h_read_generated_image))
        .route("/test_vision_support", post(h_test_vision_support))
        // Attached images arrive as base64 data URLs.
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
}

fn is_loopback_bind(bind: &str) -> bool {
    let bind = bind.trim().trim_start_matches('[').trim_end_matches(']');
    bind.eq_ignore_ascii_case("localhost")
        || bind
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// An open (passwordless) server is only acceptable where nobody else can
/// reach it, or when `allow_no_auth` says it's deliberate.
fn check_auth_policy(bind: &str, password_empty: bool, allow_no_auth: bool) -> anyhow::Result<()> {
    if password_empty && !is_loopback_bind(bind) && !allow_no_auth {
        anyhow::bail!(
            "refusing to serve on {bind} without a password -- set \"password\" in server.json, \
             bind to 127.0.0.1, or set \"allow_no_auth\": true to serve openly on purpose"
        );
    }
    Ok(())
}

/// Builds the whole app (static assets + `/api/*`) and serves it forever.
/// An empty `password` serves everything unauthenticated, but only on a
/// loopback bind unless `server.json` sets `allow_no_auth` (see
/// `check_auth_policy`), and always logged at `warn`. Static assets and
/// `/api/auth_check`/`/api/login`/`/api/logout` are always public
/// regardless (the login overlay itself is one of those assets, and has to
/// reach both routes before any session exists to show up at all); every
/// other `/api/*` route sits behind `require_session` only when a password
/// is actually configured. Every request, public or not, must pass
/// `host_guard` first.
pub async fn run(bind: String, port: u16, password: String) -> anyhow::Result<()> {
    let file_cfg = load_or_init()?;
    check_auth_policy(&bind, password.is_empty(), file_cfg.allow_no_auth)?;
    let auth = AuthState {
        password: Arc::from(password.as_str()),
        sessions: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        logins: Arc::new(std::sync::Mutex::new(LoginGuard::default())),
    };

    let public_api = Router::new()
        .route("/auth_check", get(h_auth_check))
        .route("/login", post(h_login))
        .route("/logout", post(h_logout))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(auth.clone());

    let mut protected_api = api_router();
    if auth.password.is_empty() {
        log::warn!("server.json has no password set -- serving without authentication");
    } else {
        protected_api = protected_api.layer(middleware::from_fn_with_state(
            auth.clone(),
            require_session,
        ));
        spawn_session_sweep(auth.clone());
    }

    let policy = Arc::new(HostPolicy {
        bind: bind.clone(),
        allowed: file_cfg.allowed_hosts,
    });
    let router = Router::new()
        .route("/", get(serve_index))
        .nest("/api", public_api.merge(protected_api))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(policy, host_guard))
        .layer(middleware::from_fn(security_headers));

    let host = bind.trim_start_matches('[').trim_end_matches(']');
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    log::info!(
        "web server listening on http://{} (chat mode only)",
        listener.local_addr()?
    );
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(bind: &str, allowed: &[&str]) -> HostPolicy {
        HostPolicy {
            bind: bind.to_string(),
            allowed: allowed.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn constant_time_eq_matches_plain_equality() {
        assert!(ct_eq(b"hunter2", b"hunter2"));
        assert!(ct_eq(b"", b""));
        assert!(!ct_eq(b"hunter2", b"hunter3"));
        assert!(!ct_eq(b"hunter", b"hunter2"));
        assert!(!ct_eq(b"hunter2", b"hunter"));
        assert!(!ct_eq(b"", b"x"));
    }

    #[test]
    fn host_allowlist_matching() {
        let p = policy("127.0.0.1", &["chat.example.com", "::1"]);
        for ok in [
            "localhost",
            "LOCALHOST:9333",
            "127.0.0.1:9333",
            "[::1]:9333",
            "[::1]",
            "chat.example.com",
            "Chat.Example.com:443",
        ] {
            assert!(p.allows(ok), "{ok} should be allowed");
        }
        for bad in [
            "evil.com",
            "evil.com:9333",
            "192.168.1.5:9333",
            "",
            "localhost.evil.com",
        ] {
            assert!(!p.allows(bad), "{bad:?} should be rejected");
        }
        // A specific LAN bind address is itself allowed...
        assert!(policy("192.168.1.5", &[]).allows("192.168.1.5:9333"));
        // ...a wildcard bind accepts IP literals (no rebinding via an IP) but not names.
        let wild = policy("0.0.0.0", &[]);
        assert!(wild.allows("10.0.0.7:9333"));
        assert!(!wild.allows("evil.com"));
    }

    #[test]
    fn server_json_parse_error_is_an_error_and_missing_file_gets_defaults() {
        let dir =
            std::env::temp_dir().join(format!("llm-assistant-server-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{ \"password\": \"x\" ").unwrap();
        assert!(load_from(&bad).is_err());
        assert!(
            std::fs::read_to_string(&bad).unwrap().contains("password"),
            "not overwritten"
        );

        let missing = dir.join("new.json");
        let cfg = load_from(&missing).unwrap();
        assert!(cfg.password.is_empty() && !cfg.allow_no_auth);
        assert!(missing.exists());

        let partial = dir.join("partial.json");
        std::fs::write(&partial, r#"{"password":"pw","allowed_hosts":["a.b"]}"#).unwrap();
        let cfg = load_from(&partial).unwrap();
        assert_eq!(cfg.password, "pw");
        assert_eq!(cfg.port, 9333);
        assert_eq!(cfg.allowed_hosts, vec!["a.b".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_server_needs_loopback_or_explicit_opt_in() {
        assert!(check_auth_policy("127.0.0.1", true, false).is_ok());
        assert!(check_auth_policy("127.5.5.5", true, false).is_ok());
        assert!(check_auth_policy("::1", true, false).is_ok());
        assert!(check_auth_policy("localhost", true, false).is_ok());
        assert!(check_auth_policy("0.0.0.0", true, false).is_err());
        assert!(check_auth_policy("192.168.1.5", true, false).is_err());
        assert!(check_auth_policy("0.0.0.0", true, true).is_ok());
        assert!(check_auth_policy("0.0.0.0", false, false).is_ok());
    }

    #[test]
    fn login_lockout_after_repeated_failures_then_expires() {
        let t0 = Instant::now();
        let mut guard = LoginGuard::default();
        for i in 0..LOGIN_MAX_FAILURES {
            assert!(!guard.is_locked(t0), "locked early at {i}");
            guard.record_failure(t0 + Duration::from_secs(i as u64));
        }
        let t1 = t0 + Duration::from_secs(20);
        assert!(guard.is_locked(t1));
        assert!(guard.is_locked(t1 + Duration::from_secs(200)));
        assert!(!guard.is_locked(t1 + LOGIN_LOCKOUT + Duration::from_secs(1)));
    }

    #[test]
    fn old_login_failures_age_out_of_the_window() {
        let t0 = Instant::now();
        let mut guard = LoginGuard::default();
        for _ in 0..LOGIN_MAX_FAILURES - 1 {
            guard.record_failure(t0);
        }
        guard.record_failure(t0 + LOGIN_FAILURE_WINDOW + Duration::from_secs(1));
        assert!(!guard.is_locked(t0 + LOGIN_FAILURE_WINDOW + Duration::from_secs(2)));
    }

    #[test]
    fn session_tokens_are_64_hex_chars_and_unique() {
        let a = generate_session_token().unwrap();
        let b = generate_session_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn cookie_max_age_follows_the_setting() {
        assert!(!session_cookie("t", None).contains("Max-Age"));
        assert!(session_cookie("t", Some(86400)).ends_with("Max-Age=86400"));
        assert!(session_cookie("", Some(0)).contains("Max-Age=0"));
    }

    #[test]
    fn http_save_config_never_touches_file_ops_policy_or_wipes_the_key() {
        let mut cfg = config::AppConfig {
            api_key: "secret".into(),
            auto_approve: vec!["ls".into()],
            max_auto_steps: 7,
            disable_builtin_rules: false,
            memory_enabled: true,
            ..Default::default()
        };
        let incoming: ServerSaveConfig = serde_json::from_str(
            r#"{"endpoint":"http://x/v1","api_key":"","chat_temperature":0.3,"chat_auto_images":false,
                "chat_recall":false,"chat_recall_embedding_model":"nomic-embed-text","chat_recall_max":2,
                "auto_approve":["sh","rm"],"granted_paths":[{"path":"/","note":"","read_write":true}],
                "disable_builtin_rules":true,"sandbox_trash_dir":"/etc","max_auto_steps":0,
                "confirm_fade_after":0,"memory_enabled":false,"memory_max_tokens":1}"#,
        )
        .unwrap();
        incoming.merge_into(&mut cfg);
        assert_eq!(cfg.endpoint, "http://x/v1");
        assert!(!cfg.chat_auto_images);
        assert!(!cfg.chat_recall && cfg.chat_recall_max == 2);
        assert_eq!(cfg.chat_recall_embedding_model, "nomic-embed-text");
        assert!((cfg.chat_temperature - 0.3).abs() < 1e-6);
        assert_eq!(
            cfg.api_key, "secret",
            "empty key must not wipe the stored one"
        );
        assert_eq!(cfg.auto_approve, vec!["ls".to_string()]);
        assert!(cfg.granted_paths.is_empty());
        assert!(!cfg.disable_builtin_rules);
        assert_eq!(cfg.max_auto_steps, 7);
        assert!(cfg.memory_enabled);
        assert!(cfg.sandbox_trash_dir.is_empty());

        let replace: ServerSaveConfig = serde_json::from_str(r#"{"api_key":"new"}"#).unwrap();
        replace.merge_into(&mut cfg);
        assert_eq!(cfg.api_key, "new");
    }
}
