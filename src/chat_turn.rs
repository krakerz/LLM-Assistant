//! Chat mode's turns, shared by the Tauri commands, the HTTP server and the
//! CLI. The backend owns the history; callers only send the new user message.
//!
//! 1. `send_user_message`: the streamed, user-facing reply and nothing else.
//! 2. `run_followup`: state update, then the director decides whether a photo
//!    and/or a web search follow (`chat_director`), and an image prompt is
//!    built when one does.
//! 3. `generate_image` / `web_search`: the slow follow-up steps, each ending
//!    in an in-character message appended to the history.
//!
//! Splitting these keeps every model call single-purpose: a small model asked
//! to stay in character, track state and emit a protocol format at once does
//! none of them reliably. History writes are short and locked per session
//! (`chat_session::lock`); LLM calls never run under that lock.

use crate::chat_director as director;
use crate::chat_prompts as prompts;
use crate::comfyui::{self, ImagePromptFields};
use crate::config::AppConfig;
use crate::llm::{self, ChatMessage};
use crate::{chat_session, context, gpu, recall, ruleset, searxng};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Mutex, OnceLock};

/// Fixed, low temperature for the mechanical calls (state, summary).
const MECHANICAL_TEMPERATURE: f32 = 0.1;
const PIC_PLACEHOLDER: &str = "(asks for a photo)";
const AUTO_TITLE_MAX_CHARS: usize = 40;
const STATE_PREVIOUS_MAX_CHARS: usize = 8000;
const STATE_USER_TEXT_MAX_CHARS: usize = 4000;
/// How much of her last message joins the user's text as the recall query.
const RECALL_PREVIOUS_MAX_CHARS: usize = 300;

