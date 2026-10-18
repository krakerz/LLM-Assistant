use crate::paths::app_config_dir;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantedPath {
    pub path: String,
    pub note: String,
    pub read_write: bool,
    /// Binds are inherently recursive, so `false` is handled in `sandbox.rs`
    /// by binding top-level files individually. Defaults true to match
    /// configs written before this field existed.
    #[serde(default = "default_recursive")]
    pub recursive: bool,
}

fn default_recursive() -> bool {
    true
}

// Container-level default: a hand-written config with only some keys still
// loads, every missing key taking its default.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub endpoint: String,
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    pub system_prompt: String,
    /// File-operations mode's sampling temperature. Chat mode has its own,
    /// separate `chat_temperature` -- the two used to share this one field,
    /// which meant tuning one for roleplay (usually higher) fought against
    /// tuning the other for reliable command output (usually lower).
    pub temperature: f32,
    /// Chat mode's sampling temperature, independent of file-operations
    /// mode's `temperature` above (see its doc comment for why they split).
    #[serde(default = "default_chat_temperature")]
    pub chat_temperature: f32,
    #[serde(default)]
    pub granted_paths: Vec<GrantedPath>,
    #[serde(default)]
    pub auto_approve: Vec<String>,
    /// Where soft-deleted files actually live on disk (see
    /// `sandbox::resolve_trash_dir`) -- empty means the built-in default
    /// (`<app-config-dir>/trash`), same "empty means unset" convention as
    /// `api_key`. Deliberately outside the sandboxed working folder: a
    /// model kept fetching/listing the old `.temp-trash/` despite an
    /// explicit instruction to ignore it, so it's no longer bound into the
    /// sandbox anywhere a plain `ls` from the working folder could find it.
    #[serde(default)]
    pub sandbox_trash_dir: String,
    /// Propose -> run -> respond cycles chained without the user. 0 = no
    /// limit. Generous by default since the UI collapses intermediate steps.
    #[serde(default = "default_max_auto_steps")]
    pub max_auto_steps: u32,
    /// File-ops tool transport: "auto" (native, falling back to text blocks
    /// for the session when the model rejects tools), "native" or "text".
    #[serde(default = "default_tool_calling")]
    pub tool_calling: String,
    /// Wall-clock limit for one `run_command`, in seconds.
    #[serde(default = "default_command_timeout_secs")]
    pub command_timeout_secs: u32,
    /// Skips the advisory rules files (the tool contract is still sent), so
    /// someone wanting full control isn't competing with baked-in advice.
    #[serde(default)]
    pub disable_builtin_rules: bool,
    /// Budget for one whole turn, system block included (see `context.rs`).
    /// 0 disables trimming. Default leaves headroom because the estimate is
    /// chars/4, not a real tokenizer.
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: u32,
    /// Opt-in last resort before dropping (see `context::fit_to_budget`).
    /// **Off by default**: a small local model writing the permanent record
    /// is risky -- one has already been caught fabricating a completed
    /// reorganization -- and a dropped turn at least leaves an honest gap.
    #[serde(default)]
    pub summarize_before_dropping: bool,
    /// Approvals of one program before this session stops asking about it.
    /// 0 = always ask. Unlike `auto_approve` it expires with the session and
    /// is inferred rather than chosen. Confirmation fatigue is a safety
    /// problem: a dialog always approved stops being read.
    #[serde(default = "default_confirm_fade_after")]
    pub confirm_fade_after: u32,
    /// `--server` mode only (see `server::AuthState`) -- how long a login
    /// session stays valid before it's treated as expired and swept from
    /// the in-memory session set. Previously unbounded: a token lived until
    /// the process restarted, so a long-running server slowly accumulated
    /// one entry per login forever, real but slow growth (most users log in
    /// roughly once per the cookie's own 30-day `Max-Age`, not per visit).
    /// 0 = never expire, matching every other cap in this file. Has no
    /// effect on the GUI, which never goes through `--server`'s login flow.
    #[serde(default = "default_server_session_expiry_days")]
    pub server_session_expiry_days: u32,
    /// The per-session record (`memory.rs`) in the system block. On by
    /// default -- app-written from observed facts, so unlike
    /// `summarize_before_dropping` nothing in it can be wrong. Turn off for
    /// a small context window.
    #[serde(default = "default_memory_enabled")]
    pub memory_enabled: bool,
    /// Load-bearing: the block sits in the system message, which `context.rs`
    /// never trims, so nothing else can cut it down. ~1/10 of
    /// `max_context_tokens` is sane. 0 = no cap.
    #[serde(default = "default_memory_max_tokens")]
    pub memory_max_tokens: u32,
    /// Chat mode's equivalent of `memory_max_tokens` -- same reasoning,
    /// different feature: a session's ` ```state ``` ` snapshot sits in the
    /// system message every turn, which `context.rs` never trims, so this is
    /// the only thing keeping it in check. 0 = no cap.
    #[serde(default = "default_chat_state_max_tokens")]
    pub chat_state_max_tokens: u32,
    /// Some models emit their own reasoning wrapped in `<think>...</think>`
    /// (or `<thinking>`) before the actual answer. Chat mode has no
    /// multi-step chain to show a "Thinking…" disclosure for the way
    /// operation mode does, so this repurposes the same UI pattern for that
    /// instead: shown collapsed while the request is in flight, filled in
    /// and relabeled once the reply arrives -- see `llm::split_thinking`.
    /// On by default; off just means the reasoning (if any) gets stripped
    /// and never shown, not that the model stops producing it.
    #[serde(default = "default_chat_show_thinking")]
    pub chat_show_thinking: bool,
    /// Whether a shown thinking block is written into `history.json` (on the
    /// assistant `ChatMessage`, alongside `content`) rather than only ever
    /// existing for the one live turn. Off by default -- reasoning text can
    /// be long, most of it isn't worth keeping once read, and the default
    /// keeps every existing session file exactly the shape it already is.
    /// Has no effect if `chat_show_thinking` is off; a persisted block is
    /// never re-sent to the model (`llm::to_wire` only reads `content`/
    /// `images`), it's display-only, same as showing it live already was.
    #[serde(default = "default_chat_persist_thinking")]
    pub chat_persist_thinking: bool,
    /// Client-side display only -- the model is still always told to write
    /// `*action*` text (`chat_prompts::CHAT_NARRATION_PROMPT` has no
    /// off-switch of its own), this just controls whether the UI's
    /// `splitChatBlocks` shows those blocks or drops them (thoughts stay) from the rendered
    /// bubble entirely. Off by default (narration shown, styled and
    /// separated from dialogue); on hides it completely, for someone who
    /// only wants the spoken lines.
    #[serde(default = "default_chat_hide_narration")]
    pub chat_hide_narration: bool,
    /// Client-side only, same as `chat_hide_narration` -- the backend call
    /// is always the streaming one (`send_chat_message_streaming`)
    /// either way, so a live reasoning
    /// delta can still fill in `chat_show_thinking`'s placeholder as it
    /// arrives regardless of this setting. This just controls whether the
    /// reply's own text is also shown growing live in its own bubble (on
    /// by default) or kept hidden behind the placeholder until the whole
    /// reply is ready, the pre-streaming behavior, for anyone who finds the
    /// live growing text distracting rather than reassuring.
    #[serde(default = "default_chat_stream_replies")]
    pub chat_stream_replies: bool,
    /// Whether the character may send a photo on her own when the chat
    /// director judges it fits; off means only `/pic` and the camera button.
    #[serde(default = "default_chat_auto_images")]
    pub chat_auto_images: bool,
    /// Retrieves old messages that no longer fit the prompt and adds them to
    /// the turn (see `recall.rs`). The model decides nothing.
    #[serde(default = "default_chat_recall")]
    pub chat_recall: bool,
    /// "session" or "persona" (every chat with the same persona card).
    #[serde(default = "default_chat_recall_scope")]
    pub chat_recall_scope: String,
    /// Empty = keyword search only.
    #[serde(default)]
    pub chat_recall_embedding_model: String,
    #[serde(default = "default_chat_recall_max")]
    pub chat_recall_max: u32,
    /// Cosine floor for an embedding hit.
    #[serde(default = "default_chat_recall_min_similarity")]
    pub chat_recall_min_similarity: f32,
}

