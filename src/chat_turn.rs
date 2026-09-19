//! Chat mode's turn, split into single-purpose LLM calls:
//!
//! 1. `run_chat_turn` -- the fast, user-facing reply, and *only* that. Used
//!    by both the GUI (`main.rs`'s `send_chat_message` Tauri command) and
//!    the CLI (`chat_cli.rs`) so the two can never drift the way operation
//!    mode's GUI/headless split once did before `rules.rs` centralized its
//!    shared prompts. Returns as soon as the reply is persisted -- nothing
//!    else in this list runs inside this call or blocks its return.
//! 2. `run_turn_followup` -- called separately, after the frontend has
//!    already shown turn 1's reply (`main.rs`'s own `run_turn_followup`
//!    Tauri command). Awaits the state-update turn's raw-JSON half
//!    (`run_state_json_turn`) *before* dispatch (`run_dispatch_turn`),
//!    deliberately sequential rather than concurrent: dispatch's own
//!    completion is what writes the `` ```image-prompt``` `` fence when
//!    that ruleset is already loaded, and it needs *this* turn's fresh
//!    state to describe the character accurately, not last turn's --
//!    confirmed as a real gap in an earlier, fully-concurrent version of
//!    this call, and worth the added wait now that it's deliberately masked
//!    (the GUI shows a "thinking" placeholder for exactly this step, purely
//!    cosmetic -- there's no real model reasoning behind it, just something
//!    to tell the user a process is running instead of showing nothing).
//!    Once the raw JSON is written, the slower narrative-summarize half
//!    (`finish_state_update`) is spawned detached -- nothing downstream
//!    needs *that* to be fresh, only the raw JSON dispatch is about to read,
//!    so there's nothing to gain from waiting on it too. Only dispatch's
//!    result is actually returned; state-update's success or failure is
//!    logged, not surfaced.
//! 3. `run_image_reaction_turn` -- triggered later, once ComfyUI actually
//!    returns a generated image (`main.rs`'s `generate_comfyui_image`,
//!    which can be seconds to minutes after 1/2 already returned) -- not
//!    part of `run_chat_turn` at all. The GUI calls it as its own separate
//!    `run_image_reaction` command, after `generate_comfyui_image` has
//!    already returned the image, so the image shows up right away instead
//!    of waiting on this turn too; `chat_cli.rs` still runs both back to
//!    back in one `run_full_image_generation` call, since a terminal has no
//!    separate "thinking" indicator to show between the two. Also spawns
//!    its own follow-up state-update, same as turn 1.
//! 4. `run_search_answer_turn` -- the same shape as 3, but for a real
//!    `searxng` web search instead of an image: fired once real results
//!    come back, feeding them to the model for an answer grounded in what
//!    was actually found rather than a guess. Split into its own
//!    `run_search_answer` GUI command the same way, behind `run_web_search`;
//!    also spawns its own follow-up state-update.
//!
//! This split is a deliberate departure from "one LLM request per call, no
//! auto-continue chain": a real session showed one completion asked to
//! simultaneously stay in character, update `state.md`, and remember to
//! emit a protocol fence reliably did none of the mechanical parts --
//! small models are especially bad at juggling several competing
//! responsibilities in one completion. Splitting "have the conversation"
//! from "decide whether a tool applies" from "update the character sheet"
//! into separate, single-purpose completions is far more reliable, at the
//! cost of real extra requests every turn (confirmed acceptable by the
//! user -- reliability over saving round-trips). Turn 1 was originally also
//! blocked on dispatch finishing before returning anything to the frontend
//! -- confirmed, while designing this state-update split, to be an
//! unintentional latency cost with no real benefit, so dispatch (and now
//! state-update alongside it) moved out to their own follow-up call the
//! frontend fires only after the reply is already on screen.

use crate::config::AppConfig;
use crate::llm::ChatMessage;
use crate::{chat_session, comfyui, context, persona, rules, ruleset, searxng};

pub struct ChatTurnOutcome {
    pub reply: String,
    /// The model's own reasoning, if it wrapped any in a `<think>` (or
    /// `<thinking>`) tag -- never stored in history, only surfaced for
    /// display.
    pub thinking: Option<String>,
    pub dropped: usize,
    pub condensed: usize,
    pub summarized: usize,
    pub summary: Option<String>,
    pub rewritten_history: Option<Vec<ChatMessage>>,
}