fn lock_map<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// --- Turn 1 ---

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendInput {
    pub session_id: String,
    pub text: String,
    /// Data URLs, sent to the model as vision input.
    #[serde(default)]
    pub images: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnOneResult {
    pub user_message_id: String,
    /// Empty when nothing was saved (stopped before any reply text).
    pub assistant_message_id: String,
    pub reply: String,
    pub thinking: Option<String>,
    pub photo_forced: bool,
    pub photo_hint: Option<String>,
    pub dropped: usize,
    pub summarized: bool,
    pub summary: Option<String>,
    /// The new auto-title, when this turn set one.
    pub title: Option<String>,
    pub stopped: bool,
}

/// `/pic [hint]` (case-insensitive, then whitespace or end): the hint, trimmed.
pub fn parse_pic(text: &str) -> Option<&str> {
    let text = text.trim_start();
    let head = text.get(..4)?;
    if !head.eq_ignore_ascii_case("/pic") {
        return None;
    }
    let rest = &text[4..];
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

fn auto_title_from(message: &str) -> String {
    // Roleplay markup reads as noise in a sidebar title.
    let unmarked = message.replace(['*', '|'], "").replace("//", "");
    let flat: String = unmarked.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > AUTO_TITLE_MAX_CHARS {
        format!(
            "{}…",
            flat.chars().take(AUTO_TITLE_MAX_CHARS).collect::<String>()
        )
    } else {
        flat
    }
}

/// Attachments arrive inlined as `[attached: name]` + body; the body must not
/// become the title, so only that first line is used.
fn title_from_user_text(text: &str) -> String {
    let text = text.trim();
    if text.starts_with("[attached:") {
        auto_title_from(text.lines().next().unwrap_or(""))
    } else {
        auto_title_from(text)
    }
}

fn running() -> &'static Mutex<HashMap<String, tokio::task::AbortHandle>> {
    static RUNNING: OnceLock<Mutex<HashMap<String, tokio::task::AbortHandle>>> = OnceLock::new();
    RUNNING.get_or_init(Default::default)
}

/// Aborts a running turn-1 stream; `false` if none is running. The turn then
/// returns with what streamed so far.
pub fn stop_generation(session_id: &str) -> bool {
    match lock_map(running()).get(session_id) {
        Some(handle) => {
            handle.abort();
            true
        }
        None => false,
    }
}

fn assistant_message(cfg: &AppConfig, text: String, thinking: Option<String>) -> ChatMessage {
    let mut msg = ChatMessage::text("assistant", text).with_id();
    if cfg.chat_persist_thinking {
        msg.thinking = thinking;
    }
    msg
}

/// Streams the reply in a task so `stop_generation` can abort it, forwarding
/// deltas to the caller. Returns the raw reply (reasoning folded in as a
/// `<think>` prefix) and whether it was stopped.
async fn stream_reply(
    cfg: &AppConfig,
    session_id: &str,
    messages: Vec<ChatMessage>,
    on_delta: &mut impl FnMut(llm::ChatDelta<'_>),
) -> anyhow::Result<(String, bool)> {
    enum Piece {
        Content(String),
        Reasoning(String),
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Piece>();
    let (endpoint, model, api_key, temperature) = (
        cfg.endpoint.clone(),
        cfg.model.clone(),
        cfg.api_key.clone(),
        cfg.chat_temperature,
    );
    let task = tokio::spawn(async move {
        llm::send_chat_streaming(
            &endpoint,
            &model,
            &api_key,
            temperature,
            &messages,
            move |delta: llm::ChatDelta<'_>| {
                let piece = match delta {
                    llm::ChatDelta::Content(t) => Piece::Content(t.to_string()),
                    llm::ChatDelta::Reasoning(t) => Piece::Reasoning(t.to_string()),
                };
                let _ = tx.send(piece);
            },
        )
        .await
    });
    lock_map(running()).insert(session_id.to_string(), task.abort_handle());

    let (mut content, mut reasoning) = (String::new(), String::new());
    while let Some(piece) = rx.recv().await {
        match piece {
            Piece::Content(t) => {
                on_delta(llm::ChatDelta::Content(&t));
                content.push_str(&t);
            }
            Piece::Reasoning(t) => {
                on_delta(llm::ChatDelta::Reasoning(&t));
                reasoning.push_str(&t);
            }
        }
    }
    lock_map(running()).remove(session_id);

    match task.await {
        Ok(result) => result.map(|raw| (raw, false)),
        Err(e) if e.is_cancelled() => {
            let raw = if reasoning.is_empty() {
                content
            } else {
                format!("<think>{reasoning}</think>{content}")
            };
            Ok((raw, true))
        }
        Err(e) => Err(anyhow::anyhow!("generation task failed: {e}")),
    }
}

/// The rewritten (summarized) history replaces what the turn was built from;
/// anything appended meanwhile (a reaction, say) is kept after it.
fn merge_rewritten(
    rewritten: Vec<ChatMessage>,
    snapshot: &[ChatMessage],
    current: Vec<ChatMessage>,
) -> Vec<ChatMessage> {
    let known: HashSet<&str> = snapshot.iter().map(|m| m.id.as_str()).collect();
    // The on-disk copy wins for a kept message: another writer may have
    // attached a generated image to it while this turn ran.
    let mut on_disk: HashMap<&str, &ChatMessage> = current
        .iter()
        .filter(|m| !m.id.is_empty())
        .map(|m| (m.id.as_str(), m))
        .collect();
    let mut out: Vec<ChatMessage> = rewritten
        .into_iter()
        .map(|m| match on_disk.remove(m.id.as_str()) {
            Some(disk) => disk.clone(),
            None => m.with_id(),
        })
        .collect();
    out.extend(
        current
            .iter()
            .filter(|m| !known.contains(m.id.as_str()))
            .cloned(),
    );
    out
}

pub async fn send_user_message(
    cfg: &AppConfig,
    input: SendInput,
    mut on_delta: impl FnMut(llm::ChatDelta<'_>),
) -> anyhow::Result<TurnOneResult> {
    let session_id = input.session_id;
    let (photo_forced, photo_hint, stored_text) = match parse_pic(&input.text) {
        Some("") => (true, None, PIC_PLACEHOLDER.to_string()),
        Some(hint) => (true, Some(hint.to_string()), hint.to_string()),
        None => (false, None, input.text.clone()),
    };

    let mut user_message = ChatMessage::text("user", stored_text).with_id();
    user_message.images = input.images;
    let user_message_id = user_message.id.clone();
    let snapshot = {
        let _guard = chat_session::lock(&session_id).await;
        let mut history = chat_session::load_history(&session_id)?;
        history.push(user_message);
        chat_session::save_history(&session_id, &history, None)?;
        history
    };
    // Awaited so recall below sees it with a real timestamp (a backfill would
    // index it with none); the embedding runs in the background.
    index_for_recall(
        cfg,
        &session_id,
        std::slice::from_ref(&snapshot[snapshot.len() - 1]),
    )
    .await;

    let turn = TurnOne {
        session_id: &session_id,
        snapshot,
        user_message_id: user_message_id.clone(),
        photo_forced,
        photo_hint,
    };
    let outcome = run_turn_one(cfg, turn, &mut on_delta).await;
    // A turn with no reply must not leave a dangling user message: the retry
    // would otherwise send two user messages in a row.
    let no_reply = match &outcome {
        Ok(r) => r.assistant_message_id.is_empty(),
        Err(_) => true,
    };
    if no_reply {
        remove_message(&session_id, &user_message_id).await;
        recall::delete_message(&session_id, &user_message_id).await;
    }
    outcome
}

/// Best effort: the turn's own error is the one worth reporting.
async fn remove_message(session_id: &str, message_id: &str) {
    let _guard = chat_session::lock(session_id).await;
    if let Ok(mut history) = chat_session::load_history(session_id) {
        let before = history.len();
        history.retain(|m| m.id != message_id);
        if history.len() != before {
            let _ = chat_session::save_history(session_id, &history, None);
        }
    }
}

struct TurnOne<'a> {
    session_id: &'a str,
    /// History as saved with the new user message last.
    snapshot: Vec<ChatMessage>,
    user_message_id: String,
    photo_forced: bool,
    photo_hint: Option<String>,
}

async fn run_turn_one(
    cfg: &AppConfig,
    turn: TurnOne<'_>,
    on_delta: &mut impl FnMut(llm::ChatDelta<'_>),
) -> anyhow::Result<TurnOneResult> {
    let TurnOne {
        session_id,
        snapshot,
        user_message_id,
        photo_forced,
        photo_hint,
    } = turn;
    let persona = chat_session::persona_content(session_id);
    let state = chat_session::read_state(session_id);
    let system_content = prompts::build_chat_system_content(
        persona.as_deref(),
        &state,
        cfg.chat_state_max_tokens,
        photo_forced,
    );
    let summarizer = cfg.summarize_before_dropping.then(|| context::Summarizer {
        endpoint: &cfg.endpoint,
        model: &cfg.model,
        api_key: &cfg.api_key,
    });
    let fit = context::fit_to_budget(
        context::estimate_tokens(&system_content),
        snapshot.clone(),
        cfg.max_context_tokens as usize,
        summarizer,
        None,
    )
    .await;
    let mut messages = vec![ChatMessage::text("system", system_content)];
    messages.extend(fit.messages);
    add_recall(cfg, session_id, &snapshot, &mut messages).await;

    let (raw, stopped) = stream_reply(cfg, session_id, messages, on_delta).await?;
    let (reply, thinking) = prompts::clean_reply(&raw);

    let mut result = TurnOneResult {
        user_message_id,
        photo_forced,
        photo_hint,
        dropped: fit.dropped,
        summarized: fit.summarized > 0,
        summary: fit.summary,
        stopped,
        ..Default::default()
    };
    if reply.is_empty() && !stopped {
        anyhow::bail!("the model returned an empty reply");
    }

    let assistant =
        (!reply.is_empty()).then(|| assistant_message(cfg, reply.clone(), thinking.clone()));
    {
        let _guard = chat_session::lock(session_id).await;
        let mut history = chat_session::load_history(session_id)?;
        if let Some(rewritten) = fit.rewritten_history {
            history = merge_rewritten(rewritten, &snapshot, history);
        }
        if let Some(assistant) = &assistant {
            history.push(assistant.clone());
        }
        let title_hint = (history.len() == 2 && history[0].role == "user")
            .then(|| title_from_user_text(&history[0].content));
        result.title = chat_session::save_history(session_id, &history, title_hint.as_deref())?;
    }
    if let Some(assistant) = &assistant {
        index_for_recall(cfg, session_id, std::slice::from_ref(assistant)).await;
    }
    result.assistant_message_id = assistant.map(|m| m.id).unwrap_or_default();
    result.reply = reply;
    result.thinking = thinking;
    Ok(result)
}

/// Indexes persisted messages for recall (best effort) and embeds them in the
/// background when an embedding model is configured.
async fn index_for_recall(cfg: &AppConfig, session_id: &str, msgs: &[ChatMessage]) {
    let persona = chat_session::persona_name(session_id);
    let fresh = recall::index_messages(session_id, persona.as_deref(), msgs).await;
    if cfg.chat_recall && !fresh.is_empty() && !cfg.chat_recall_embedding_model.trim().is_empty() {
        let cfg = cfg.clone();
        spawn_background(session_id, async move {
            recall::embed_fresh(&cfg, fresh).await;
        });
    }
}

/// Old messages that fell out of the prompt, appended to the request copy's
/// last user message only: never stored, and never in the system prompt,
/// which stays static so the server's prefix cache survives.
async fn add_recall(
    cfg: &AppConfig,
    session_id: &str,
    snapshot: &[ChatMessage],
    messages: &mut [ChatMessage],
) {
    if !cfg.chat_recall {
        return;
    }
    let Some(last) = messages.last().filter(|m| m.role == "user") else {
        return;
    };
    let in_prompt: HashSet<String> = messages
        .iter()
        .filter(|m| !m.id.is_empty())
        .map(|m| m.id.clone())
        .collect();
    let previous: String = snapshot
        .iter()
        .rev()
        .skip(1)
        .find(|m| m.role == "assistant")
        .map(|m| m.content.chars().take(RECALL_PREVIOUS_MAX_CHARS).collect())
        .unwrap_or_default();
    // The user's own words only: mixing in the previous reply dragged the
    // search back to the last topic ("what's my job?" recalled the jazz bar).
    // A bare follow-up ("and then?") has nothing to search with, so it
    // borrows the previous reply.
    let query = if last.content.split_whitespace().count() >= 3 {
        last.content.clone()
    } else {
        format!("{} {previous}", last.content)
    };
    log::debug!(
        "recall query: {:?}",
        query.chars().take(120).collect::<String>()
    );
    let persona = chat_session::persona_name(session_id);
    let recalled = recall::recall(cfg, session_id, persona.as_deref(), &query, &in_prompt).await;
    if recalled.is_empty() {
        return;
    }
    let block = recall::format_block(&recalled);
    log::debug!("recall block:\n{block}");
    if let Some(last) = messages.last_mut() {
        last.content = format!("{}\n\n{block}", last.content);
    }
}

// --- Follow-up: state, then the director ---

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagePlan {
    pub fields: ImagePromptFields,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowupPlan {
    pub image: Option<ImagePlan>,
    pub search_query: Option<String>,
    pub state_updated: bool,
}

fn searxng_configured() -> bool {
    searxng::load_or_init()
        .map(|c| !c.base_url.trim().is_empty())
        .unwrap_or(false)
}

fn comfyui_configured() -> bool {
    comfyui::load_or_init()
        .map(|c| !c.base_url.trim().is_empty() && !c.workflow_json.trim().is_empty())
        .unwrap_or(false)
}

pub async fn run_followup(
    cfg: &AppConfig,
    session_id: &str,
    assistant_message_id: &str,
    photo_forced: bool,
    photo_hint: Option<String>,
) -> anyhow::Result<FollowupPlan> {
    let (meta, history) = chat_session::load_session(session_id)?;
    let idx = history
        .iter()
        .position(|m| m.id == assistant_message_id && m.role == "assistant")
        .ok_or_else(|| anyhow::anyhow!("that message no longer exists"))?;
    let upto = &history[..=idx];
    let user_text = upto
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let persona = chat_session::persona_content(session_id);

    // Awaited: the director and the prompt builder need this turn's state.
    let written = update_state_json(
        cfg,
        session_id,
        assistant_message_id,
        persona.as_deref(),
        Some(&user_text),
        &upto[idx].content,
    )
    .await;
    let state_updated = written.is_some();
    if let Some(json) = written {
        spawn_state_summary(cfg, session_id, assistant_message_id, json);
    }

    let search_available = searxng_configured();
    let auto_images = cfg.chat_auto_images && comfyui_configured();
    let raw_state = chat_session::read_raw_state(session_id);
    let want_photo_signals = auto_images && !photo_forced;
    let decision = if want_photo_signals || search_available {
        director::decide(
            cfg,
            persona.as_deref(),
            &user_text,
            &upto[idx].content,
            want_photo_signals,
            search_available,
        )
        .await
    } else {
        director::Decision::default()
    };
    let plan = director::resolve(
        &decision,
        photo_forced,
        auto_images,
        search_available,
        director::photo_in_previous_message(upto),
        &user_text,
    );

    let image = if plan.image {
        let style_rules =
            ruleset::load_ruleset(ruleset::IMAGE_GENERATION_RULESET_NAME).unwrap_or_default();
        let fields = director::build_image_prompt(
            cfg,
            &director::BuilderInput {
                persona_name: meta.persona.as_deref(),
                persona: persona.as_deref(),
                raw_state: &raw_state,
                history: upto,
                photo_reason: &plan.photo_reason,
                hint: photo_hint.as_deref(),
                style_rules: &style_rules,
            },
        )
        .await;
        let reason = if plan.photo_reason.is_empty() {
            photo_hint.unwrap_or_default()
        } else {
            plan.photo_reason
        };
        Some(ImagePlan { fields, reason })
    } else {
        None
    };

    Ok(FollowupPlan {
        image,
        search_query: plan.search_query,
        state_updated,
    })
}

// --- State update ---

fn state_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "outfit": {"type": "string"},
            "location": {"type": "string"},
            "activity": {"type": "string"},
            "mood": {"type": "string"},
        },
        "required": ["outfit", "location", "activity", "mood"],
        "additionalProperties": {"type": "string"},
    })
}

