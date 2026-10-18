//! The follow-up decision after a character message: should a photo be sent,
//! should the web be searched, and what image prompt to render.
//!
//! Judgement is the model's: it gets the character card as reference and
//! answers a small JSON schema; no keyword lists decide anything. The app
//! only enforces the mechanical parts (what a forced photo or a disabled
//! setting means, a fallback prompt when the builder fails).

use crate::comfyui::ImagePromptFields;
use crate::config::AppConfig;
use crate::llm::{self, ChatMessage};
use serde_json::{json, Value};

const CARD_MAX_CHARS: usize = 3000;
const STATE_MAX_CHARS: usize = 3000;
const MESSAGE_MAX_CHARS: usize = 500;
const BUILDER_MESSAGES: usize = 2;
const POSITIVE_MAX_CHARS: usize = 1500;
const QUERY_MAX_CHARS: usize = 200;

fn builder_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "positive": {"type": "string"},
            "negative": {"type": "string"},
            "width": {"type": "integer"},
            "height": {"type": "integer"},
            "steps": {"type": "integer"},
            "cfg": {"type": "number"},
            "sampler": {"type": "string"},
            "scheduler": {"type": "string"},
            "checkpoint": {"type": "string"},
        },
        "required": ["positive", "negative"],
    })
}

fn cap(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!("{}...", text.chars().take(max).collect::<String>())
}

fn conversation(history: &[ChatMessage], last_n: usize) -> String {
    let relevant: Vec<&ChatMessage> = history
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .collect();
    let start = relevant.len().saturating_sub(last_n);
    relevant[start..]
        .iter()
        .map(|m| {
            let who = if m.role == "user" {
                "user"
            } else {
                "character"
            };
            let text = if m.content.trim().is_empty() && !m.images.is_empty() {
                "(sent an image)".to_string()
            } else {
                cap(&m.content, MESSAGE_MAX_CHARS)
            };
            format!("[{who}]: {text}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn card_block(persona: Option<&str>) -> String {
    match persona.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => format!("<<<\n{}\n>>>", cap(p, CARD_MAX_CHARS)),
        None => "(no character card)".to_string(),
    }
}

fn lenient_string(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").trim().to_string()
}

// Small models judge "should she send a photo?" badly as one yes/no (they
// collapse to false), but reliably copy the words that would justify it. So
// each call asks for quotes, Rust checks every quote really occurs in the
// message it claims to come from (an invented reason is dropped), and the
// persona's photo habit decides how much a visual moment counts. Two short
// calls, one per message, because tiny models otherwise quote the wrong one.

const USER_SIGNALS_SYSTEM: &str = "Read the USER MESSAGE. For each field copy the EXACT words \
from it that prove the field, or \"\" if none. Only copy, never invent.\n\
- see_request: the user asks or hints that they want to see the character, her look, her outfit, \
or something of hers (a photo, \"show me\", \"what are you wearing\", ...).\n\
- search_need: the user asks for real-world current facts (weather, news, prices, scores, recent \
events).\n\
- search_query: a short web search query for search_need, or \"\".\n\
Return only the JSON object.";

const CHARACTER_SIGNALS_SYSTEM: &str = "Read the CHARACTER CARD and the CHARACTER MESSAGE. For \
the text fields copy the EXACT words from the CHARACTER MESSAGE, or \"\" if none. Only copy, \
never invent.\n\
- photo_mention: she sends, takes or shows a photo, picture or selfie.\n\
- visual_detail: she describes a new outfit, a new place, a pose, or an activity worth a \
picture.\n\
- photo_habit: \"often\" if the card says she sends photos or selfies a lot, \"rarely\" if the \
card says she is camera-shy or rarely sends photos, otherwise \"normal\".\n\
Return only the JSON object.";

fn user_signals_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "see_request": {"type": "string"},
            "search_need": {"type": "string"},
            "search_query": {"type": "string"},
        },
        "required": ["see_request", "search_need", "search_query"],
        "additionalProperties": false,
    })
}