/// Turn 2's result -- see the module doc comment for why this is no longer
/// part of `ChatTurnOutcome`. State-update's own full completion is still
/// not part of this shape: only its raw-JSON half is awaited before this
/// returns (dispatch needs it fresh), and the slower narrative-summarize
/// half stays a detached background task with nothing awaiting it, so
/// there's still no moment at which "has state *fully* finished updating
/// this turn" could be reported without re-introducing the exact latency
/// this whole split exists to avoid for that half specifically.
pub struct TurnFollowupOutcome {
    /// A ruleset the dispatch pass requested this turn
    /// (` ```ruleset <name> ``` `) that existed and is now loaded for the
    /// rest of this session.
    pub ruleset_loaded: Option<String>,
    /// Set instead of `ruleset_loaded` when the requested name doesn't
    /// match any existing ruleset -- surfaced rather than silently dropped,
    /// so a typo'd request doesn't just look like nothing happened.
    pub ruleset_error: Option<String>,
    /// An image the dispatch pass requested this turn
    /// (` ```image-prompt ``` `), if any. Generation itself happens later,
    /// separately -- see the module doc comment.
    pub image_prompt_requested: Option<comfyui::ImagePromptFields>,
    /// A web search the dispatch pass requested this turn
    /// (` ```web-search ``` `), if any -- the query itself. The actual
    /// search and answer happen later, separately, same reasoning as
    /// `image_prompt_requested`.
    pub web_search_requested: Option<String>,
    /// `None` only if the raw-JSON half itself failed or produced nothing
    /// usable (logged, not surfaced) -- otherwise `Some`, covering just the
    /// detached narrative-summarize half (the raw-JSON half already
    /// finished by the time this outcome exists at all, since it's awaited
    /// before dispatch runs). Same "GUI/`--server` drop it, `chat_cli.rs`
    /// drains it" contract as `TurnReply::state_update_handle`.
    pub state_update_handle: Option<tokio::task::JoinHandle<()>>,
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

/// The dispatch pass never writes creatively -- a fixed, low value instead
/// of `cfg.chat_temperature`, since this is a mechanical
/// yes/no-and-format-a-fence task, and low temperature means more reliable
/// fence-syntax adherence.
const DISPATCH_TEMPERATURE: f32 = 0.1;

/// "Load a ruleset, then use it" is the only legitimate reason a dispatch
/// decision needs more than one attempt -- unlike operation mode's
/// user-configurable step chain, there's no reason a real decision would
/// ever need more, so this is a plain constant, not a new config knob. 3,
/// not 2: a real session showed a small model correctly requesting and
/// loading `web-search` on attempt 1, then answering "none" instead of
/// actually using it on attempt 2 -- the newly-loaded protocol was right
/// there in the prompt, it just didn't follow through immediately. One
/// extra attempt gives that a second chance without letting a genuinely
/// indecisive model loop forever (still a hard cap, not unlimited).
const MAX_DISPATCH_ATTEMPTS: u32 = 3;

/// `history` is the caller's live copy, already including the new user
/// message. The updated history -- reply appended, any ` ```state ``` `
/// block always stripped before being stored (`state.md` already keeps the
/// durable copy) and the model's reasoning attached only if
/// `chat_persist_thinking` says to keep it (never re-explained to the model
/// itself next turn either way -- `llm::to_wire` never reads it) -- is
/// persisted before returning. Returns immediately after that -- dispatch
/// and the state-update turn (turn 2, see module doc comment) are a
/// separate follow-up call the caller fires only after showing this reply,
/// not something awaited here.
/// Everything after obtaining the raw reply string -- thinking extraction,
/// defensive fence-stripping, history persistence, building the outcome.
/// Shared by `run_chat_turn` and `run_chat_turn_streaming` so this logic
/// exists exactly once regardless of how the reply text was actually
/// obtained.
fn finish_chat_turn(
    cfg: &AppConfig,
    session_id: &str,
    history: Vec<ChatMessage>,
    trimmed: context::FitOutcome,
    raw_reply: String,
) -> anyhow::Result<ChatTurnOutcome> {
    let thinking = rules::extract_thinking_block(&raw_reply);
    let reply = rules::strip_thinking_blocks(&raw_reply);

    // Defensive only -- turn 1's system prompt no longer mentions state or
    // rulesets at all (see `rules::build_chat_system_content`'s doc
    // comment), so a well-behaved model has no reason to emit any of these
    // fences here. Stripped in case one shows up anyway; never acted on --
    // that's the follow-up call's job.
    let stored_reply = rules::strip_web_search_blocks(&rules::strip_image_prompt_blocks(
        &rules::strip_ruleset_requests(&rules::strip_state_blocks(&reply)),
    ));

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
        dropped: trimmed.dropped,
        condensed: trimmed.condensed,
        summarized: trimmed.summarized,
        summary: trimmed.summary,
        rewritten_history: trimmed.rewritten_history,
    })
}

/// Builds the system prompt and the context-trimmed message list turn 1
/// actually sends -- identical setup for both the non-streaming and
/// streaming paths below, only the LLM call itself differs between them.
async fn prepare_chat_turn(
    cfg: &AppConfig,
    session_id: &str,
    history: &[ChatMessage],
) -> anyhow::Result<(Vec<ChatMessage>, context::FitOutcome)> {
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
        history.to_vec(),
        cfg.max_context_tokens as usize,
        summarizer,
    )
    .await;

    let mut messages = vec![ChatMessage::text("system", system_content)];
    messages.extend(trimmed.messages.clone());
    Ok((messages, trimmed))
}

pub async fn run_chat_turn(
    cfg: &AppConfig,
    session_id: &str,
    history: Vec<ChatMessage>,
) -> anyhow::Result<ChatTurnOutcome> {
    let (messages, trimmed) = prepare_chat_turn(cfg, session_id, &history).await?;

    let raw = crate::llm::send_chat(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        cfg.chat_temperature,
        &messages,
    )
    .await?;

    finish_chat_turn(cfg, session_id, history, trimmed, raw)
}