/// Rewrites `state.json` from this exchange. Serialized per session so each
/// update starts from the previous one's result; the write is stamped with
/// `source_id` and skipped if a later message already stamped it. Returns the
/// JSON written, `None` if nothing usable came back or the write was stale.
/// Never fails outward.
async fn update_state_json(
    cfg: &AppConfig,
    session_id: &str,
    source_id: &str,
    persona: Option<&str>,
    user_text: Option<&str>,
    reply: &str,
) -> Option<String> {
    let _queue = chat_session::state_queue(session_id).await;
    let previous = chat_session::read_raw_state(session_id);
    let mut system = prompts::CHAT_STATE_UPDATE_PROMPT.to_string();
    if let Some(persona) = persona {
        system.push_str("\n\n");
        system.push_str(persona);
    }
    if !previous.trim().is_empty() {
        system.push_str("\n\n## Your previous state (JSON)\n");
        system.push_str(&context::truncate_with_note(
            previous.trim(),
            STATE_PREVIOUS_MAX_CHARS,
            "previous state",
        ));
    }
    let exchange = match user_text.map(str::trim).filter(|s| !s.is_empty()) {
        Some(user) => format!(
            "The user's message was:\n{}\n\nYour reply was:\n{reply}",
            context::truncate_with_note(user, STATE_USER_TEXT_MAX_CHARS, "message")
        ),
        None => format!("Your reply was:\n{reply}"),
    };
    let messages = vec![
        ChatMessage::text("system", system),
        ChatMessage::text("user", exchange),
    ];
    let value = match llm::send_chat_json(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        &messages,
        MECHANICAL_TEMPERATURE,
        None,
        "state",
        &state_schema(),
    )
    .await
    {
        Ok(v) => v,
        Err(e) => {
            log::warn!("state update request failed: {e}");
            return None;
        }
    };
    let Value::Object(map) = value else {
        return None;
    };
    let fields = prompts::flat_fields(map);
    if fields.is_empty() {
        log::warn!("state update produced no usable fields");
        return None;
    }
    let flat: serde_json::Map<String, Value> = fields
        .into_iter()
        .map(|f| (f.name, Value::String(f.value)))
        .collect();
    let json = serde_json::to_string_pretty(&flat).ok()?;
    match chat_session::write_raw_state_stamped(session_id, source_id, &json).await {
        Ok(true) => Some(json),
        Ok(false) => {
            log::debug!("state update for {source_id} was stale, skipped");
            None
        }
        Err(e) => {
            log::warn!("failed to write state.json: {e}");
            None
        }
    }
}