fn default_confirm_fade_after() -> u32 {
    3
}

fn default_server_session_expiry_days() -> u32 {
    7
}

fn default_memory_enabled() -> bool {
    true
}

fn default_memory_max_tokens() -> u32 {
    800
}

/// 500 (2000 chars) turned out too small for a real richly-tracked
/// character state -- once a state block exceeded it, `chat_prompts::build_chat_system_content`
/// silently cut off the tail before it ever reached the model, so the next
/// reply could only "restate everything" from what it could still see,
/// permanently losing whatever got cut. 2000 (8000 chars) gives generous
/// headroom for a state that's meant to hold a lot -- a full character
/// sheet's worth of tracked fields -- while still being a fraction of the
/// default `max_context_tokens` budget.
fn default_chat_state_max_tokens() -> u32 {
    2000
}

fn default_chat_show_thinking() -> bool {
    true
}

fn default_chat_persist_thinking() -> bool {
    false
}

fn default_chat_hide_narration() -> bool {
    false
}

fn default_chat_stream_replies() -> bool {
    true
}

fn default_chat_auto_images() -> bool {
    true
}

fn default_chat_recall() -> bool {
    true
}

fn default_chat_recall_scope() -> String {
    "session".into()
}

fn default_chat_recall_max() -> u32 {
    4
}