/// Streaming sibling of `run_chat_turn` -- identical setup
/// (`prepare_chat_turn`), only the LLM call differs: `on_delta` fires once
/// per chunk as the reply is generated, for a caller that wants to forward
/// partial text live (a Tauri `Channel`, an SSE stream). The final,
/// persisted outcome is identical either way -- `finish_chat_turn` runs on
/// the complete accumulated reply exactly as it does for the non-streaming
/// path, so nothing about *what* gets stored or returned changes, only
/// whether the caller also learns about the reply incrementally.
pub async fn run_chat_turn_streaming(
    cfg: &AppConfig,
    session_id: &str,
    history: Vec<ChatMessage>,
    mut on_delta: impl FnMut(crate::llm::ChatDelta<'_>) + Send,
) -> anyhow::Result<ChatTurnOutcome> {
    let (messages, trimmed) = prepare_chat_turn(cfg, session_id, &history).await?;

    let raw = crate::llm::send_chat_streaming(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        cfg.chat_temperature,
        &messages,
        &mut on_delta,
    )
    .await?;

    finish_chat_turn(cfg, session_id, history, trimmed, raw)
}

/// The dedicated state-update turn's own completion, temperature-locked the
/// same as dispatch (a mechanical restate-and-classify task, not a creative
/// one). Given the exchange that just happened and the previous raw JSON,
/// extracts+validates the new JSON via `rules::extract_json_state_block` +
/// `serde_json::from_str`. `Ok(None)` (not an error) if the model produced
/// nothing usable this turn -- treated exactly like "state didn't change",
/// never surfaced to whatever's waiting on the reply this update follows.
///
/// `last_user_message` is `None` for turns 3/4 (image reaction, search
/// answer) -- those are triggered by a generated image or search result
/// landing, not a fresh user message, so there's nothing of that shape to
/// include; the user message that actually led there already went through
/// its own state update back on turn 1/2. `Some` for turn 1/2's own call
/// (`run_turn_followup`) -- previously always omitted here regardless
/// (`CHAT_STATE_UPDATE_PROMPT` says "given the exchange," but the message
/// sent was only ever the reply, never what the user actually said), so
/// anything the user's own message conveyed -- an action or intention
/// wrapped in `//...//`, the same narration convention their own replies
/// use -- was invisible to this turn entirely.
async fn run_state_json_turn(
    cfg: &AppConfig,
    session_id: &str,
    persona_content: Option<&str>,
    last_user_message: Option<&str>,
    last_reply: &str,
) -> anyhow::Result<Option<String>> {
    let previous_raw = chat_session::read_raw_state(session_id);
    let mut system_content = rules::CHAT_STATE_UPDATE_PROMPT.to_string();
    if let Some(persona) = persona_content {
        system_content.push_str("\n\n");
        system_content.push_str(persona);
    }
    if !previous_raw.trim().is_empty() {
        system_content.push_str("\n\n## Your previous state (JSON)\n");
        system_content.push_str(&crate::context::truncate_with_note(
            previous_raw.trim(),
            8000,
            "previous state",
        ));
    }
    let exchange = match last_user_message.map(str::trim).filter(|s| !s.is_empty()) {
        Some(user_message) => {
            format!("The user's message was:\n{user_message}\n\nYour reply was:\n{last_reply}")
        }
        None => format!("Your reply was:\n{last_reply}"),
    };
    let messages = vec![
        ChatMessage::text("system", system_content),
        ChatMessage::text("user", exchange),
    ];
    let reply = crate::llm::send_chat(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        DISPATCH_TEMPERATURE,
        &messages,
    )
    .await?;
    let Some(raw_json) = rules::extract_json_state_block(&reply) else {
        return Ok(None);
    };
    // Validated here, not just handed straight to `update_raw_state` --
    // `state.json` must always be valid JSON for the *next* turn's own
    // read of it to mean anything, unlike `state.md` (display/context only,
    // safe to cap with blind truncation -- see `rules::append_state_block`).
    if serde_json::from_str::<serde_json::Value>(&raw_json).is_err() {
        log::warn!(
            "run_state_json_turn: model's ```state``` block wasn't valid JSON: {raw_json:?}"
        );
        return Ok(None);
    }
    Ok(Some(raw_json))
}

/// The whole dedicated state-update turn: raw JSON first, then the derived
/// `state.md` summary -- see the module doc comment and
/// `rules::is_precise_field`'s doc comment for the fidelity-drift reasoning
/// behind splitting fields this way. Always runs both halves back to back
/// (unlike `run_turn_followup`'s own use of these same two pieces, which
/// awaits only the first) -- turns 3/4 have nothing downstream in the same
/// round that needs the raw JSON fresher than "eventually," so there's no
/// reason to split them here. Never fails outward -- every error is logged
/// and simply means state doesn't change this round, same "not a hard
/// failure of anything the user is waiting on" contract as dispatch.
pub async fn run_state_update_turn(
    cfg: &AppConfig,
    session_id: &str,
    persona_content: Option<&str>,
    last_reply: &str,
) {
    let raw_json =
        match run_state_json_turn(cfg, session_id, persona_content, None, last_reply).await {
            Ok(Some(json)) => json,
            Ok(None) => {
                log::warn!("run_state_update_turn: produced no usable state JSON this turn");
                return;
            }
            Err(e) => {
                log::warn!("run_state_update_turn: state JSON request failed: {e}");
                return;
            }
        };
    if let Err(e) = chat_session::update_raw_state(session_id, &raw_json) {
        log::warn!("run_state_update_turn: failed to write state.json: {e}");
        return;
    }
    finish_state_update(cfg, session_id, &raw_json).await;
}

/// The narrative-summarize half of state-update, split out of
/// `run_state_update_turn` so `run_turn_followup` can await the raw-JSON
/// half alone (dispatch needs it fresh) while spawning just this slower
/// half detached (nothing needs *it* fresh -- see the module doc comment).
/// `raw_json` is the value already just written to `state.json`, not
/// re-read from disk, since the caller already has it on hand either way.
async fn finish_state_update(cfg: &AppConfig, session_id: &str, raw_json: &str) {
    let fields = rules::parse_state_fields(raw_json);
    let (precise, narrative) = rules::partition_state_fields(fields);
    let narrative_summary = if narrative.is_empty() {
        String::new()
    } else {
        let narrative_json = serde_json::to_string(
            &narrative
                .iter()
                .map(|f| (f.name.clone(), f.value.clone()))
                .collect::<std::collections::HashMap<_, _>>(),
        )
        .unwrap_or_default();
        let messages = vec![
            ChatMessage::text("system", rules::CHAT_STATE_SUMMARY_PROMPT),
            ChatMessage::text("user", narrative_json),
        ];
        match crate::llm::send_chat(
            &cfg.endpoint,
            &cfg.model,
            &cfg.api_key,
            DISPATCH_TEMPERATURE,
            &messages,
        )
        .await
        {
            Ok(summary) => summary.trim().to_string(),
            Err(e) => {
                // Never silently drop the narrative fields just because the
                // summarize call itself failed -- fall back to a plain
                // mechanical listing rather than lose that information.
                log::warn!(
                    "finish_state_update: summarize call failed, falling back to a plain listing: {e}"
                );
                rules::plain_field_listing(&narrative)
            }
        }
    };
    let markdown = rules::build_state_markdown(&precise, &narrative_summary);
    if let Err(e) = chat_session::update_state(session_id, &markdown) {
        log::warn!("finish_state_update: failed to write state.md: {e}");
    }
}

/// Thin `tokio::spawn` wrapper around `run_state_update_turn` -- not
/// awaited by the GUI/`--server` (see the module doc comment for why:
/// nothing there should have to wait on this to get its own result back),
/// which is free to drop the returned handle immediately -- dropping a
/// `JoinHandle` does not cancel the task, it just stops listening for its
/// result, so the update keeps running to completion regardless. The
/// handle exists at all for `chat_cli.rs`: a short-lived process can (and,
/// confirmed live, does) exit before a detached task gets a chance to run,
/// silently discarding the update -- `chat_cli.rs` collects these and
/// drains them before the process exits instead of dropping them. Takes
/// owned values rather than borrows since a spawned task must be `'static`.
fn spawn_state_update(
    cfg: AppConfig,
    session_id: String,
    persona_content: Option<String>,
    last_reply: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_state_update_turn(&cfg, &session_id, persona_content.as_deref(), &last_reply).await;
    })
}

