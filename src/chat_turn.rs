//! Chat mode's one shared turn: no sandbox, no auto-continue chain, one LLM
//! request per call. Used by both the GUI (`main.rs`'s `send_chat_message`
//! Tauri command) and the CLI (`chat_cli.rs`) so the two can never drift the
//! way operation mode's GUI/headless split once did before `rules.rs`
//! centralized its shared prompts.

use crate::config::AppConfig;
use crate::llm::ChatMessage;
use crate::{chat_session, context, persona, rules};

pub struct ChatTurnOutcome {
    pub reply: String,
    /// The model's own reasoning, if it wrapped any in a `<think>` (or
    /// `<thinking>`) tag -- never stored in history, only surfaced for
    /// display.
    pub thinking: Option<String>,
    /// Whether this turn's reply included a ` ```state ``` ` block that got
    /// saved -- callers show a small indicator rather than the raw block.
    pub state_updated: bool,
    pub dropped: usize,
    pub condensed: usize,
    pub summarized: usize,
    pub summary: Option<String>,
    pub rewritten_history: Option<Vec<ChatMessage>>,
}

const AUTO_TITLE_MAX_CHARS: usize = 40;

fn auto_title_from(message: &str) -> String {
    let flat: String = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > AUTO_TITLE_MAX_CHARS {
        format!(
            "{}…",
            flat.chars().take(AUTO_TITLE_MAX_CHARS).collect::<String>()
        )
    } else {
        flat
    }
}

/// `history` is the caller's live copy, already including the new user
/// message. The updated history -- reply appended, any ` ```state ``` `
/// block always stripped before being stored (`state.md` already keeps the
/// durable copy) and the model's reasoning attached only if
/// `chat_persist_thinking` says to keep it (never re-explained to the model
/// itself next turn either way -- `llm::to_wire` never reads it) -- is
/// persisted before returning, so a session survives a crash up to its last
/// successful reply.
pub async fn run_chat_turn(
    cfg: &AppConfig,
    session_id: &str,
    history: Vec<ChatMessage>,
) -> anyhow::Result<ChatTurnOutcome> {
    let (meta, _) = chat_session::load_session(session_id)?;
    let persona_content = match &meta.persona {
        Some(name) => persona::load_persona(name).ok(),
        None => None,
    };
    let state = chat_session::read_state(session_id);
    let system_content = rules::build_chat_system_content(
        persona_content.as_deref(),
        &state,
        cfg.chat_state_max_tokens,
    );

    let summarizer = cfg.summarize_before_dropping.then(|| context::Summarizer {
        endpoint: &cfg.endpoint,
        model: &cfg.model,
        api_key: &cfg.api_key,
    });
    let trimmed = context::fit_to_budget(
        context::estimate_tokens(&system_content),
        history.clone(),
        cfg.max_context_tokens as usize,
        summarizer,
    )
    .await;

    let mut messages = vec![ChatMessage::text("system", system_content)];
    messages.extend(trimmed.messages);

    let reply = crate::llm::send_chat(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        cfg.temperature,
        &messages,
    )
    .await?;

    let thinking = rules::extract_thinking_block(&reply);
    let reply = rules::strip_thinking_blocks(&reply);

    let state_block = rules::extract_state_block(&reply);
    let state_updated = state_block.is_some();
    if let Some(new_state) = &state_block {
        chat_session::update_state(session_id, new_state)?;
    }
    let stored_reply = rules::strip_state_blocks(&reply);

    let mut full_history = trimmed
        .rewritten_history
        .clone()
        .unwrap_or_else(|| history.clone());
    let mut assistant_message = ChatMessage::text("assistant", stored_reply.clone());
    if cfg.chat_persist_thinking {
        assistant_message.thinking = thinking.clone();
    }
    full_history.push(assistant_message);
    let title_hint = (full_history.len() == 2)
        .then(|| full_history.first())
        .flatten()
        .map(|m| auto_title_from(&m.content));
    chat_session::save_history(session_id, &full_history, title_hint.as_deref())?;

    Ok(ChatTurnOutcome {
        reply: stored_reply,
        thinking,
        state_updated,
        dropped: trimmed.dropped,
        condensed: trimmed.condensed,
        summarized: trimmed.summarized,
        summary: trimmed.summary,
        rewritten_history: trimmed.rewritten_history,
    })
}