// Calibrated on nomic-embed-text: related pairs 0.56-0.76, unrelated
// mostly below 0.53 (NOTES 2026-10-18).
fn default_chat_recall_min_similarity() -> f32 {
    0.58
}

/// 8000 turned out tight once accounted for: `CHAT_NARRATION_PROMPT` alone is
/// already ~1000 tokens, `chat_state_max_tokens` allows up to 2000 more, and
/// persona content on top of that -- all fixed, system-level, never trimmed
/// by `context.rs` -- easily leaves under half the old budget for actual
/// conversation history before dropping/condensing kicks in. The image
/// reaction and search answer read that history too, so a tight budget
/// pinches more than just turn 1. 16000 is still a context size most
/// modern small local models handle natively, and this is a plain default
/// -- freely raised or lowered per model in Settings, never forced.
fn default_max_context_tokens() -> u32 {
    16000
}

fn default_tool_calling() -> String {
    "auto".into()
}

fn default_command_timeout_secs() -> u32 {
    120
}

fn default_max_auto_steps() -> u32 {
    12
}

fn default_chat_temperature() -> f32 {
    0.2
}

/// Short on purpose: the user-customizable part. Mechanical rules live in
/// `rules.rs`/`rules.md` so editing this can't break them.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a local file assistant. The user has opened a \
single folder for you to work in; you cannot see or change anything outside it unless they've \
explicitly granted you another path. Follow the working rules provided separately from this prompt.";

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:11434/v1/chat/completions".into(),
            model: "llama3.1".into(),
            api_key: String::new(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            temperature: 0.2,
            chat_temperature: default_chat_temperature(),
            granted_paths: vec![],
            auto_approve: vec![],
            sandbox_trash_dir: String::new(),
            max_auto_steps: default_max_auto_steps(),
            tool_calling: default_tool_calling(),
            command_timeout_secs: default_command_timeout_secs(),
            disable_builtin_rules: false,
            max_context_tokens: default_max_context_tokens(),
            summarize_before_dropping: false,
            confirm_fade_after: default_confirm_fade_after(),
            server_session_expiry_days: default_server_session_expiry_days(),
            memory_enabled: default_memory_enabled(),
            memory_max_tokens: default_memory_max_tokens(),
            chat_state_max_tokens: default_chat_state_max_tokens(),
            chat_show_thinking: default_chat_show_thinking(),
            chat_persist_thinking: default_chat_persist_thinking(),
            chat_hide_narration: default_chat_hide_narration(),
            chat_stream_replies: default_chat_stream_replies(),
            chat_auto_images: default_chat_auto_images(),
            chat_recall: default_chat_recall(),
            chat_recall_scope: default_chat_recall_scope(),
            chat_recall_embedding_model: String::new(),
            chat_recall_max: default_chat_recall_max(),
            chat_recall_min_similarity: default_chat_recall_min_similarity(),
        }
    }
}

fn config_path() -> PathBuf {
    app_config_dir().join("config.toml")
}

/// Global, not per-folder: it must exist before any folder is picked.
/// Defaults are written only when the file is absent; an unreadable or
/// malformed file is an error, never silently replaced.
pub fn load_or_init() -> anyhow::Result<AppConfig> {
    let path = config_path();
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cfg = AppConfig::default();
            save(&cfg)?;
            Ok(cfg)
        }
        Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
    }
}

/// 0600: `api_key` lives in here.
pub fn save(cfg: &AppConfig) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    crate::paths::write_atomic(&path, toml::to_string_pretty(cfg)?.as_bytes(), Some(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn partial_config_fills_missing_keys_with_defaults() {
        let cfg: super::AppConfig =
            toml::from_str("endpoint = \"http://x/v1/chat/completions\"\nmodel = \"m\"\n").unwrap();
        assert_eq!(cfg.model, "m");
        assert_eq!(cfg.system_prompt, super::AppConfig::default().system_prompt);
        assert!(cfg.chat_auto_images);
        assert!(cfg.chat_recall && cfg.chat_recall_embedding_model.is_empty());
        assert_eq!(
            (cfg.chat_recall_scope.as_str(), cfg.chat_recall_max),
            ("session", 4)
        );
    }
}