/// Same reasoning and same CLI-drain contract as `spawn_state_update`, just
/// for `finish_state_update` alone -- used by `run_turn_followup`, which
/// awaits the raw-JSON half itself and only spawns this slower, narrative
/// half detached.
fn spawn_finish_state_update(
    cfg: AppConfig,
    session_id: String,
    raw_json: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        finish_state_update(&cfg, &session_id, &raw_json).await;
    })
}

struct DispatchOutcome {
    ruleset_loaded: Option<String>,
    ruleset_error: Option<String>,
    image_prompt_requested: Option<comfyui::ImagePromptFields>,
    web_search_requested: Option<String>,
}

impl DispatchOutcome {
    fn none() -> Self {
        Self {
            ruleset_loaded: None,
            ruleset_error: None,
            image_prompt_requested: None,
            web_search_requested: None,
        }
    }
}

/// Turn 2's dispatch half -- see the module doc comment for why this is a
/// separate completion from turn 1, and `run_turn_followup`'s doc comment
/// for why it now reads raw JSON instead of the `state.md` summary. Never
/// fails the overall turn: a network hiccup or an unparseable reply here is
/// logged and simply means nothing happened this turn, exactly as if the
/// model had said "none" on purpose.
async fn run_dispatch_turn(
    cfg: &AppConfig,
    session_id: &str,
    persona_content: Option<&str>,
    last_user_message: &str,
    last_assistant_reply: &str,
) -> DispatchOutcome {
    // At INFO, not DEBUG -- unlike the raw-reply log further down (only
    // useful once something's already gone wrong), whether dispatch ran at
    // all this turn is worth seeing in the plain log, not just when
    // debugging: it's the app's own record of every "did the model decide
    // to use a tool this turn" check, silent turns included.
    log::info!("dispatch turn: evaluating session {session_id}");
    let mut outcome = DispatchOutcome::none();
    let state = chat_session::read_raw_state(session_id);
    // Offering `web-search` when SearXNG has no `base_url` set would just
    // lead the model to request a search that's guaranteed to fail (turn 4
    // still answers in character, apologizing for the error, but that's a
    // wasted round trip for something this cheap to rule out up front) --
    // so it's left off the list entirely rather than relying on the model
    // to somehow infer "available" doesn't really mean available yet.
    let searxng_configured = searxng::load_or_init()
        .map(|cfg| !cfg.base_url.trim().is_empty())
        .unwrap_or(false);

    // Set right after a ruleset is freshly loaded (below), cleared the
    // instant it's actually used for a nudge -- see the "none" handling at
    // the bottom of the loop for why this exists: a real session showed the
    // model correctly requesting and loading `web-search`, then answering
    // "none" on the very next attempt instead of actually using the
    // protocol just injected for it.
    let mut just_loaded: Option<String> = None;

    for _ in 0..MAX_DISPATCH_ATTEMPTS {
        let loaded_names = chat_session::read_loaded_rulesets(session_id);
        let available_rulesets: Vec<ruleset::RulesetSummary> = ruleset::list_rulesets()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| !loaded_names.contains(&r.name))
            .filter(|r| searxng_configured || r.name != ruleset::WEB_SEARCH_RULESET_NAME)
            .collect();
        let loaded_rulesets: Vec<(String, String)> = loaded_names
            .iter()
            .filter_map(|name| ruleset::load_ruleset(name).ok().map(|c| (name.clone(), c)))
            .collect();

        let mut system_content = rules::build_dispatch_system_content(
            persona_content,
            &state,
            cfg.chat_state_max_tokens,
            &available_rulesets,
            &loaded_rulesets,
        );
        // A direct, forceful nudge for the exact failure above -- reworded
        // instead of trusting the general "already loaded" instruction a
        // second time, since that's precisely what didn't work the first
        // time around.
        if let Some(name) = &just_loaded {
            system_content.push_str(&format!(
                "\n\n---\nYou just loaded \"{name}\" because it applies to the exchange below \
                -- if that's still true, use its own fence NOW. Answering \"none\" right after \
                loading a ruleset for this exact exchange means you loaded it for nothing."
            ));
        }
        // Deliberately ONE trailing "user"-role message, not a separate
        // user/assistant pair -- ending the list on an "assistant" message
        // is out-of-distribution for how chat templates expect to elicit a
        // fresh completion (they append the generation prompt right after
        // whatever the last message is, and most instruction tuning never
        // sees "continue after your own prior turn" as a shape), and lined
        // up with the real symptom: dispatch was intermittently returning
        // an empty string, or a raw continuation of the state block, both
        // consistent with the model treating the last "assistant" message
        // as something to extend rather than a decision to make fresh.
        let messages = vec![
            ChatMessage::text("system", system_content),
            ChatMessage::text(
                "user",
                format!(
                    "-- Exchange to evaluate --\nUser: {last_user_message}\n\
                     Assistant: {last_assistant_reply}\n-- end of exchange --"
                ),
            ),
        ];

        let dispatch_reply = match crate::llm::send_chat(
            &cfg.endpoint,
            &cfg.model,
            &cfg.api_key,
            DISPATCH_TEMPERATURE,
            &messages,
        )
        .await
        {
            Ok(reply) => reply,
            Err(e) => {
                log::warn!("dispatch turn failed, treating as no tool needed: {e}");
                return outcome;
            }
        };
        // Kept at debug (not just during development) -- when dispatch
        // doesn't fire, the raw reply is exactly what's needed to tell
        // "the model said something unparseable" apart from "the request
        // failed outright" apart from "it correctly said none".
        log::debug!("dispatch turn raw reply: {dispatch_reply:?}");

        let image_request = rules::extract_image_prompt_request(&dispatch_reply)
            .or_else(|| extract_mistagged_image_prompt(&dispatch_reply, &loaded_rulesets));
        if let Some(fields) = image_request {
            outcome.image_prompt_requested = Some(fields);
            return outcome;
        }
        if let Some(query) = rules::extract_web_search_request(&dispatch_reply) {
            outcome.web_search_requested = Some(query);
            return outcome;
        }
        let ruleset_request = rules::extract_ruleset_request(&dispatch_reply)
            .or_else(|| extract_bare_ruleset_fence(&dispatch_reply, &available_rulesets));
        if let Some(name) = ruleset_request {
            if ruleset::load_ruleset(&name).is_ok() {
                if let Err(e) = chat_session::add_loaded_ruleset(session_id, &name) {
                    log::warn!("dispatch turn: failed to record loaded ruleset {name}: {e}");
                    return outcome;
                }
                log::info!("dispatch turn: loaded ruleset \"{name}\"");
                just_loaded = Some(name.clone());
                outcome.ruleset_loaded = Some(name);
                continue; // retry now that this ruleset's content is available
            } else {
                log::warn!("dispatch turn: requested unknown ruleset \"{name}\"");
                outcome.ruleset_error = Some(format!("requested unknown ruleset \"{name}\""));
                return outcome;
            }
        }
        // "none" or unparseable. If this is right after a fresh load,
        // give it exactly one nudged retry (see `just_loaded` above)
        // instead of accepting "none" immediately -- `.take()` clears it,
        // so a second "none" after the nudge is final either way, and the
        // loop's own attempt cap still bounds this regardless.
        if just_loaded.take().is_some() {
            continue;
        }
        return outcome;
    }
    outcome
}

