//! Chat mode's turn, split into three single-purpose LLM calls:
//!
//! 1. `run_chat_turn` -- the fast, user-facing reply. Used by both the GUI
//!    (`main.rs`'s `send_chat_message` Tauri command) and the CLI
//!    (`chat_cli.rs`) so the two can never drift the way operation mode's
//!    GUI/headless split once did before `rules.rs` centralized its shared
//!    prompts.
//! 2. `run_dispatch_turn` -- a separate, narrowly-scoped pass (called from
//!    `run_chat_turn`, after the reply above is already persisted) whose
//!    only job is deciding whether the exchange that was just had calls for
//!    a ruleset/tool, and firing it.
//! 3. `run_image_reaction_turn` -- triggered later, once ComfyUI actually
//!    returns a generated image (`main.rs`'s `generate_comfyui_image`,
//!    which can be seconds to minutes after 1/2 already returned) -- not
//!    part of `run_chat_turn` at all.
//!
//! This three-way split is a deliberate departure from "one LLM request per
//! call, no auto-continue chain": a real session showed one completion
//! asked to simultaneously stay in character, update `state.md`, and
//! remember to emit a protocol fence reliably did none of the mechanical
//! parts -- small models are especially bad at juggling several competing
//! responsibilities in one completion. Splitting "have the conversation"
//! from "decide whether a tool applies" into separate, single-purpose
//! completions is far more reliable, at the cost of a real extra request
//! every turn (confirmed acceptable by the user -- reliability over saving
//! one round-trip, and it runs on *every* turn, not gated behind "only if a
//! ruleset exists").

use crate::config::AppConfig;
use crate::llm::ChatMessage;
use crate::{chat_session, comfyui, context, persona, rules, ruleset};

pub struct ChatTurnOutcome {
    pub reply: String,
    /// The model's own reasoning, if it wrapped any in a `<think>` (or
    /// `<thinking>`) tag -- never stored in history, only surfaced for
    /// display.
    pub thinking: Option<String>,
    /// Whether this turn's reply included a ` ```state ``` ` block that got
    /// saved -- callers show a small indicator rather than the raw block.
    pub state_updated: bool,
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

/// The dispatch pass never writes creatively -- a fixed, low value instead
/// of `cfg.chat_temperature`, since this is a mechanical
/// yes/no-and-format-a-fence task, and low temperature means more reliable
/// fence-syntax adherence.
const DISPATCH_TEMPERATURE: f32 = 0.1;

/// "Load a ruleset, then use it" is the only legitimate reason a dispatch
/// decision needs more than one attempt -- unlike operation mode's
/// user-configurable step chain, there's no reason a real decision would
/// ever need more, so this is a plain constant, not a new config knob.
const MAX_DISPATCH_ATTEMPTS: u32 = 2;

/// `history` is the caller's live copy, already including the new user
/// message. The updated history -- reply appended, any ` ```state ``` `
/// block always stripped before being stored (`state.md` already keeps the
/// durable copy) and the model's reasoning attached only if
/// `chat_persist_thinking` says to keep it (never re-explained to the model
/// itself next turn either way -- `llm::to_wire` never reads it) -- is
/// persisted before returning, so a session survives a crash up to its last
/// successful reply. The dispatch pass (turn 2, see module doc comment)
/// runs after that persistence, so its own failure can never lose the
/// user-facing reply.
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
        cfg.chat_temperature,
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