/// The `state.md` half: precise fields verbatim, narrative fields summarized.
/// Skipped at write time if a newer state has been stamped since.
async fn update_state_summary(cfg: &AppConfig, session_id: &str, source_id: &str, raw_json: &str) {
    let _queue = chat_session::state_queue(session_id).await;
    let (precise, narrative) =
        prompts::partition_state_fields(prompts::parse_state_fields(raw_json));
    let narrative_summary = if narrative.is_empty() {
        String::new()
    } else {
        let narrative_json = serde_json::to_string(
            &narrative
                .iter()
                .map(|f| (f.name.clone(), f.value.clone()))
                .collect::<HashMap<_, _>>(),
        )
        .unwrap_or_default();
        let messages = vec![
            ChatMessage::text("system", prompts::CHAT_STATE_SUMMARY_PROMPT),
            ChatMessage::text("user", narrative_json),
        ];
        let summary = llm::send_chat(
            &cfg.endpoint,
            &cfg.model,
            &cfg.api_key,
            MECHANICAL_TEMPERATURE,
            &messages,
        )
        .await
        .map(|raw| prompts::clean_reply(&raw).0);
        match summary {
            Ok(s) if !s.is_empty() => s,
            other => {
                if let Err(e) = other {
                    log::warn!("state summary call failed, using a plain listing: {e}");
                }
                prompts::plain_field_listing(&narrative)
            }
        }
    };
    let markdown = prompts::build_state_markdown(&precise, &narrative_summary);
    if let Err(e) = chat_session::write_state_md_stamped(session_id, source_id, &markdown).await {
        log::warn!("failed to write state.md: {e}");
    }
}