/// Turn 2 as a whole -- see the module doc comment. Called separately from
/// `run_chat_turn`, only after the caller has already shown turn 1's reply,
/// so neither half of this can add latency to that. Spawns the state-update
/// turn detached (`spawn_state_update`, never awaited) and awaits+returns
/// only the dispatch decision, which is the one half anything downstream
/// (image generation, web search) actually needs a result from.
///
/// Dispatch reads the raw JSON state, not the `state.md` summary turn 1/3/4
/// get: when the image-generation ruleset is already loaded, this same
/// completion is what writes the ` ```image-prompt``` ` fence, and that
/// needs precise visual detail (exact clothing/appearance) a summary would
/// already have compressed away. The state-update turn always reads/writes
/// the raw JSON regardless, since maintaining it *is* that turn's job.
pub async fn run_turn_followup(
    cfg: &AppConfig,
    session_id: &str,
    persona_content: Option<&str>,
    last_user_message: &str,
    last_assistant_reply: &str,
) -> TurnFollowupOutcome {
    // Awaited before dispatch, not spawned alongside it -- see the module
    // doc comment for why dispatch specifically needs this turn's fresh
    // raw JSON rather than last turn's.
    let state_update_handle = match run_state_json_turn(
        cfg,
        session_id,
        persona_content,
        Some(last_user_message),
        last_assistant_reply,
    )
    .await
    {
        Ok(Some(raw_json)) => match chat_session::update_raw_state(session_id, &raw_json) {
            Ok(()) => Some(spawn_finish_state_update(
                cfg.clone(),
                session_id.to_string(),
                raw_json,
            )),
            Err(e) => {
                log::warn!("run_turn_followup: failed to write state.json: {e}");
                None
            }
        },
        Ok(None) => {
            log::warn!("run_turn_followup: produced no usable state JSON this turn");
            None
        }
        Err(e) => {
            log::warn!("run_turn_followup: state JSON request failed: {e}");
            None
        }
    };
    let dispatch = run_dispatch_turn(
        cfg,
        session_id,
        persona_content,
        last_user_message,
        last_assistant_reply,
    )
    .await;
    TurnFollowupOutcome {
        ruleset_loaded: dispatch.ruleset_loaded,
        ruleset_error: dispatch.ruleset_error,
        image_prompt_requested: dispatch.image_prompt_requested,
        web_search_requested: dispatch.web_search_requested,
        state_update_handle,
    }
}

/// Defensive fallback for a near-miss `rules::extract_ruleset_request`
/// won't catch: a model that confuses the ruleset's *name* with a fence's
/// own language tag, writing e.g. ` ```image-generation-prompt``` ` instead
/// of the correct ` ```ruleset image-generation-prompt``` ` -- observed in
/// a real CLI test session despite `CHAT_DISPATCH_PROMPT` spelling out the
/// difference with a worked example. Since the dispatch pass already knows
/// the finite list of names that could legitimately mean anything here,
/// matching against that list directly (rather than trying to generalize
/// the parser) is safe: it can only ever recognize an actual available
/// ruleset's name, never an arbitrary fence tag a model might use for
/// something else entirely (a code sample, for instance).
fn extract_bare_ruleset_fence(
    reply: &str,
    available: &[ruleset::RulesetSummary],
) -> Option<String> {
    let trimmed = reply.trim();
    let bare = trimmed.trim_matches('`').trim();
    available
        .iter()
        .find(|r| bare == r.name || trimmed.contains(&format!("```{}```", r.name)))
        .map(|r| r.name.clone())
}

/// Another near-miss variant `rules::extract_image_prompt_request` won't
/// catch, seen repeatedly in the same real CLI testing:
/// ` ```image-generation-prompt\npositive: ...\n``` ` -- the *correct*
/// `key: value` body, but tagged with the ruleset's own name instead of the
/// actual `image-prompt` fence tag. Unsurprising given how close
/// "image-prompt" and "image-generation-prompt" are as strings; rather than
/// keep chasing every new near-miss with its own one-off patch, this
/// rewrites the fence tag to the correct one for every *currently loaded*
/// ruleset name found in the reply, then re-runs the real extractor on the
/// patched text -- bounded to loaded rulesets specifically, same reasoning
/// as `extract_bare_ruleset_fence`: it can only ever match a name that's
/// genuinely relevant to this conversation, not an arbitrary tag.
fn extract_mistagged_image_prompt(
    reply: &str,
    loaded_rulesets: &[(String, String)],
) -> Option<comfyui::ImagePromptFields> {
    for (name, _) in loaded_rulesets {
        let marker = format!("```{name}\n");
        if reply.contains(&marker) {
            let patched = reply.replacen(&marker, "```image-prompt\n", 1);
            if let Some(fields) = rules::extract_image_prompt_request(&patched) {
                return Some(fields);
            }
        }
    }
    None
}

/// Just the generate-and-save half of "an image was requested" -- no
/// reaction. Split out of what used to be one `run_full_image_generation`
/// call so the GUI can show the finished image the moment it's ready
/// instead of turn 3's own separate LLM call (which can take a real chunk
/// of time on its own) holding up something that's already done. Shared by
/// `main.rs`'s `generate_comfyui_image` Tauri command and
/// `run_full_image_generation` below.
pub struct GeneratedImage {
    pub path: std::path::PathBuf,
    pub data_url: String,
}