    // Defensive only -- turn 1's system prompt no longer mentions rulesets
    // at all (see `rules::build_chat_system_content`'s doc comment), so a
    // well-behaved model has no reason to emit either fence here. Stripped
    // in case one shows up anyway; never acted on -- that's the dispatch
    // pass's job below.
    let stored_reply = rules::strip_image_prompt_blocks(&rules::strip_ruleset_requests(
        &rules::strip_state_blocks(&reply),
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

    // Turn 2: dispatch. Just the exchange that was just had, not the full
    // history -- `state.md` already carries forward anything durable, so
    // re-sending everything would be wasted tokens on every single turn for
    // what's a classification-shaped task.
    let last_user_message = full_history
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let dispatch = run_dispatch_turn(
        cfg,
        session_id,
        persona_content.as_deref(),
        &last_user_message,
        &stored_reply,
    )
    .await;

    Ok(ChatTurnOutcome {
        reply: stored_reply,
        thinking,
        state_updated,
        ruleset_loaded: dispatch.ruleset_loaded,
        ruleset_error: dispatch.ruleset_error,
        image_prompt_requested: dispatch.image_prompt_requested,
        dropped: trimmed.dropped,
        condensed: trimmed.condensed,
        summarized: trimmed.summarized,
        summary: trimmed.summary,
        rewritten_history: trimmed.rewritten_history,
    })
}

struct DispatchOutcome {
    ruleset_loaded: Option<String>,
    ruleset_error: Option<String>,
    image_prompt_requested: Option<comfyui::ImagePromptFields>,
}

impl DispatchOutcome {
    fn none() -> Self {
        Self {
            ruleset_loaded: None,
            ruleset_error: None,
            image_prompt_requested: None,
        }
    }
}

/// Turn 2 -- see the module doc comment for why this is a separate
/// completion from turn 1. Never fails the overall turn: a network hiccup
/// or an unparseable reply here is logged and simply means nothing
/// happened this turn, exactly as if the model had said "none" on purpose.
async fn run_dispatch_turn(
    cfg: &AppConfig,
    session_id: &str,
    persona_content: Option<&str>,
    last_user_message: &str,
    last_assistant_reply: &str,
) -> DispatchOutcome {
    let mut outcome = DispatchOutcome::none();
    let state = chat_session::read_state(session_id);

    for _ in 0..MAX_DISPATCH_ATTEMPTS {
        let loaded_names = chat_session::read_loaded_rulesets(session_id);
        let available_rulesets: Vec<ruleset::RulesetSummary> = ruleset::list_rulesets()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| !loaded_names.contains(&r.name))
            .collect();
        let loaded_rulesets: Vec<(String, String)> = loaded_names
            .iter()
            .filter_map(|name| ruleset::load_ruleset(name).ok().map(|c| (name.clone(), c)))
            .collect();

        let system_content = rules::build_dispatch_system_content(
            persona_content,
            &state,
            cfg.chat_state_max_tokens,
            &available_rulesets,
            &loaded_rulesets,
        );
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
        let ruleset_request = rules::extract_ruleset_request(&dispatch_reply)
            .or_else(|| extract_bare_ruleset_fence(&dispatch_reply, &available_rulesets));
        if let Some(name) = ruleset_request {
            if ruleset::load_ruleset(&name).is_ok() {
                if let Err(e) = chat_session::add_loaded_ruleset(session_id, &name) {
                    log::warn!("dispatch turn: failed to record loaded ruleset {name}: {e}");
                    return outcome;
                }
                log::info!("dispatch turn: loaded ruleset \"{name}\"");
                outcome.ruleset_loaded = Some(name);
                continue; // retry now that this ruleset's content is available
            } else {
                log::warn!("dispatch turn: requested unknown ruleset \"{name}\"");
                outcome.ruleset_error = Some(format!("requested unknown ruleset \"{name}\""));
                return outcome;
            }
        }
        // "none" or unparseable -- nothing to do this turn.
        return outcome;
    }
    outcome
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

/// The whole "an image was requested" pipeline: generate, save, persist the
/// path onto the message that requested it, then let the persona react
/// (turn 3). Shared by `main.rs`'s `generate_comfyui_image` Tauri command
/// and `chat_cli.rs` -- same reasoning as the rest of this module, one
/// implementation so the two can't drift.
pub struct ImageGenerationResult {
    pub path: std::path::PathBuf,
    pub data_url: String,
    /// `None` if the reaction call itself failed -- the image already
    /// generated fine, and losing the commentary on it isn't worth
    /// treating as an overall failure of this whole function.
    pub reaction: Option<String>,
}

pub async fn run_full_image_generation(
    cfg: &AppConfig,
    comfy_cfg: &comfyui::ComfyUiConfig,
    session_id: &str,
    fields: &comfyui::ImagePromptFields,
) -> anyhow::Result<ImageGenerationResult> {
    let image = comfyui::generate_image(comfy_cfg, fields).await?;
    let path = comfyui::save_generated_image(comfy_cfg, session_id, &image)?;
    chat_session::append_generated_image(session_id, &path.display().to_string())?;
    let data_url = comfyui::read_as_data_url(&path)?;

    let positive = fields.positive.as_deref().unwrap_or("an image");
    let reaction = match run_image_reaction_turn(
        cfg,
        session_id,
        positive,
        &data_url,
        comfy_cfg.reaction_mode,
    )
    .await
    {
        Ok(Some(text)) => {
            if let Err(e) = chat_session::append_assistant_message(session_id, &text) {
                log::warn!("run_full_image_generation: failed to save reaction: {e}");
            }
            Some(text)
        }
        // `Optional` mode's own considered choice not to comment -- not a
        // failure, nothing to log or persist.
        Ok(None) => None,
        Err(e) => {
            log::warn!("run_full_image_generation: reaction turn failed: {e}");
            None
        }
    };

    Ok(ImageGenerationResult {
        path,
        data_url,
        reaction,
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
/// `reaction_mode` decides whether a comment is mandatory
/// (`ReactionMode::Always`, the original behavior) or left to the model,
/// based on how turn 1 just went and the session's own `state.md` -- both
/// already in `system_content`/persisted history, so no extra context is
/// needed to let it decide. `Ok(None)` is that considered "no comment
/// fits", distinct from an actual request failure (`Err`).
pub async fn run_image_reaction_turn(
    cfg: &AppConfig,
    session_id: &str,
    positive_prompt: &str,
    image_data_url: &str,
    reaction_mode: comfyui::ReactionMode,
) -> anyhow::Result<Option<String>> {
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

    let instruction = match reaction_mode {
        comfyui::ReactionMode::Always => "React to it, briefly, in character.".to_string(),
        comfyui::ReactionMode::Optional => {
            "Given how the conversation has just gone and your current state, decide for \
             yourself whether an in-character comment on it actually fits right now. If it \
             does, react to it briefly, in character. If it doesn't (for example if you were \
             already mid-scene and it's not a natural moment for one), reply with exactly the \
             single word: none"
                .to_string()
        }
    };
    let mut trigger = ChatMessage::text(
        "user",
        format!(
            "[You just finished generating an image described as: {positive_prompt}] {instruction}"
        ),
    );
    trigger.images = vec![image_data_url.to_string()];

    let messages = vec![ChatMessage::text("system", system_content), trigger];
    let reply = crate::llm::send_chat(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        cfg.chat_temperature,
        &messages,
    )
    .await?;

    let reply = rules::strip_thinking_blocks(&reply);
    let state_block = rules::extract_state_block(&reply);
    if let Some(new_state) = &state_block {
        chat_session::update_state(session_id, new_state)?;
    }
    let stored_reply = rules::strip_image_prompt_blocks(&rules::strip_ruleset_requests(
        &rules::strip_state_blocks(&reply),
    ));

    if reaction_mode == comfyui::ReactionMode::Optional
        && stored_reply.trim().eq_ignore_ascii_case("none")
    {
        return Ok(None);
    }
    Ok(Some(stored_reply))
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
                name: "other-tools".to_string(),
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