fn character_signals_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "photo_mention": {"type": "string"},
            "visual_detail": {"type": "string"},
            "photo_habit": {"type": "string", "enum": ["often", "normal", "rarely"]},
        },
        "required": ["photo_mention", "visual_detail", "photo_habit"],
        "additionalProperties": false,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum PhotoHabit {
    Often,
    #[default]
    Normal,
    Rarely,
}

/// Every text field is a quote already verified against its message (empty
/// when the model gave none or made it up).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Decision {
    pub see_request: String,
    pub photo_mention: String,
    pub visual_detail: String,
    pub habit: PhotoHabit,
    pub search_need: String,
    pub search_query: String,
}

/// What the app actually does, after applying the user's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub image: bool,
    pub photo_reason: String,
    pub search_query: Option<String>,
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// The quote's words appear, in order and contiguously, in `text`
/// (punctuation, case and narration markers ignored).
pub fn quote_occurs(quote: &str, text: &str) -> bool {
    let q = words(quote);
    let t = words(text);
    let letters: usize = q.iter().map(|w| w.chars().count()).sum();
    if q.is_empty() || letters < 3 {
        return false;
    }
    // Models quote with small slips ("she snaps a selfie" for "she quickly
    // snaps a selfie"), so: at least 80% of the quote's words, in order,
    // inside a window barely longer than the quote. Invented text still
    // fails, because its words aren't in the message at all.
    let need = (q.len() * 4).div_ceil(5);
    let span = q.len() + 3;
    (0..t.len()).any(|start| {
        let window = &t[start..t.len().min(start + span)];
        let mut qi = 0;
        let mut hits = 0;
        for w in window {
            if let Some(pos) = q[qi..].iter().position(|x| x == w) {
                qi += pos + 1;
                hits += 1;
                if qi == q.len() {
                    break;
                }
            }
        }
        hits >= need
    })
}

/// Looser check for search: models paraphrase the need ("weather tokyo
/// tomorrow"), so at least half its words must come from the message.
pub fn mostly_from(quote: &str, text: &str) -> bool {
    let q = words(quote);
    if q.is_empty() {
        return false;
    }
    let t = words(text);
    let hits = q.iter().filter(|w| t.contains(w)).count();
    hits * 2 >= q.len()
}

/// Photo evidence is a phrase, not a paragraph: a long "quote" means the
/// model copied the message instead of finding anything in it.
const PHOTO_QUOTE_MAX_WORDS: usize = 20;

fn photo_quote_occurs(quote: &str, text: &str) -> bool {
    words(quote).len() <= PHOTO_QUOTE_MAX_WORDS && quote_occurs(quote, text)
}

fn verified(v: Option<&Value>, text: &str, check: fn(&str, &str) -> bool) -> String {
    let quote = lenient_string(v);
    if check(&quote, text) {
        quote
    } else {
        String::new()
    }
}

fn parse_habit(v: Option<&Value>) -> PhotoHabit {
    match lenient_string(v).to_ascii_lowercase().as_str() {
        "often" => PhotoHabit::Often,
        "rarely" => PhotoHabit::Rarely,
        _ => PhotoHabit::Normal,
    }
}

pub fn parse_user_signals(v: &Value, user_message: &str, d: &mut Decision) {
    d.see_request = verified(v.get("see_request"), user_message, photo_quote_occurs);
    d.search_need = verified(v.get("search_need"), user_message, mostly_from);
    d.search_query = lenient_string(v.get("search_query"));
}

pub fn parse_character_signals(v: &Value, character_message: &str, d: &mut Decision) {
    d.photo_mention = verified(
        v.get("photo_mention"),
        character_message,
        photo_quote_occurs,
    );
    d.visual_detail = verified(
        v.get("visual_detail"),
        character_message,
        photo_quote_occurs,
    );
    d.habit = parse_habit(v.get("photo_habit"));
}

/// A photo went out with the previous exchange (the character's reply or
/// its image reaction), so one on her own initiative now would be a streak.
pub fn photo_in_previous_message(history: &[ChatMessage]) -> bool {
    let mut users = 0;
    for m in history.iter().rev() {
        if m.role == "user" {
            users += 1;
            if users == 2 {
                break;
            }
        } else if users == 1 && !m.generated_images.is_empty() {
            return true;
        }
    }
    false
}