fn background() -> &'static Mutex<HashMap<String, Vec<tokio::task::JoinHandle<()>>>> {
    static BACKGROUND: OnceLock<Mutex<HashMap<String, Vec<tokio::task::JoinHandle<()>>>>> =
        OnceLock::new();
    BACKGROUND.get_or_init(Default::default)
}

fn spawn_background(session_id: &str, work: impl Future<Output = ()> + Send + 'static) {
    let handle = tokio::spawn(work);
    let mut map = lock_map(background());
    let list = map.entry(session_id.to_string()).or_default();
    list.retain(|h| !h.is_finished());
    list.push(handle);
}

/// Waits for this session's background state work; a short-lived process
/// (the CLI) must await it before exiting or the update is silently lost.
pub async fn flush_background(session_id: &str) {
    loop {
        let handles = lock_map(background())
            .remove(session_id)
            .unwrap_or_default();
        if handles.is_empty() {
            return;
        }
        for handle in handles {
            let _ = handle.await;
        }
    }
}

fn spawn_state_summary(cfg: &AppConfig, session_id: &str, source_id: &str, raw_json: String) {
    let (cfg, sid, src) = (cfg.clone(), session_id.to_string(), source_id.to_string());
    spawn_background(session_id, async move {
        update_state_summary(&cfg, &sid, &src, &raw_json).await;
    });
}

/// State update for a message that isn't a user-driven turn (a reaction or a
/// search answer), entirely in the background.
fn queue_state_update(cfg: &AppConfig, session_id: &str, source_id: &str, reply: String) {
    let (cfg, sid, src) = (cfg.clone(), session_id.to_string(), source_id.to_string());
    spawn_background(session_id, async move {
        let persona = chat_session::persona_content(&sid);
        if let Some(json) =
            update_state_json(&cfg, &sid, &src, persona.as_deref(), None, &reply).await
        {
            update_state_summary(&cfg, &sid, &src, &json).await;
        }
    });
}

// --- In-character follow-up messages (image reaction, search answer) ---

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplyMsg {
    pub id: String,
    pub text: String,
    pub thinking: Option<String>,
}

/// One in-character reply to `trigger` (ephemeral, never stored), with the
/// real, budget-trimmed history before it. `None` for an empty reply.
async fn in_character_reply(
    cfg: &AppConfig,
    session_id: &str,
    policy: Option<&str>,
    trigger: ChatMessage,
) -> anyhow::Result<Option<(String, Option<String>)>> {
    let history = chat_session::load_history(session_id)?;
    let persona = chat_session::persona_content(session_id);
    let state = chat_session::read_state(session_id);
    let mut system_content = prompts::build_chat_system_content(
        persona.as_deref(),
        &state,
        cfg.chat_state_max_tokens,
        false,
    );
    if let Some(policy) = policy.map(str::trim).filter(|p| !p.is_empty()) {
        system_content.push_str("\n\n## Search answer policy\n");
        system_content.push_str(policy);
    }
    let trimmed = context::trim_to_budget(
        context::estimate_tokens(&system_content),
        history,
        cfg.max_context_tokens as usize,
    );
    let mut messages = vec![ChatMessage::text("system", system_content)];
    messages.extend(trimmed.messages);
    messages.push(trigger);

    let raw = llm::send_chat(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        cfg.chat_temperature,
        &messages,
    )
    .await?;
    let (reply, thinking) = prompts::clean_reply(&raw);
    Ok((!reply.is_empty()).then_some((reply, thinking)))
}

/// Appends the message, then queues its state update.
async fn save_reply(
    cfg: &AppConfig,
    session_id: &str,
    text: String,
    thinking: Option<String>,
) -> anyhow::Result<ReplyMsg> {
    let msg = assistant_message(cfg, text.clone(), thinking.clone());
    let id = msg.id.clone();
    chat_session::append_message(session_id, msg.clone()).await?;
    index_for_recall(cfg, session_id, std::slice::from_ref(&msg)).await;
    queue_state_update(cfg, session_id, &id, text.clone());
    Ok(ReplyMsg { id, text, thinking })
}

/// A reply that is just "none" (markers and punctuation ignored).
fn is_none_reply(text: &str) -> bool {
    let letters: String = text.chars().filter(|c| c.is_alphanumeric()).collect();
    letters.eq_ignore_ascii_case("none")
}

const REACTION_ALWAYS: &str = "The picture is now part of the scene, exactly like anything else \
your character just said or did -- don't step outside the moment to review or caption it. \
Continue the conversation forward in character, using what's actually in the picture together \
with how things have actually been going and your current state to decide what your character \
would naturally say or do next. For example, if the user asked to see you relaxing with your cat, \
your next line is something like \"See? Isn't he cute?\" -- a real next beat of dialogue that \
happens to reference the picture, not a description or review of an image file.";

const REACTION_OPTIONAL: &str = "The picture is now part of the scene, exactly like anything else \
your character just said or did. Given it, how the conversation has actually been going, and \
your current state, decide for yourself whether continuing forward right now fits, or whether \
you were already mid-scene and this isn't a natural moment to. If it fits, continue in character \
-- using what's actually in the picture the way you'd naturally reference something you just \
showed someone, not describing or reviewing an image file. If it doesn't fit, reply with \
exactly the single word: none";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratedImageResult {
    pub image_path: String,
    pub image_data_url: String,
    pub reaction: Option<ReplyMsg>,
}