pub async fn generate_and_save_image(
    comfy_cfg: &comfyui::ComfyUiConfig,
    session_id: &str,
    fields: &comfyui::ImagePromptFields,
) -> anyhow::Result<GeneratedImage> {
    let image = comfyui::generate_image(comfy_cfg, fields).await?;
    let path = comfyui::save_generated_image(comfy_cfg, session_id, &image)?;
    chat_session::append_generated_image(session_id, &path.display().to_string())?;
    let data_url = comfyui::read_as_data_url(&path)?;
    Ok(GeneratedImage { path, data_url })
}

/// A turn 3/4 reply plus whatever reasoning came with it -- both turns share
/// this shape. `thinking` used to be silently discarded for these two turns
/// (only turn 1 ever surfaced it), even though the GUI's own "thinking"
/// placeholder (`createChatThinkingPlaceholder`/`resolveChatThinking`) is
/// generic enough to show any turn's reasoning, not just turn 1's -- kept
/// here purely for live display, same as turn 1's; never persisted into
/// session history, since only the reply text itself is part of the
/// conversation the model needs to see again next turn.
pub struct TurnReply {
    pub text: Option<String>,
    pub thinking: Option<String>,
    /// `Some` only when this turn actually produced text and so spawned its
    /// own follow-up state-update (see `spawn_state_update`'s doc comment
    /// for why this exists at all -- GUI/`--server` callers drop it
    /// immediately via `TurnReply -> TurnReply*Result/Response`'s `From`
    /// impls, which don't reference this field; `chat_cli.rs`'s
    /// `run_full_image_generation`/`run_full_web_search` wrappers surface
    /// it so the CLI can drain it before the process exits).
    pub state_update_handle: Option<tokio::task::JoinHandle<()>>,
}

/// Runs turn 3 and persists the result if it produced one, folding `Never`
/// mode's "skip the extra request entirely" and an actual request failure
/// into the same "nothing to show" -- neither is worth surfacing as an
/// error to a caller, only a log line. Shared by `run_full_image_generation`
/// (the CLI's one-shot path) and `main.rs`'s standalone `run_image_reaction`
/// command (the GUI's split-out second round-trip -- see that command's
/// doc comment).
pub async fn run_and_persist_image_reaction(
    cfg: &AppConfig,
    session_id: &str,
    positive_prompt: &str,
    image_data_url: &str,
    reaction_mode: comfyui::ReactionMode,
) -> TurnReply {
    if reaction_mode == comfyui::ReactionMode::Never {
        return TurnReply {
            text: None,
            thinking: None,
            state_update_handle: None,
        };
    }
    match run_image_reaction_turn(
        cfg,
        session_id,
        positive_prompt,
        image_data_url,
        reaction_mode,
    )
    .await
    {
        Ok(TurnReply {
            text: Some(text),
            thinking,
            ..
        }) => {
            if let Err(e) = chat_session::append_assistant_message(session_id, &text) {
                log::warn!("run_and_persist_image_reaction: failed to save reaction: {e}");
            }
            // Also gets its own follow-up state-update, same as turn 1 --
            // confirmed explicitly: a character's reaction to a generated
            // image should be able to update state too, not just the
            // original reply that requested it.
            let persona_content = chat_session::load_session(session_id)
                .ok()
                .and_then(|(meta, _)| meta.persona)
                .and_then(|name| persona::load_persona(&name).ok());
            let handle = spawn_state_update(
                cfg.clone(),
                session_id.to_string(),
                persona_content,
                text.clone(),
            );
            TurnReply {
                text: Some(text),
                thinking,
                state_update_handle: Some(handle),
            }
        }
        // `Optional` mode's own considered choice not to comment -- not a
        // failure, nothing to log or persist, but any reasoning behind that
        // choice is still worth showing.
        Ok(reply) => reply,
        Err(e) => {
            log::warn!("run_and_persist_image_reaction: reaction turn failed: {e}");
            TurnReply {
                text: None,
                thinking: None,
                state_update_handle: None,
            }
        }
    }
}

/// The whole "an image was requested" pipeline: generate, save, then let the
/// persona react (turn 3), all in one call -- what the GUI used to do too,
/// before splitting into two round-trips (see `generate_and_save_image` and
/// `run_and_persist_image_reaction`'s doc comments). Still exactly what
/// `chat_cli.rs` wants: a terminal has no separate "thinking" indicator to
/// show between the two, so there's nothing to gain from splitting them
/// there.
pub struct ImageGenerationResult {
    pub path: std::path::PathBuf,
    /// `None` if the reaction call itself failed, or `reaction_mode` was
    /// `Never` -- the image already generated fine either way, and losing
    /// the commentary on it isn't worth treating as an overall failure of
    /// this whole function.
    pub reaction: Option<String>,
    /// See `TurnReply::state_update_handle`'s doc comment -- `chat_cli.rs`
    /// (this function's only caller) drains it before exiting.
    pub state_update_handle: Option<tokio::task::JoinHandle<()>>,
}

pub async fn run_full_image_generation(
    cfg: &AppConfig,
    comfy_cfg: &comfyui::ComfyUiConfig,
    session_id: &str,
    fields: &comfyui::ImagePromptFields,
) -> anyhow::Result<ImageGenerationResult> {
    let image = generate_and_save_image(comfy_cfg, session_id, fields).await?;
    let positive = fields.positive.as_deref().unwrap_or("an image");
    let reaction = run_and_persist_image_reaction(
        cfg,
        session_id,
        positive,
        &image.data_url,
        comfy_cfg.reaction_mode,
    )
    .await;

    Ok(ImageGenerationResult {
        path: image.path,
        reaction: reaction.text,
        state_update_handle: reaction.state_update_handle,
    })
}