async fn ask(
    cfg: &AppConfig,
    system: &str,
    user: String,
    name: &str,
    schema: Value,
) -> Option<Value> {
    let messages = vec![
        ChatMessage::text("system", system),
        ChatMessage::text("user", user),
    ];
    match llm::send_chat_json(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        &messages,
        0.2,
        Some(200),
        name,
        &schema,
    )
    .await
    {
        Ok(v) => {
            log::info!("director {name}: {v}");
            Some(v)
        }
        Err(e) => {
            log::warn!("director {name} call failed: {e}");
            None
        }
    }
}

// Second pass: a model that must fill a quote field sometimes copies any
// sentence. A focused yes/no about just that phrase is far more reliable for
// small models, and a failed call counts as "no", so errors never send a
// photo or a search on their own.
const CONFIRM_SYSTEM: &str =
    "Answer the question about the quoted text with yes or no. Return only the JSON object.";
// Each question carries a few examples: tiny (1-2B) models answer "no" to
// almost every bare yes/no question, and a handful of examples fixes that.
const CONFIRM_SEE: &str = "Does this message ask to see the other person, a photo of them, \
their look or outfit, or something of theirs?\nExamples:\n\
\"pic please\" -> yes\n\
\"what do you look like right now?\" -> yes\n\
\"what are you wearing?\" -> yes\n\
\"show me your room\" -> yes\n\
\"how was your day?\" -> no\n\
\"I ate ramen\" -> no\n\
\"what are you up to?\" -> no";
const CONFIRM_PHOTO: &str = "Does this sentence say that she is sending, taking, or showing a \
photo, picture or selfie right now?\nExamples:\n\
\"here, I just took this for you\" -> yes\n\
\"she snaps a mirror selfie\" -> yes\n\
\"I love taking pictures at work\" -> no\n\
\"see you later!\" -> no";
const CONFIRM_VISUAL: &str = "Does this phrase show the character, right now, in a new outfit, at \
a new place, striking a pose, or doing an activity worth a picture?\nExamples:\n\
\"she spins around in her new red dress\" -> yes\n\
\"she arrives at the beach\" -> yes\n\
\"she nods\" -> no\n\
\"I might go out later\" -> no";
const CONFIRM_SEARCH: &str = "Is this message asking about the real world outside the chat, \
something a web search could answer (news, weather, prices, sports, public facts, what is new)? \
Personal questions to the other person are not.\nExamples:\n\
\"will it rain in paris tomorrow?\" -> yes\n\
\"who won the game last night?\" -> yes\n\
\"how much is an iphone now?\" -> yes\n\
\"how was your day?\" -> no\n\
\"what are you reading?\" -> no";

async fn confirm(cfg: &AppConfig, question: &str, text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let schema = json!({
        "type": "object",
        "properties": {"answer": {"type": "string", "enum": ["yes", "no"]}},
        "required": ["answer"],
        "additionalProperties": false,
    });
    let messages = vec![
        ChatMessage::text("system", CONFIRM_SYSTEM),
        ChatMessage::text("user", format!("{question}\n\nText: \"{text}\"")),
    ];
    let yes = match llm::send_chat_json(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        &messages,
        0.0,
        Some(24),
        "confirm",
        &schema,
    )
    .await
    {
        Ok(v) => lenient_string(v.get("answer")).eq_ignore_ascii_case("yes"),
        Err(e) => {
            log::warn!("director confirm call failed: {e}");
            false
        }
    };
    log::info!(
        "director confirm {}: {text:?}",
        if yes { "yes" } else { "no" }
    );
    yes
}