/// Renders the image, attaches it to `message_id`, then lets the character
/// react (skipped for `ReactionMode::Never`). `vision == Some(true)` sends the
/// picture to the model; otherwise the reaction only gets the prompt text. A
/// failed reaction is logged, not an error: the image itself is done.
pub async fn generate_image(
    cfg: &AppConfig,
    session_id: &str,
    message_id: &str,
    fields: &ImagePromptFields,
    vision: Option<bool>,
) -> anyhow::Result<GeneratedImageResult> {
    let comfy_cfg = comfyui::load_or_init()?;
    // The slot (and its gate) is released before the reaction turn below.
    let image = gpu::with_image_slot(cfg, &comfy_cfg, || {
        comfyui::generate_image(&comfy_cfg, fields)
    })
    .await?;
    let path = comfyui::save_generated_image(&comfy_cfg, session_id, &image)?;
    let image_path = path.display().to_string();
    chat_session::attach_generated_image(session_id, message_id, &image_path).await?;
    let image_data_url = comfyui::read_as_data_url(&path)?;

    let optional = match comfy_cfg.reaction_mode {
        comfyui::ReactionMode::Never => {
            return Ok(GeneratedImageResult {
                image_path,
                image_data_url,
                reaction: None,
            })
        }
        comfyui::ReactionMode::Optional => true,
        comfyui::ReactionMode::Always => false,
    };
    let positive = fields.positive.as_deref().unwrap_or("an image");
    let mut trigger = ChatMessage::text(
        "user",
        format!(
            "[Here's the picture you just shared, generated from: {positive}] {}",
            if optional {
                REACTION_OPTIONAL
            } else {
                REACTION_ALWAYS
            }
        ),
    );
    if vision == Some(true) {
        trigger.images = vec![image_data_url.clone()];
    }
    let reaction = match in_character_reply(cfg, session_id, None, trigger).await {
        Ok(Some((text, _))) if optional && is_none_reply(&text) => None,
        Ok(Some((text, thinking))) => save_reply(cfg, session_id, text, thinking)
            .await
            .map_err(|e| log::warn!("failed to save the image reaction: {e}"))
            .ok(),
        Ok(None) => None,
        Err(e) => {
            log::warn!("image reaction failed: {e}");
            None
        }
    };
    Ok(GeneratedImageResult {
        image_path,
        image_data_url,
        reaction,
    })
}

// --- Web search ---

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchResult {
    pub results_summary: String,
    pub answer: Option<ReplyMsg>,
}