/// Turn 3 -- see the module doc comment. Reuses `build_chat_system_content`
/// (the same one turn 1 uses) rather than a bespoke prompt: a reaction
/// **is** a normal in-character reply, it just needs a different trigger
/// message instead of the user's own words. The trigger message is
/// ephemeral (constructed for this call only, never stored in history) with
/// the generated image attached as vision input -- if the configured model
/// can't actually see it, the prompt text alone is still enough to react
/// to, so this degrades gracefully rather than depending on a vision probe.
///
/// Also sends the real, trimmed conversation history alongside the trigger
/// -- a first version sent only the system prompt plus the ephemeral
/// trigger, so all the model actually had to go on was `state.md`'s summary
/// and the raw ComfyUI tag string (`positive_prompt`), never the actual
/// back-and-forth that led to the image. That's exactly why the reaction
/// often read as a generic caption disconnected from the scene rather than
/// a real continuation of it. Trimmed the same way turn 1 respects the
/// context budget (`context::trim_to_budget`), but mechanically only, with
/// no summarizer -- this is a lightweight follow-up, not the place to
/// trigger a fresh summarization pass over the session's history.
///
/// The instruction itself no longer asks for a caption or review of the
/// image either: the picture is treated as part of the scene, the same way
/// anything else the character just did or showed would be, and the model's
/// job is to continue the conversation forward in character from there --
/// grounded in what's actually in the picture, the real history above, and
/// its current state -- rather than stepping outside the moment to comment
/// on an image file. Immersion, which is this turn's whole purpose, over a
/// reflexive "nice picture" remark every time.
///
/// `reaction_mode` decides whether continuing is mandatory
/// (`ReactionMode::Always`, the original behavior) or left to the model.
/// `text: None` is that considered "no comment fits", distinct from an
/// actual request failure (`Err`).
pub async fn run_image_reaction_turn(
    cfg: &AppConfig,
    session_id: &str,
    positive_prompt: &str,
    image_data_url: &str,
    reaction_mode: comfyui::ReactionMode,
) -> anyhow::Result<TurnReply> {
    let (meta, history) = chat_session::load_session(session_id)?;
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

    let instruction = match reaction_mode {
        // `Never` is intercepted by the caller (`run_full_image_generation`)
        // before this function is ever reached -- treated the same as
        // `Always` here only so the match stays exhaustive, not because
        // this arm is expected to run.
        comfyui::ReactionMode::Always | comfyui::ReactionMode::Never => {
            "The picture is now part of the scene, exactly like anything else your character just \
             said or did -- don't step outside the moment to review or caption it. Continue the \
             conversation forward in character, using what's actually in the picture together with \
             how things have actually been going and your current state to decide what your \
             character would naturally say or do next. For example, if the user asked to see you \
             relaxing with your cat, your next line is something like \"See? Isn't he cute?\" -- a \
             real next beat of dialogue that happens to reference the picture, not a description or \
             review of an image file."
                .to_string()
        }
        comfyui::ReactionMode::Optional => {
            "The picture is now part of the scene, exactly like anything else your character just \
             said or did. Given it, how the conversation has actually been going, and your current \
             state, decide for yourself whether continuing forward right now fits, or whether you \
             were already mid-scene and this isn't a natural moment to. If it fits, continue in \
             character -- using what's actually in the picture the way you'd naturally reference \
             something you just showed someone, not describing or reviewing an image file. For \
             example, if the user asked to see you relaxing with your cat, that continuation is \
             something like \"See? Isn't he cute?\", a real next beat of dialogue. If it doesn't \
             fit, reply with exactly the single word: none"
                .to_string()
        }
    };
    let mut trigger = ChatMessage::text(
        "user",
        format!(
            "[Here's the picture you just shared, generated from: {positive_prompt}] {instruction}"
        ),
    );
    trigger.images = vec![image_data_url.to_string()];

    let trimmed = context::trim_to_budget(
        context::estimate_tokens(&system_content),
        history,
        cfg.max_context_tokens as usize,
    );
    let mut messages = vec![ChatMessage::text("system", system_content)];
    messages.extend(trimmed.messages);
    messages.push(trigger);

    let reply = crate::llm::send_chat(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        cfg.chat_temperature,
        &messages,
    )
    .await?;

    let thinking = rules::extract_thinking_block(&reply);
    let reply = rules::strip_thinking_blocks(&reply);
    let stored_reply = rules::strip_web_search_blocks(&rules::strip_image_prompt_blocks(
        &rules::strip_ruleset_requests(&rules::strip_state_blocks(&reply)),
    ));

    if reaction_mode == comfyui::ReactionMode::Optional
        && stored_reply.trim().eq_ignore_ascii_case("none")
    {
        return Ok(TurnReply {
            text: None,
            thinking,
            state_update_handle: None,
        });
    }
    Ok(TurnReply {
        text: Some(stored_reply),
        thinking,
        state_update_handle: None,
    })
}

/// Runs turn 4 and persists the result, folding an actual request failure
/// into `None` -- not worth surfacing as an error to a caller, only a log
/// line (the answer turn has no "skip entirely" mode the way image
/// reaction's `Never` does; it always attempts one, even on a failed
/// search, so the persona can apologize in character -- see
/// `run_search_answer_turn`'s doc comment). Shared by `run_full_web_search`
/// (the CLI's one-shot path) and `main.rs`'s standalone `run_search_answer`
/// command (the GUI's split-out second round-trip).
pub async fn run_and_persist_search_answer(
    cfg: &AppConfig,
    session_id: &str,
    query: &str,
    results: &[searxng::SearchResult],
    search_error: Option<&str>,
) -> TurnReply {
    match run_search_answer_turn(cfg, session_id, query, results, search_error).await {
        Ok(TurnReply {
            text: Some(text),
            thinking,
            ..
        }) => {
            if let Err(e) = chat_session::append_assistant_message(session_id, &text) {
                log::warn!("run_and_persist_search_answer: failed to save answer: {e}");
            }
            // Same reasoning as `run_and_persist_image_reaction`'s own
            // follow-up: the answer is a real in-character continuation too,
            // so it gets a chance to update state, not just turn 1's reply.
            let persona_content = chat_session::load_session(session_id)
                .ok()
                .and_then(|(meta, _)| meta.persona)
                .and_then(|name| persona::load_persona(&name).ok());
            let handle = spawn_state_update(
                cfg.clone(),
                session_id.to_string(),
                persona_content,
                text.clone(),
            );
            TurnReply {
                text: Some(text),
                thinking,
                state_update_handle: Some(handle),
            }
        }
        // The answer turn always attempts a reply (see this function's doc
        // comment), so `text: None` here would mean `run_search_answer_turn`
        // itself changed that contract -- kept for exhaustiveness, not
        // because it's expected to run.
        Ok(reply) => reply,
        Err(e) => {
            log::warn!("run_and_persist_search_answer: answer turn failed: {e}");
            TurnReply {
                text: None,
                thinking: None,
                state_update_handle: None,
            }
        }
    }
}