/// Runs only the calls whose answer can still change the outcome; a failed
/// call simply contributes no signals (a forced photo still happens).
pub async fn decide(
    cfg: &AppConfig,
    persona: Option<&str>,
    user_message: &str,
    character_message: &str,
    want_photo_signals: bool,
    search_available: bool,
) -> Decision {
    let mut d = Decision::default();
    let user_call = async {
        if want_photo_signals || search_available {
            ask(
                cfg,
                USER_SIGNALS_SYSTEM,
                format!(
                    "USER MESSAGE:\n{}",
                    cap(user_message, 2 * MESSAGE_MAX_CHARS)
                ),
                "user_signals",
                user_signals_schema(),
            )
            .await
        } else {
            None
        }
    };
    let character_call = async {
        if want_photo_signals {
            ask(
                cfg,
                CHARACTER_SIGNALS_SYSTEM,
                format!(
                    "CHARACTER CARD:\n{}\n\nCHARACTER MESSAGE:\n{}",
                    card_block(persona),
                    cap(character_message, 2 * MESSAGE_MAX_CHARS)
                ),
                "character_signals",
                character_signals_schema(),
            )
            .await
        } else {
            None
        }
    };
    let (u, c) = tokio::join!(user_call, character_call);
    if let Some(v) = u {
        parse_user_signals(&v, user_message, &mut d);
    }
    if let Some(v) = c {
        parse_character_signals(&v, character_message, &mut d);
    }
    // A visual moment only matters for a photo-happy character.
    if d.habit != PhotoHabit::Often {
        d.visual_detail.clear();
    }
    if !search_available {
        d.search_need.clear();
    }
    let search_text = if search_available { user_message } else { "" };
    // Asked about the whole message, not the extracted quote: the
    // extraction step sometimes files "what are you wearing?" under the
    // wrong field, while the focused question gets it right.
    let see_text = if want_photo_signals { user_message } else { "" };
    let (see, photo, visual, search) = tokio::join!(
        confirm(cfg, CONFIRM_SEE, see_text),
        confirm(cfg, CONFIRM_PHOTO, &d.photo_mention),
        confirm(cfg, CONFIRM_VISUAL, &d.visual_detail),
        confirm(cfg, CONFIRM_SEARCH, search_text),
    );
    d.see_request = match (see, d.see_request.is_empty()) {
        (false, _) => String::new(),
        (true, true) => cap(user_message, MESSAGE_MAX_CHARS),
        (true, false) => d.see_request,
    };
    for (ok, field) in [
        (photo, &mut d.photo_mention),
        (visual, &mut d.visual_detail),
    ] {
        if !ok {
            field.clear();
        }
    }
    if search {
        if d.search_need.is_empty() {
            d.search_need = cap(user_message, MESSAGE_MAX_CHARS);
        }
    } else {
        d.search_need.clear();
        d.search_query.clear();
    }
    d
}

/// The persona's habit sets how much a visual moment counts: a photo-happy
/// character sends one on her own (not twice in a row), anyone else only
/// when the user asks or she says she's sending one.
pub fn resolve(
    d: &Decision,
    forced: bool,
    auto_images: bool,
    search_available: bool,
    photo_in_previous: bool,
    user_message: &str,
) -> Plan {
    let own_initiative =
        d.habit == PhotoHabit::Often && !d.visual_detail.is_empty() && !photo_in_previous;
    let image = forced
        || (auto_images
            && (!d.see_request.is_empty() || !d.photo_mention.is_empty() || own_initiative));
    let photo_reason = [&d.visual_detail, &d.photo_mention, &d.see_request]
        .into_iter()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_default();
    let search_query =
        (search_available && !d.search_need.is_empty()).then(|| match d.search_query.trim() {
            "" => fallback_search_query(user_message),
            q => cap(q, QUERY_MAX_CHARS),
        });
    Plan {
        image,
        photo_reason,
        search_query: search_query.filter(|q| !q.is_empty()),
    }
}

// --- Search query fallback ---

const QUERY_FILLER: &[&str] = &[
    "can you",
    "could you",
    "would you",
    "will you",
    "please",
    "pls",
    "hey",
    "search the web for",
    "search for",
    "search",
    "look up",
    "google",
    "find me",
    "find out",
    "tell me",
    "check",
];