fn summarize_results(results: &[searxng::SearchResult], error: Option<&str>) -> String {
    if let Some(error) = error {
        return format!("Search failed: {error}");
    }
    if results.is_empty() {
        return "No results.".to_string();
    }
    results
        .iter()
        .map(|r| format!("- {} ({})", r.title, r.url))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Searches, then lets the character answer from the real results. A failed
/// search still gets an answer turn: an in-character apology, not a raw error.
pub async fn web_search(
    cfg: &AppConfig,
    session_id: &str,
    query: &str,
) -> anyhow::Result<WebSearchResult> {
    let searxng_cfg = searxng::load_or_init()?;
    if searxng_cfg.base_url.trim().is_empty() {
        anyhow::bail!("web search isn't configured (set a SearXNG URL in Settings)");
    }
    let (results, search_error) = match searxng::search(&searxng_cfg, query).await {
        Ok(results) => (results, None),
        Err(e) => {
            log::warn!("web search failed: {e}");
            (Vec::new(), Some(e.to_string()))
        }
    };

    let (situation, instruction) = if let Some(err) = &search_error {
        (
            format!("The search itself failed with a real error, not just an empty result: {err}"),
            "Apologize briefly, in character, and let the user know you couldn't search right now \
             -- don't guess an answer or make anything up.",
        )
    } else if results.is_empty() {
        (
            "No results came back at all.".to_string(),
            "Say so honestly, briefly, in character -- don't guess or make something up.",
        )
    } else {
        let list = results
            .iter()
            .enumerate()
            .map(|(i, r)| format!("{}. {} ({})\n{}", i + 1, r.title, r.url, r.content))
            .collect::<Vec<_>>()
            .join("\n\n");
        (
            format!("Here are the real results:\n\n{list}"),
            "Answer the user's question above using these results, briefly, in character. If \
             the user's message didn't really need a web search, or none of the results help \
             with it, reply with exactly the word none and nothing else.",
        )
    };
    let trigger = ChatMessage::text(
        "user",
        format!("[You just tried searching the web for: {query}] {situation}\n\n{instruction}"),
    );
    let policy = ruleset::load_ruleset(ruleset::WEB_SEARCH_RULESET_NAME).unwrap_or_default();
    let answer = match in_character_reply(cfg, session_id, Some(&policy), trigger).await {
        // The director can mistake small talk for a fact question; "none"
        // lets the answer turn veto an unhelpful search instead of
        // apologising for links nobody asked for.
        Ok(Some((text, _))) if is_none_reply(&text) => None,
        Ok(Some((text, thinking))) => save_reply(cfg, session_id, text, thinking)
            .await
            .map_err(|e| log::warn!("failed to save the search answer: {e}"))
            .ok(),
        Ok(None) => None,
        Err(e) => {
            log::warn!("search answer failed: {e}");
            None
        }
    };
    Ok(WebSearchResult {
        results_summary: summarize_results(&results, search_error.as_deref()),
        answer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex as StdMutex};

    fn m(role: &str, text: &str) -> ChatMessage {
        ChatMessage::text(role, text).with_id()
    }

    #[test]
    fn titles_drop_roleplay_markup() {
        assert_eq!(
            auto_title_from("*walks into the cafe* hey mika!"),
            "walks into the cafe hey mika!"
        );
        assert_eq!(auto_title_from("// waves // || hi ||"), "waves hi");
    }

    #[test]
    fn pic_parsing() {
        assert_eq!(parse_pic("/pic"), Some(""));
        assert_eq!(parse_pic("/PIC   at the beach "), Some("at the beach"));
        assert_eq!(parse_pic("  /pic\nsmiling"), Some("smiling"));
        assert_eq!(parse_pic("/picture of you"), None);
        assert_eq!(parse_pic("/pi"), None);
        assert_eq!(parse_pic("send a /pic"), None);
        assert_eq!(parse_pic("/pic\u{e9}x"), None);
        assert_eq!(parse_pic("é"), None, "non-boundary slice must not panic");
    }

    #[test]
    fn titles_never_include_attachment_bodies() {
        assert_eq!(title_from_user_text("  hello   there "), "hello there");
        assert_eq!(
            title_from_user_text("[attached: notes.txt]\nsecret body\n\nwhat is this"),
            "[attached: notes.txt]"
        );
        assert!(title_from_user_text(&"x".repeat(100)).chars().count() <= 41);
    }

    #[test]
    fn none_replies_ignore_markers_and_punctuation() {
        for s in ["none", "None.", "|| none ||", "// None! //", " NONE "] {
            assert!(is_none_reply(s), "{s:?}");
        }
        for s in ["none of that", "|| Not now. ||", ""] {
            assert!(!is_none_reply(s), "{s:?}");
        }
    }

    #[test]
    fn rewritten_history_keeps_messages_appended_meanwhile() {
        let (a, b, c) = (m("user", "a"), m("assistant", "b"), m("user", "c"));
        let extra = m("assistant", "reaction");
        let summary = ChatMessage::text("user", "summary of a and b");
        let merged = merge_rewritten(
            vec![summary, c.clone()],
            &[a.clone(), b.clone(), c.clone()],
            vec![a, b, c, extra.clone()],
        );
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].content, "summary of a and b");
        assert!(!merged[0].id.is_empty(), "rewritten messages get ids");
        assert_eq!(merged[2].id, extra.id);
    }

    #[test]
    fn rewritten_history_keeps_images_attached_meanwhile() {
        let (a, mut b, c) = (m("user", "a"), m("assistant", "b"), m("user", "c"));
        let snapshot = [a.clone(), b.clone(), c.clone()];
        let rewritten = vec![a.clone(), b.clone(), c.clone()];
        b.generated_images = vec!["/out/pic.png".into()];
        let merged = merge_rewritten(rewritten, &snapshot, vec![a, b, c]);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[1].generated_images, ["/out/pic.png"]);
    }

    #[test]
    fn results_summary_covers_error_empty_and_hits() {
        assert_eq!(summarize_results(&[], Some("down")), "Search failed: down");
        assert_eq!(summarize_results(&[], None), "No results.");
        let hit = searxng::SearchResult {
            title: "T".into(),
            url: "https://x".into(),
            content: "c".into(),
        };
        assert_eq!(summarize_results(&[hit], None), "- T (https://x)");
    }

    #[tokio::test]
    async fn background_work_is_awaited_by_flush() {
        let done = Arc::new(StdMutex::new(false));
        let flag = done.clone();
        spawn_background("session-test-flush", async move {
            tokio::task::yield_now().await;
            *flag.lock().unwrap() = true;
        });
        flush_background("session-test-flush").await;
        assert!(*done.lock().unwrap());
        flush_background("session-test-flush").await;
    }

    #[test]
    fn stopping_with_nothing_running_is_false() {
        assert!(!stop_generation("session-20260101-000000"));
    }

    // --- send_user_message against a local fake OpenAI-compatible server ---

    /// One-shot SSE server: records the request body, streams `chunks`, then
    /// either ends the stream or (`hang`) holds the connection until the
    /// client closes it.
    fn fake_llm(chunks: Vec<&'static str>, hang: bool) -> (String, Arc<StdMutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let seen = Arc::new(StdMutex::new(String::new()));
        let captured = seen.clone();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let body_start = loop {
                let n = stream.read(&mut tmp).unwrap();
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break p + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            while buf.len() < body_start + len {
                let n = stream.read(&mut tmp).unwrap();
                buf.extend_from_slice(&tmp[..n]);
            }
            *captured.lock().unwrap() = String::from_utf8_lossy(&buf[body_start..]).to_string();
            let mut out = String::from(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            );
            for c in chunks {
                out.push_str(&format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
                    serde_json::to_string(c).unwrap()
                ));
            }
            if !hang {
                out.push_str("data: [DONE]\n\n");
            }
            stream.write_all(out.as_bytes()).unwrap();
            stream.flush().unwrap();
            if hang {
                while stream.read(&mut tmp).map(|n| n > 0).unwrap_or(false) {}
            }
        });
        (url, seen)
    }

    fn test_root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-assistant-chat-turn-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        chat_session::use_test_root(dir.join("chat/sessions"));
        dir
    }

    fn test_cfg(url: String) -> AppConfig {
        AppConfig {
            endpoint: url,
            model: "m".into(),
            ..AppConfig::default()
        }
    }

    #[tokio::test]
    async fn turn_one_persists_both_messages_titles_and_streams() {
        let dir = test_root("turn-one");
        let session = chat_session::create_session(None).unwrap();
        let (url, seen) = fake_llm(vec!["|| Hi", " there ||"], false);
        let cfg = test_cfg(url);

        let mut streamed = String::new();
        let result = send_user_message(
            &cfg,
            SendInput {
                session_id: session.id.clone(),
                text: "hello world".into(),
                images: vec![],
            },
            |d| {
                if let llm::ChatDelta::Content(t) = d {
                    streamed.push_str(t);
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(streamed, "|| Hi there ||");
        assert_eq!(result.reply, "|| Hi there ||");
        assert!(!result.stopped && !result.photo_forced);
        assert_eq!(result.title.as_deref(), Some("hello world"));
        let history = chat_session::load_history(&session.id).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id, result.user_message_id);
        assert_eq!(history[1].id, result.assistant_message_id);
        assert_eq!(history[1].content, "|| Hi there ||");
        assert!(!seen.lock().unwrap().contains("The user asked for a photo"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pic_stores_the_hint_and_tells_the_character() {
        let dir = test_root("pic");
        let session = chat_session::create_session(None).unwrap();
        let (url, seen) = fake_llm(vec!["|| here you go ||"], false);
        let cfg = test_cfg(url);

        let result = send_user_message(
            &cfg,
            SendInput {
                session_id: session.id.clone(),
                text: "/pic at the beach".into(),
                images: vec![],
            },
            |_| {},
        )
        .await
        .unwrap();

        assert!(result.photo_forced);
        assert_eq!(result.photo_hint.as_deref(), Some("at the beach"));
        let history = chat_session::load_history(&session.id).unwrap();
        assert_eq!(history[0].content, "at the beach");
        let body = seen.lock().unwrap().clone();
        assert!(body.contains("The user asked for a photo"), "{body}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_empty_pic_is_stored_as_a_placeholder() {
        let dir = test_root("pic-empty");
        let session = chat_session::create_session(None).unwrap();
        let (url, _) = fake_llm(vec!["ok"], false);
        let result = send_user_message(
            &test_cfg(url),
            SendInput {
                session_id: session.id.clone(),
                text: "/pic".into(),
                images: vec![],
            },
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(result.photo_hint, None);
        let history = chat_session::load_history(&session.id).unwrap();
        assert_eq!(history[0].content, PIC_PLACEHOLDER);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stopping_saves_the_partial_reply() {
        let dir = test_root("stop");
        let session = chat_session::create_session(None).unwrap();
        let (url, _) = fake_llm(vec!["|| Hel"], true);
        let cfg = test_cfg(url);
        let sid = session.id.clone();

        let result = send_user_message(
            &cfg,
            SendInput {
                session_id: session.id.clone(),
                text: "hi".into(),
                images: vec![],
            },
            |_| {
                assert!(stop_generation(&sid));
            },
        )
        .await
        .unwrap();

        assert!(result.stopped);
        assert_eq!(result.reply, "|| Hel");
        assert!(!result.assistant_message_id.is_empty());
        let history = chat_session::load_history(&session.id).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].content, "|| Hel");
        assert!(!stop_generation(&session.id), "nothing running afterwards");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn turn_one_recalls_dropped_messages_into_the_request_only() {
        let dir = test_root("recall");
        let session = chat_session::create_session(None).unwrap();
        let history = vec![
            m("user", "good morning"),
            m("assistant", "|| morning! ||"),
            m("user", "My cat is named Biscuit and she loves tuna"),
            m("assistant", "|| Biscuit is a lovely name. ||"),
            m("user", "anyway, the weather is nice"),
            m("assistant", "|| It really is. ||"),
        ];
        chat_session::save_history(&session.id, &history, None).unwrap();
        let (url, seen) = fake_llm(vec!["|| Biscuit! ||"], false);
        // Tiny budget: everything but the first and last message is dropped.
        let cfg = AppConfig {
            max_context_tokens: 1,
            ..test_cfg(url)
        };

        send_user_message(
            &cfg,
            SendInput {
                session_id: session.id.clone(),
                text: "what was my cat called?".into(),
                images: vec![],
            },
            |_| {},
        )
        .await
        .unwrap();

        let body: Value = serde_json::from_str(&seen.lock().unwrap()).unwrap();
        let sent = body["messages"].as_array().unwrap();
        let text_of = |v: &Value| match &v["content"] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let last = sent.last().unwrap();
        assert_eq!(last["role"], "user");
        let last = text_of(last);
        assert!(last.starts_with("what was my cat called?"), "{last}");
        assert!(last.contains("[From earlier in this chat."), "{last}");
        assert!(
            last.contains("the user said: My cat is named Biscuit"),
            "{last}"
        );
        assert!(
            !text_of(&sent[0]).contains("Biscuit"),
            "never in the system prompt"
        );
        assert_eq!(
            sent.iter()
                .filter(|m| text_of(m).contains("Biscuit"))
                .count(),
            1,
            "only the last user message carries it"
        );

        let stored = chat_session::load_history(&session.id).unwrap();
        assert_eq!(stored.len(), 8);
        assert!(stored.iter().all(|m| !m.content.contains("From earlier")));
        assert_eq!(stored[6].content, "what was my cat called?");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn turn_one_without_dropped_messages_adds_no_recall_block() {
        let dir = test_root("recall-none");
        let session = chat_session::create_session(None).unwrap();
        let (url, seen) = fake_llm(vec!["ok"], false);
        send_user_message(
            &test_cfg(url),
            SendInput {
                session_id: session.id.clone(),
                text: "my cat Biscuit".into(),
                images: vec![],
            },
            |_| {},
        )
        .await
        .unwrap();
        assert!(!seen.lock().unwrap().contains("From earlier"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_turn_with_no_reply_leaves_no_dangling_user_message() {
        let dir = test_root("no-reply");
        let session = chat_session::create_session(None).unwrap();
        let (url, _) = fake_llm(vec![], false);
        let err = send_user_message(
            &test_cfg(url),
            SendInput {
                session_id: session.id.clone(),
                text: "hello".into(),
                images: vec![],
            },
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("empty reply"), "{err}");
        assert!(chat_session::load_history(&session.id).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