/// The whole "a web search was requested" pipeline: search, then let the
/// persona answer using the real results (turn 4), all in one call -- what
/// the GUI used to do too, before splitting into two round-trips (see
/// `run_and_persist_search_answer`'s doc comment). Still exactly what
/// `chat_cli.rs` wants, same reasoning as `run_full_image_generation`.
pub struct WebSearchResult {
    pub results: Vec<searxng::SearchResult>,
    /// The search itself failing (network down, rate-limited, misconfigured
    /// URL) is not the same as it succeeding with nothing relevant -- kept
    /// separate so a caller can tell "0 results, nothing found" apart from
    /// "0 results, the request never actually worked".
    pub search_error: Option<String>,
    /// `None` only if the answer turn itself failed (network/LLM error) --
    /// distinct from `search_error`: even a failed *search* still gets an
    /// answer turn, so the persona can apologize in character instead of a
    /// raw technical error reaching the user.
    pub answer: Option<String>,
    /// See `TurnReply::state_update_handle`'s doc comment -- `chat_cli.rs`
    /// (this function's only caller) drains it before exiting.
    pub state_update_handle: Option<tokio::task::JoinHandle<()>>,
}

pub async fn run_full_web_search(
    cfg: &AppConfig,
    searxng_cfg: &searxng::SearxngConfig,
    session_id: &str,
    query: &str,
) -> anyhow::Result<WebSearchResult> {
    let (results, search_error) = match searxng::search(searxng_cfg, query).await {
        Ok(results) => (results, None),
        Err(e) => {
            log::warn!("run_full_web_search: search itself failed: {e}");
            (Vec::new(), Some(e.to_string()))
        }
    };
    let answer =
        run_and_persist_search_answer(cfg, session_id, query, &results, search_error.as_deref())
            .await;
    Ok(WebSearchResult {
        results,
        search_error,
        answer: answer.text,
        state_update_handle: answer.state_update_handle,
    })
}

/// Turn 4 -- see the module doc comment. Same shape as
/// `run_image_reaction_turn`: reuses `build_chat_system_content`, a real
/// answer **is** a normal in-character reply, it just needs the real
/// results (or the fact that the search itself failed) as its trigger
/// instead of the user's own words. No vision input this time -- it's
/// text, not an image.
///
/// `search_error`, when set, means the search never actually ran (not just
/// "found nothing") -- the trigger tells the model plainly so it apologizes
/// for a real problem rather than treating an empty result list as "nothing
/// relevant was found".
pub async fn run_search_answer_turn(
    cfg: &AppConfig,
    session_id: &str,
    query: &str,
    results: &[searxng::SearchResult],
    search_error: Option<&str>,
) -> anyhow::Result<TurnReply> {
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

    let (situation, instruction) = if let Some(err) = search_error {
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
            "Answer the original question using these results, briefly, in character. If none of \
             them actually help, say so honestly rather than guessing.",
        )
    };
    let trigger = ChatMessage::text(
        "user",
        format!("[You just tried searching the web for: {query}] {situation}\n\n{instruction}"),
    );

    let messages = vec![ChatMessage::text("system", system_content), trigger];
    let reply = crate::llm::send_chat(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        cfg.chat_temperature,
        &messages,
    )
    .await?;

    let thinking = rules::extract_thinking_block(&reply);
    let reply = rules::strip_thinking_blocks(&reply);
    let stored_reply = rules::strip_web_search_blocks(&rules::strip_image_prompt_blocks(
        &rules::strip_ruleset_requests(&rules::strip_state_blocks(&reply)),
    ));
    Ok(TurnReply {
        text: Some(stored_reply),
        thinking,
        state_update_handle: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn available() -> Vec<ruleset::RulesetSummary> {
        vec![
            ruleset::RulesetSummary {
                name: "image-generation-prompt".to_string(),
                hint: None,
            },
            ruleset::RulesetSummary {
                name: "web-search".to_string(),
                hint: None,
            },
        ]
    }

    #[test]
    fn extract_bare_ruleset_fence_catches_the_name_used_as_a_fence_tag() {
        // The exact real-world near-miss: a model confusing the ruleset's
        // name with the fence's own language tag.
        assert_eq!(
            extract_bare_ruleset_fence("\n```image-generation-prompt```", &available()),
            Some("image-generation-prompt".to_string())
        );
    }

    #[test]
    fn extract_bare_ruleset_fence_catches_the_bare_name_with_no_fence_at_all() {
        assert_eq!(
            extract_bare_ruleset_fence("image-generation-prompt", &available()),
            Some("image-generation-prompt".to_string())
        );
    }

    #[test]
    fn extract_bare_ruleset_fence_ignores_an_unrelated_fence() {
        assert_eq!(
            extract_bare_ruleset_fence("```python\nprint(1)\n```", &available()),
            None
        );
    }

    #[test]
    fn extract_bare_ruleset_fence_ignores_plain_conversational_text() {
        assert_eq!(
            extract_bare_ruleset_fence("I hope you're having a wonderful day!", &available()),
            None
        );
    }

    #[test]
    fn extract_mistagged_image_prompt_catches_the_ruleset_name_used_as_the_fence_tag() {
        // The exact real-world near-miss: correct body, wrong tag.
        let reply = "\n```image-generation-prompt\npositive: a red circle\nnegative: bad hand\n```";
        let loaded = vec![(
            "image-generation-prompt".to_string(),
            "some ruleset content".to_string(),
        )];
        let fields = extract_mistagged_image_prompt(reply, &loaded).unwrap();
        assert_eq!(fields.positive.as_deref(), Some("a red circle"));
        assert_eq!(fields.negative.as_deref(), Some("bad hand"));
    }

    #[test]
    fn extract_mistagged_image_prompt_ignores_a_ruleset_not_currently_loaded() {
        let reply = "```image-generation-prompt\npositive: a red circle\n```";
        assert!(extract_mistagged_image_prompt(reply, &[]).is_none());
    }

    #[test]
    fn extract_mistagged_image_prompt_ignores_unrelated_text() {
        assert!(extract_mistagged_image_prompt("just a normal reply", &[]).is_none());
    }
}