/// Removes every `open..close` span; an unpaired opener is dropped on its
/// own when `drop_unpaired`, else kept as literal text.
fn drop_spans(text: &str, open: char, close: char, drop_unpaired: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(open) {
        out.push_str(&rest[..i]);
        let after = &rest[i + open.len_utf8()..];
        match after.find(close) {
            Some(j) => {
                out.push(' ');
                rest = &after[j + close.len_utf8()..];
            }
            None => {
                if !drop_unpaired {
                    out.push(open);
                }
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The user's message without actions, thoughts, quote marks and leading
/// filler. Handles `*action*` / `(thought)` plus the older `//action//` and
/// `||speech||` markers.
pub fn fallback_search_query(user_message: &str) -> String {
    let legacy: String = user_message
        .split("//")
        .enumerate()
        .filter(|(i, _)| i % 2 == 0)
        .map(|(_, s)| s)
        .collect::<Vec<_>>()
        .join(" ");
    let outside_narration = drop_spans(&legacy.replace("**", "*"), '*', '*', true);
    let outside_narration = drop_spans(&outside_narration, '(', ')', false);
    let mut text = outside_narration
        .replace("||", " ")
        .replace(['"', '\u{201c}', '\u{201d}'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    loop {
        let lower = text.to_ascii_lowercase();
        let Some(filler) = QUERY_FILLER.iter().find(|f| {
            lower
                .strip_prefix(*f)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', ',']))
        }) else {
            break;
        };
        text = text[filler.len()..]
            .trim_start_matches([' ', ','])
            .to_string();
    }
    cap(
        text.trim_matches(|c: char| c.is_whitespace() || c == '?'),
        QUERY_MAX_CHARS,
    )
}

// --- Image prompt builder ---

fn builder_system_content(style_rules: &str) -> String {
    let mut out =
        String::from("You write prompts for an image generator (Stable Diffusion-style, comma-separated tags).\n");
    if !style_rules.trim().is_empty() {
        out.push_str("\nSTYLE RULES (follow these exactly):\n<<<\n");
        out.push_str(style_rules.trim());
        out.push_str("\n>>>\n");
    }
    out.push_str(
        "\nRules:\n\
- Describe the character's appearance from the card, plus her current outfit, pose, expression and \
setting from the state and the latest messages, plus the photo framing (a selfie if she is taking \
it, and so on).\n\
- positive is comma-separated tags only, no sentences. negative lists what to avoid.\n\
- Do not add tags the style rules forbid.\n\
- Set width, height, steps, cfg, sampler, scheduler and checkpoint only if the style rules specify \
them; otherwise leave them out.\n\
Return only the JSON object.",
    );
    out
}

fn builder_user_content(
    persona: Option<&str>,
    state: &str,
    history: &[ChatMessage],
    photo_reason: &str,
    hint: Option<&str>,
) -> String {
    let state = match state.trim() {
        "" => "(none)".to_string(),
        s => cap(s, STATE_MAX_CHARS),
    };
    let mut out = format!(
        "CHARACTER CARD:\n{}\n\nCURRENT STATE:\n{state}\n\nRECENT CONVERSATION (oldest first):\n{}",
        card_block(persona),
        conversation(history, BUILDER_MESSAGES),
    );
    if !photo_reason.trim().is_empty() {
        out.push_str(&format!(
            "\n\nWHAT THE PHOTO SHOWS: {}",
            photo_reason.trim()
        ));
    }
    if let Some(hint) = hint.map(str::trim).filter(|h| !h.is_empty()) {
        out.push_str(&format!("\n\nUSER REQUEST FOR THE PHOTO: {hint}"));
    }
    out
}

/// Newlines become commas, tags are trimmed and de-duplicated
/// case-insensitively (order kept), and the whole is capped at
/// `POSITIVE_MAX_CHARS`.
pub fn clean_tags(raw: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut out = String::new();
    for tag in raw.replace(['\r', '\n'], ",").split(',') {
        let tag = tag.split_whitespace().collect::<Vec<_>>().join(" ");
        if tag.is_empty() || !seen.insert(tag.to_lowercase()) {
            continue;
        }
        let extra = if out.is_empty() { 0 } else { 2 } + tag.chars().count();
        if out.chars().count() + extra > POSITIVE_MAX_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(&tag);
    }
    out
}

fn lenient_int(v: Option<&Value>) -> Option<i64> {
    let n = match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<f64>().ok().map(|f| f as i64),
        _ => None,
    }?;
    (n > 0).then_some(n)
}

fn lenient_float(v: Option<&Value>) -> Option<f64> {
    let n = match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    (n > 0.0).then_some(n)
}

fn nonempty(v: Option<&Value>) -> Option<String> {
    Some(lenient_string(v)).filter(|s| !s.is_empty())
}

/// `None` when the builder gave no usable positive prompt.
pub fn fields_from_value(v: &Value) -> Option<ImagePromptFields> {
    let positive = clean_tags(v.get("positive").and_then(Value::as_str).unwrap_or(""));
    if positive.is_empty() {
        return None;
    }
    let negative = clean_tags(v.get("negative").and_then(Value::as_str).unwrap_or(""));
    Some(ImagePromptFields {
        positive: Some(positive),
        negative: (!negative.is_empty()).then_some(negative),
        width: lenient_int(v.get("width")),
        height: lenient_int(v.get("height")),
        steps: lenient_int(v.get("steps")),
        cfg: lenient_float(v.get("cfg")),
        sampler: nonempty(v.get("sampler")),
        scheduler: nonempty(v.get("scheduler")),
        checkpoint: nonempty(v.get("checkpoint")),
    })
}

const FALLBACK_STATE_KEYS: &[&str] = &[
    "outfit",
    "cloth",
    "location",
    "activity",
    "pose",
    "expression",
];

/// A decided photo always has a prompt: the character's name, outfit-like
/// state values, a framing tag and the user's hint.
pub fn fallback_fields(
    persona_name: Option<&str>,
    raw_state: &str,
    hint: Option<&str>,
) -> ImagePromptFields {
    let mut parts: Vec<String> = Vec::new();
    if let Some(name) = persona_name.map(str::trim).filter(|n| !n.is_empty()) {
        parts.push(name.to_string());
    }
    for field in crate::chat_prompts::parse_state_fields(raw_state) {
        let key = field.name.to_lowercase();
        let value = field.value.trim();
        if FALLBACK_STATE_KEYS.iter().any(|k| key.contains(k))
            && !value.is_empty()
            && !value.eq_ignore_ascii_case("none")
        {
            parts.push(value.to_string());
        }
    }
    parts.push("looking at viewer".to_string());
    if let Some(hint) = hint.map(str::trim).filter(|h| !h.is_empty()) {
        parts.push(hint.to_string());
    }
    ImagePromptFields {
        positive: Some(clean_tags(&parts.join(", "))),
        ..Default::default()
    }
}

pub struct BuilderInput<'a> {
    pub persona_name: Option<&'a str>,
    pub persona: Option<&'a str>,
    pub raw_state: &'a str,
    pub history: &'a [ChatMessage],
    pub photo_reason: &'a str,
    pub hint: Option<&'a str>,
    pub style_rules: &'a str,
}

/// One schema-constrained call; any failure or an empty `positive` falls back
/// to `fallback_fields`.
pub async fn build_image_prompt(cfg: &AppConfig, input: &BuilderInput<'_>) -> ImagePromptFields {
    let messages = vec![
        ChatMessage::text("system", builder_system_content(input.style_rules)),
        ChatMessage::text(
            "user",
            builder_user_content(
                input.persona,
                input.raw_state,
                input.history,
                input.photo_reason,
                input.hint,
            ),
        ),
    ];
    let built = llm::send_chat_json(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        &messages,
        0.4,
        Some(600),
        "image_prompt",
        &builder_schema(),
    )
    .await;
    match built {
        Ok(v) => fields_from_value(&v).unwrap_or_else(|| {
            log::warn!("image prompt builder returned no positive prompt, using the fallback");
            fallback_fields(input.persona_name, input.raw_state, input.hint)
        }),
        Err(e) => {
            log::warn!("image prompt builder failed, using the fallback: {e}");
            fallback_fields(input.persona_name, input.raw_state, input.hint)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(role: &str, text: &str) -> ChatMessage {
        ChatMessage::text(role, text)
    }

    fn with_photo(mut msg: ChatMessage) -> ChatMessage {
        msg.generated_images.push("/x/a.png".into());
        msg
    }

    #[test]
    fn quotes_must_really_occur_in_their_message() {
        let msg = "// She smiles // || Check this out! I'm wearing my pink hoodie. ||";
        assert!(quote_occurs("check this out", msg));
        assert!(quote_occurs("I'm wearing my pink hoodie", msg));
        assert!(!quote_occurs("she sends a selfie", msg), "invented");
        assert!(!quote_occurs("I", msg), "too short to mean anything");
        assert!(!quote_occurs("", msg));
        assert!(!quote_occurs("pink check", msg), "words must be in order");
        let selfie = "She flashes a huge, bright smile and quickly snaps a selfie with her phone.";
        assert!(
            quote_occurs("she snaps a selfie with her phone", selfie),
            "one dropped word"
        );
        assert!(!quote_occurs(
            "she sends, takes or shows a photo, picture or selfie",
            selfie
        ));
        assert!(
            !quote_occurs("she sends a photo of herself", selfie),
            "invented"
        );
        assert!(mostly_from(
            "weather tokyo tomorrow",
            "will it rain in tokyo tomorrow?"
        ));
        assert!(!mostly_from("election results", "I had pasta"));
    }

    #[test]
    fn signals_drop_invented_quotes() {
        let mut d = Decision::default();
        parse_user_signals(
            &json!({"see_request": "show me", "search_need": "", "search_query": ""}),
            "how was your day?",
            &mut d,
        );
        assert_eq!(d, Decision::default(), "quote not in the message");
        parse_user_signals(
            &json!({"see_request": "what are you wearing", "search_need": "weather tokyo",
                    "search_query": "tokyo weather tomorrow"}),
            "what are you wearing? also is it raining in tokyo",
            &mut d,
        );
        assert_eq!(d.see_request, "what are you wearing");
        assert_eq!(d.search_query, "tokyo weather tomorrow");
        parse_character_signals(
            &json!({"photo_mention": "", "visual_detail": "on the beach in her new bikini",
                    "photo_habit": "Often"}),
            "// she spins around on the beach in her new bikini //",
            &mut d,
        );
        assert_eq!(d.habit, PhotoHabit::Often);
        assert_eq!(d.visual_detail, "on the beach in her new bikini");
    }

    #[test]
    fn previous_photo_is_detected_through_the_reaction() {
        assert!(!photo_in_previous_message(&[]));
        let h = vec![
            m("user", "pic?"),
            with_photo(m("assistant", "here")),
            m("assistant", "reaction to the photo"),
            m("user", "nice"),
            m("assistant", "thanks"),
        ];
        assert!(photo_in_previous_message(&h));
        let h = vec![
            with_photo(m("assistant", "here")),
            m("user", "and?"),
            m("assistant", "middle"),
            m("user", "ok"),
            m("assistant", "latest"),
        ];
        assert!(!photo_in_previous_message(&h), "two exchanges ago is fine");
    }

    #[test]
    fn long_photo_quotes_are_not_evidence() {
        let msg = format!("{} she takes a selfie", "word ".repeat(40));
        let mut d = Decision::default();
        parse_character_signals(
            &json!({"photo_mention": msg.trim(), "visual_detail": "she takes a selfie",
                    "photo_habit": "normal"}),
            &msg,
            &mut d,
        );
        assert!(d.photo_mention.is_empty());
        assert_eq!(d.visual_detail, "she takes a selfie");
    }

    #[test]
    fn resolve_follows_the_persona_habit_and_settings() {
        let visual = |habit| Decision {
            visual_detail: "on the beach".into(),
            habit,
            ..Default::default()
        };
        assert!(resolve(&visual(PhotoHabit::Often), false, true, false, false, "").image);
        assert!(
            !resolve(&visual(PhotoHabit::Often), false, true, false, true, "").image,
            "not twice in a row on her own"
        );
        assert!(!resolve(&visual(PhotoHabit::Normal), false, true, false, false, "").image);
        assert!(!resolve(&visual(PhotoHabit::Rarely), false, true, false, false, "").image);

        let asked = Decision {
            see_request: "show me".into(),
            habit: PhotoHabit::Rarely,
            ..Default::default()
        };
        let p = resolve(&asked, false, true, false, true, "");
        assert!(p.image, "asking always works");
        assert_eq!(p.photo_reason, "show me");
        assert!(
            !resolve(&asked, false, false, false, false, "").image,
            "auto off"
        );
        assert!(
            resolve(&Decision::default(), true, false, false, false, "").image,
            "forced"
        );

        let search = Decision {
            search_need: "weather in Tokyo".into(),
            ..Default::default()
        };
        assert_eq!(
            resolve(
                &search,
                false,
                true,
                true,
                false,
                "Can you search for the weather in Tokyo?"
            )
            .search_query
            .as_deref(),
            Some("the weather in Tokyo")
        );
        assert_eq!(
            resolve(&search, false, true, false, false, "x").search_query,
            None
        );
    }

    #[test]
    fn fallback_query_strips_narration_and_filler() {
        assert_eq!(
            fallback_search_query(
                "// leans in // || please look up who won the match yesterday? ||"
            ),
            "who won the match yesterday"
        );
        assert_eq!(
            fallback_search_query(
                "*leans in* (hope she knows) \"please look up who won the **match** yesterday?\" *smiles*"
            ),
            "who won the yesterday"
        );
        assert_eq!(
            fallback_search_query("hey, can you tell me the price of gold"),
            "the price of gold"
        );
        assert_eq!(fallback_search_query("searching"), "searching");
    }

    #[test]
    fn clean_tags_collapses_dedupes_and_caps() {
        assert_eq!(
            clean_tags("1girl,\n  red dress , 1GIRL,\r\nred   dress, smile,, "),
            "1girl, red dress, smile"
        );
        let long: String = (0..2000)
            .map(|i| format!("tag{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let out = clean_tags(&long);
        assert!(out.chars().count() <= POSITIVE_MAX_CHARS);
        assert!(out.starts_with("tag0, tag1") && !out.ends_with(','));
    }

    #[test]
    fn builder_output_becomes_fields() {
        let f = fields_from_value(&json!({
            "positive": "1girl, red dress,\nselfie, 1girl",
            "negative": "blurry\nblurry, bad hands",
            "width": "832", "height": 1216.0, "steps": 0, "cfg": 7,
            "sampler": " euler ", "scheduler": "", "checkpoint": "m.safetensors"
        }))
        .unwrap();
        assert_eq!(f.positive.as_deref(), Some("1girl, red dress, selfie"));
        assert_eq!(f.negative.as_deref(), Some("blurry, bad hands"));
        assert_eq!((f.width, f.height, f.steps), (Some(832), Some(1216), None));
        assert_eq!(f.cfg, Some(7.0));
        assert_eq!(f.sampler.as_deref(), Some("euler"));
        assert_eq!(f.scheduler, None);
        assert_eq!(f.checkpoint.as_deref(), Some("m.safetensors"));
        assert!(fields_from_value(&json!({"positive": " , ", "negative": "x"})).is_none());
        assert!(fields_from_value(&json!("nope")).is_none());
    }

    #[test]
    fn fallback_prompt_uses_name_outfit_state_and_hint() {
        let state =
            r#"{"Outfit":"red dress","Mood":"happy","Location":"beach","Pose":"none","HP":"9"}"#;
        let f = fallback_fields(Some("Asuna"), state, Some("on the beach"));
        let positive = f.positive.unwrap();
        assert_eq!(
            positive,
            "Asuna, beach, red dress, looking at viewer, on the beach"
        );
        assert!(f.negative.is_none());

        let bare = fallback_fields(None, "not json", None);
        assert_eq!(bare.positive.as_deref(), Some("looking at viewer"));
    }

    #[test]
    fn builder_prompts_carry_the_style_rules_verbatim() {
        let sys = builder_system_content("- always start positive with: masterpiece");
        assert!(sys.contains("STYLE RULES (follow these exactly)"));
        assert!(sys.contains("- always start positive with: masterpiece"));
        assert!(!builder_system_content("  ").contains("STYLE RULES"));

        let user = builder_user_content(
            Some("Asuna, long hair"),
            r#"{"Outfit":"dress"}"#,
            &[m("user", "show me")],
            "a selfie in a dress",
            Some("at the beach"),
        );
        assert!(
            user.contains("Asuna, long hair") && user.contains("WHAT THE PHOTO SHOWS: a selfie")
        );
        assert!(user.contains("USER REQUEST FOR THE PHOTO: at the beach"));
    }
}
