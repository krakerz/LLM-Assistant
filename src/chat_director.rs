//! The follow-up decision after a character message: should a photo be sent,
//! should the web be searched, and what image prompt to render.
//!
//! Judgement is the model's: it gets the character card as reference and
//! answers a small JSON schema; no keyword lists decide anything. The app
//! only enforces the mechanical parts (what a forced photo or a disabled
//! setting means, a fallback prompt when the builder fails).

use crate::comfyui::{ImagePromptFields, SeedMode, SizePreset};
use crate::config::AppConfig;
use crate::llm::{self, ChatMessage};
use serde_json::{json, Value};
use std::collections::HashSet;

const CARD_MAX_CHARS: usize = 3000;
const STATE_MAX_CHARS: usize = 3000;
const MESSAGE_MAX_CHARS: usize = 500;
const BUILDER_MESSAGES: usize = 2;
const POSITIVE_MAX_CHARS: usize = 1500;
const PREVIOUS_PROMPT_MAX_CHARS: usize = 600;
const QUERY_MAX_CHARS: usize = 200;

fn builder_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "positive": {"type": "string"},
            "negative": {"type": "string"},
            "size": {"type": "string", "enum": ["portrait", "landscape", "square"]},
            "seed": {"type": "string", "enum": ["new", "same"]},
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

/// How photos get triggered (the `chat_auto_images` and
/// `chat_photo_every_reply` settings read together).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhotoMode {
    Off,
    /// The director decides per reply.
    Auto,
    EveryReply,
}

/// Shown to the prompt builder as what the photo depicts when no reason exists.
pub const EVERY_REPLY_PHOTO_REASON: &str = "her current action, pose and expression in this very \
reply (expression from her mood), with her current outfit and location";

impl PhotoMode {
    pub fn from_config(cfg: &AppConfig) -> Self {
        match (cfg.chat_auto_images, cfg.chat_photo_every_reply) {
            (false, _) => Self::Off,
            (true, false) => Self::Auto,
            (true, true) => Self::EveryReply,
        }
    }
}

/// Which photo machinery a follow-up uses, given the mode and whether ComfyUI
/// is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhotoFlow {
    /// Passed to `resolve` as `auto_images`.
    pub auto_images: bool,
    /// Whether the director must read the reply for photo signals.
    pub director_signals: bool,
    /// The photo goes out regardless of the decision and the previous photo.
    pub force: bool,
}

pub fn photo_flow(mode: PhotoMode, comfy_configured: bool, forced: bool) -> PhotoFlow {
    let auto_images = mode != PhotoMode::Off && comfy_configured;
    let every = mode == PhotoMode::EveryReply && comfy_configured;
    PhotoFlow {
        auto_images,
        director_signals: auto_images && !forced && !every,
        force: forced || every,
    }
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
/// Asked only after a photo went out: a remark on that photo is not a request
/// for a new one (a small model files "why is there X in it?" as see_request).
const CONFIRM_PHOTO_REMARK: &str = "The user just got a photo from the other person. Is this \
message only a comment or question about that photo (what is in it, something odd in it), \
without asking for a new photo or to see more?\nExamples:\n\
\"why is there a cat on your head?\" -> yes\n\
\"lol your hand looks weird in that one\" -> yes\n\
\"cute! is that your room?\" -> yes\n\
\"send another one\" -> no\n\
\"can I see your outfit from the back?\" -> no\n\
\"show me closer\" -> no";
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
\"show me where you are\" -> no\n\
\"what date is it today?\" -> no\n\
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
    after_photo: bool,
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
    // Only the extra call when it can change the answer.
    let see = see && !(after_photo && confirm(cfg, CONFIRM_PHOTO_REMARK, user_message).await);
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
- The scene, pose, action and expression come from THIS PHOTO (the reason, the user's request and her \
latest message). Add the photo framing (a selfie if she is taking it, and so on).\n\
- The background state only supplies her outfit, the place, and her mood as the facial expression. Turn \
the mood into expression tags (angry -> angry, frown; lustful -> seductive smile, half-closed eyes; \
happy -> smile). Never copy sentences from the state.\n\
- Describe her appearance from the card.\n\
- Framing words in the user's request decide the shot: \"close\", \"in front of my face\", \"zoom\" -> \
close-up; \"full body\"/\"whole body\" -> full body; \"from behind\"/\"your back\" -> from behind; \
\"from above\"/\"from below\" -> from above/from below.\n\
- Her clothes come ONLY from the state's outfit (unless THIS PHOTO says she changes or undresses): \
always include them as tags, and never use other outfits the card lists.\n\
- positive is comma-separated tags only, no sentences: short tags of 1-4 words, no labels like \
\"outfit:\", and no size or orientation words (portrait, landscape) in positive. negative lists what \
to avoid.\n\
- Do not add tags the style rules forbid.\n\
- size: portrait for standing or full-body shots and selfies, landscape for wide scenes or lying \
down, square for face close-ups. Follow the style rules if they say otherwise.\n\
- seed: \"same\" only when the user asks for the previous picture again with a change (same photo, \
another outfit or expression); otherwise \"new\".\n\
- Set steps, cfg, sampler, scheduler and checkpoint only if the style rules specify them; otherwise \
leave them out.\n\
Return only the JSON object.",
    );
    out
}

/// State keys (lowercase substrings) that can show up in a picture. Mood stays:
/// it becomes her expression.
const VISUAL_STATE_KEYS: &[&str] = &[
    "outfit",
    "cloth",
    "wear",
    "dress",
    "location",
    "place",
    "setting",
    "activity",
    "pose",
    "expression",
    "mood",
    "emotion",
    "hair",
    "makeup",
    "accessor",
];

fn is_visual_key(name: &str) -> bool {
    let name = name.to_lowercase();
    VISUAL_STATE_KEYS.iter().any(|k| name.contains(k))
}

/// The state fields worth drawing, one `name: value` per line; thoughts and
/// stats (mind, arousal, user_state, ...) are dropped so the builder can't
/// copy them into the prompt.
fn visual_state(raw_state: &str) -> String {
    crate::chat_prompts::parse_state_fields(raw_state)
        .into_iter()
        .filter(|f| {
            let value = f.value.trim();
            is_visual_key(&f.name) && !value.is_empty() && !value.eq_ignore_ascii_case("none")
        })
        .map(|f| format!("{}: {}", f.name, f.value.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn builder_user_content(
    gender: Gender,
    persona: Option<&str>,
    state: &str,
    history: &[ChatMessage],
    photo_reason: &str,
    hint: Option<&str>,
    previous_prompt: Option<&str>,
) -> String {
    let state = match visual_state(state).as_str() {
        "" => "(none)".to_string(),
        s => cap(s, STATE_MAX_CHARS),
    };
    let mut this_photo: Vec<String> = Vec::new();
    if let Some(last) = history
        .iter()
        .rev()
        .find(|m| m.role == "assistant" && !m.content.trim().is_empty())
    {
        this_photo.push(format!(
            "{} latest message: {}",
            gendered("Her", gender),
            cap(&last.content, 800)
        ));
    }
    if !photo_reason.trim().is_empty() {
        let reason = if photo_reason.trim() == EVERY_REPLY_PHOTO_REASON {
            gendered(photo_reason.trim(), gender)
        } else {
            photo_reason.trim().to_string()
        };
        this_photo.push(format!("What the photo shows: {reason}"));
    }
    if let Some(hint) = hint.map(str::trim).filter(|h| !h.is_empty()) {
        this_photo.push(format!(
            "The user asked to see: {hint}\n{}",
            gendered(
                "(Use this only for framing and subject. Her outfit, pose and action come from \
her latest message and her state, never from this request.)",
                gender
            )
        ));
    }
    let this_photo = if this_photo.is_empty() {
        "(nothing specific)".to_string()
    } else {
        this_photo.join("\n")
    };
    let mut out = format!(
        "THIS PHOTO:\n{this_photo}\n\nCHARACTER CARD:\n{}\n\nBACKGROUND STATE (outfit, place and mood, \
unless {} latest message says otherwise):\n{state}\n\nRECENT CONVERSATION (oldest first):\n{}",
        card_block(persona),
        gendered("her", gender),
        conversation(history, BUILDER_MESSAGES),
    );
    if let Some(prev) = previous_prompt.map(str::trim).filter(|p| !p.is_empty()) {
        out.push_str(&format!(
            "\n\nPREVIOUS PHOTO'S PROMPT (pick a different pose, camera angle and composition unless the \
user asks for the same photo again):\n{}",
            cap(prev, PREVIOUS_PROMPT_MAX_CHARS)
        ));
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

/// Whole tags that only state the picture's shape; the app owns the size.
const SHAPE_TAGS: &[&str] = &[
    "portrait",
    "landscape",
    "square",
    "portrait orientation",
    "landscape orientation",
    "vertical",
    "horizontal",
];

/// Labels of state fields that must never reach the prompt.
const NON_VISUAL_LABELS: &[&str] = &[
    "mind",
    "state",
    "user_state",
    "user",
    "arousal",
    "thought",
    "thoughts",
];

const POSITIVE_TAG_MAX_WORDS: usize = 6;

/// ComfyUI/booru syntax that looks like `label:` but is part of the tag.
const KEPT_PREFIXES: &[&str] = &["embedding", "rating", "lora", "score"];

/// Splits a `label:` prefix (one word or snake_case word) off a tag. A weight
/// like `smile:1.2` and `(smile:1.2)` are not labels.
fn split_label(tag: &str) -> (Option<&str>, &str) {
    let Some((label, rest)) = tag.split_once(':') else {
        return (None, tag);
    };
    let rest = rest.trim();
    let is_label = !label.is_empty()
        && !KEPT_PREFIXES.contains(&label.to_lowercase().as_str())
        && label.chars().all(|c| c.is_ascii_alphabetic() || c == '_')
        && !rest.is_empty()
        && rest.parse::<f64>().is_err();
    if is_label {
        (Some(label), rest)
    } else {
        (None, tag)
    }
}

/// `clean_tags` for the positive prompt: also splits on `;` and sentence
/// periods, strips `label:` prefixes, drops state-label tags, tags longer than
/// 6 words and bare size/orientation words.
pub fn clean_positive(raw: &str) -> String {
    let mut flat = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ';' => flat.push(','),
            '.' if chars.peek().map_or(true, |n| n.is_whitespace()) => flat.push(','),
            c => flat.push(c),
        }
    }
    let kept: Vec<String> = flat
        .replace(['\r', '\n'], ",")
        .split(',')
        .filter_map(|tag| {
            let tag = tag.split_whitespace().collect::<Vec<_>>().join(" ");
            let (label, value) = split_label(&tag);
            if label.is_some_and(|l| NON_VISUAL_LABELS.contains(&l.to_lowercase().as_str())) {
                return None;
            }
            let lower = value.to_lowercase();
            if lower.contains("user state") || lower.contains("user_state") {
                return None;
            }
            if value.split_whitespace().count() > POSITIVE_TAG_MAX_WORDS
                || SHAPE_TAGS.contains(&lower.as_str())
            {
                return None;
            }
            Some(value.to_string())
        })
        .collect();
    escape_parens(&clean_tags(&kept.join(", ")))
}

/// `(text:1.2)`: a ComfyUI weight group, which must keep its bare parens.
fn is_weight_group(inner: &str) -> bool {
    inner.rsplit_once(':').is_some_and(|(label, weight)| {
        let weight = weight.trim();
        !label.trim().is_empty()
            && !weight.is_empty()
            && weight.chars().all(|c| c.is_ascii_digit() || c == '.')
            && weight.parse::<f64>().is_ok()
    })
}

/// ComfyUI reads a bare `(x)` as emphasis, so character tags such as
/// `asuna (blue archive)` need `\(`..`\)`. Weight groups and already escaped
/// parens stay as they are, so running it twice changes nothing.
pub fn escape_parens(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        let len = c.len_utf8();
        match c {
            '\\' if rest[len..].starts_with(['(', ')']) => {
                out.push_str(&rest[..len + 1]);
                rest = &rest[len + 1..];
                continue;
            }
            '(' => {
                let after = &rest[len..];
                let group = after
                    .find(['(', ')'])
                    .filter(|&i| after[i..].starts_with(')') && is_weight_group(&after[..i]));
                if let Some(i) = group {
                    out.push_str(&rest[..len + i + 1]);
                    rest = &rest[len + i + 1..];
                    continue;
                }
                out.push_str("\\(");
            }
            ')' => out.push_str("\\)"),
            c => out.push(c),
        }
        rest = &rest[len..];
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

fn choice(v: Option<&Value>) -> Option<Value> {
    let s = v?.as_str()?.trim().to_lowercase();
    Some(Value::String(s))
}

/// `None` when the builder gave no usable positive prompt.
pub fn fields_from_value(v: &Value) -> Option<ImagePromptFields> {
    let positive = clean_positive(v.get("positive").and_then(Value::as_str).unwrap_or(""));
    if positive.is_empty() {
        return None;
    }
    let negative = clean_tags(v.get("negative").and_then(Value::as_str).unwrap_or(""));
    Some(ImagePromptFields {
        positive: Some(positive),
        negative: (!negative.is_empty()).then_some(negative),
        // Pixel sizes and seeds are the app's: only the named choices count.
        size: choice(v.get("size")).and_then(|c| serde_json::from_value::<SizePreset>(c).ok()),
        seed_mode: choice(v.get("seed")).and_then(|c| serde_json::from_value::<SeedMode>(c).ok()),
        steps: lenient_int(v.get("steps")),
        cfg: lenient_float(v.get("cfg")),
        sampler: nonempty(v.get("sampler")),
        scheduler: nonempty(v.get("scheduler")),
        checkpoint: nonempty(v.get("checkpoint")),
        ..Default::default()
    })
}

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
        if is_visual_key(&key) && !value.is_empty() && !value.eq_ignore_ascii_case("none") {
            parts.push(value.to_string());
        }
    }
    parts.push("looking at viewer".to_string());
    if let Some(hint) = hint.map(str::trim).filter(|h| !h.is_empty()) {
        parts.push(hint.to_string());
    }
    ImagePromptFields {
        positive: Some(escape_parens(&clean_tags(&parts.join(", ")))),
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
    pub previous_prompt: Option<&'a str>,
}

/// Words that mean the photo shows her (partly) out of her outfit on purpose.
const UNDRESS_WORDS: &[&str] = &[
    "nude",
    "naked",
    "topless",
    "undress",
    "bottomless",
    "lingerie",
];

/// Clothing-state phrases that must show up as a tag; the builder tends to
/// keep the garment list and miss that part of it is gone.
const CLOTHING_STATE_TAGS: &[(&str, &[&str])] = &[
    (
        "nude",
        &[
            "nude",
            "naked",
            "fully undressed",
            "completely undressed",
            "nothing on",
        ],
    ),
    (
        "topless",
        &[
            "topless",
            "top off",
            "top entirely off",
            "top completely off",
            "top removed",
            "shirt off",
            "exposed above the waist",
            "bare chest",
        ],
    ),
];

/// Words that mark a state segment as describing a change, not a garment.
const CLOTHING_CHANGE_WORDS: &[&str] = &[
    "off",
    "remain",
    "remains",
    "removed",
    "falling",
    "discarded",
    "being",
    "completely",
    "entirely",
];

/// Garment words by class; anything matching none of them (hair ribbon,
/// choker, halo, earrings, glasses) is an accessory and always stays.
const BOTTOM_WORDS: &[&str] = &[
    "skirt",
    "shorts",
    "pants",
    "jeans",
    "trousers",
    "leggings",
    "panties",
    "underwear",
    "bottom",
];
/// Full-body garments with a recognisable bottom half: swapped for a skirt.
const UNIFORM_WORDS: &[&str] = &["uniform", "serafuku"];
/// One-piece garments with no usable bottom half: dropped.
const ONE_PIECE_WORDS: &[&str] = &[
    "dress",
    "sundress",
    "nightgown",
    "gown",
    "kimono",
    "yukata",
    "bodysuit",
    "leotard",
    "swimsuit",
    "bikini",
    "jumpsuit",
    "romper",
    "overalls",
];
const TOP_WORDS: &[&str] = &[
    "shrug",
    "bolero",
    "capelet",
    "shirt",
    "blouse",
    "sweater",
    "jacket",
    "hoodie",
    "cardigan",
    "vest",
    "bra",
    "camisole",
    "blazer",
    "coat",
    "top",
    "tank",
    "lingerie",
    "necktie",
    "bowtie",
    "neckerchief",
    "tie",
    "cape",
    "obi",
];
const LEGWEAR_WORDS: &[&str] = &[
    "thighhighs",
    "highs",
    "kneehighs",
    "stockings",
    "socks",
    "pantyhose",
    "tights",
    "shoes",
    "boots",
    "loafers",
    "sneakers",
];
const COLOUR_WORDS: &[&str] = &[
    "blue", "red", "black", "white", "pink", "green", "yellow", "purple", "brown", "grey", "gray",
    "orange",
];

/// Splits on commas that are not inside parentheses (escaped or bare).
fn split_tags_outside_parens(text: &str) -> Vec<String> {
    let (mut tags, mut cur, mut depth) = (Vec::new(), String::new(), 0i32);
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            ',' if depth == 0 => {
                tags.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    tags.push(cur);
    tags.into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// When the clothing state says she is topless or nude, the builder often keeps
/// a garment tag that still covers her. Topless: a uniform becomes its bottom
/// half ("blue pleated skirt" when a colour is named, else "pleated skirt"),
/// one-piece garments (dress, bodysuit, swimsuit...) are dropped, as are pure
/// tops and neckwear. Bottoms, legwear and accessories stay. Nude: every
/// garment goes (legwear too, unless the state names it); accessories stay.
/// Text after " - " in a garment tag is a leaked state sentence and goes with it.
fn strip_removed_garments(positive: &str, nude: bool, clothing: &str) -> String {
    let tags = split_tags_outside_parens(positive);
    let has = |words: &[String], list: &[&str]| words.iter().any(|w| list.contains(&w.as_str()));
    let mut bottom_present = tags.iter().any(|t| {
        let head = t.split(" - ").next().unwrap_or(t);
        has(&norm_words(head), BOTTOM_WORDS)
    });
    let state_words = norm_words(clothing);
    let mut out: Vec<String> = Vec::with_capacity(tags.len() + 1);
    for tag in &tags {
        let head = tag.split(" - ").next().unwrap_or(tag).trim();
        let words = norm_words(head);
        // "girl on top" / "boy on top" is a pose, not a garment.
        let hair_item = words.iter().any(|w| w == "hair")
            || head.eq_ignore_ascii_case("girl on top")
            || head.eq_ignore_ascii_case("boy on top");
        if has(&words, BOTTOM_WORDS) {
            if nude {
                log::info!("image prompt: nude, dropping '{head}'");
            } else {
                out.push(head.to_string());
            }
        } else if has(&words, UNIFORM_WORDS) {
            log::info!("image prompt: undressed, replacing '{head}'");
            if !nude && !bottom_present {
                let colour = words.iter().find(|w| COLOUR_WORDS.contains(&w.as_str()));
                out.push(match colour {
                    Some(c) => format!("{c} pleated skirt"),
                    None => "pleated skirt".to_string(),
                });
                bottom_present = true;
            }
        } else if !nude && !bottom_present && words.iter().any(|w| w == "bikini") {
            log::info!("image prompt: topless, keeping the bottom of '{head}'");
            out.push(format!("{head} bottom"));
            bottom_present = true;
        } else if !hair_item && (has(&words, ONE_PIECE_WORDS) || has(&words, TOP_WORDS)) {
            log::info!("image prompt: undressed, dropping '{head}'");
        } else if nude
            && has(&words, LEGWEAR_WORDS)
            && !words.iter().any(|w| state_words.contains(w))
        {
            log::info!("image prompt: nude, dropping '{head}'");
        } else {
            out.push(tag.clone());
        }
    }
    out.join(", ")
}

/// Topless in a bikini: when the prompt has no bottom half left, add the
/// state's bikini as "<its words> bikini bottom".
fn ensure_bikini_bottom(positive: &str, worn: &str) -> String {
    let tags = split_tags_outside_parens(positive);
    if tags.iter().any(|t| {
        norm_words(t)
            .iter()
            .any(|w| BOTTOM_WORDS.contains(&w.as_str()))
    }) {
        return positive.to_string();
    }
    let bikini = split_tags_outside_parens(worn).into_iter().find_map(|seg| {
        let words = norm_words(&seg);
        let end = words.iter().position(|w| w == "bikini")?;
        Some(words[..=end].join(" "))
    });
    match bikini {
        Some(b) => {
            log::info!("image prompt: topless, adding '{b} bottom'");
            format!("{positive}, {b} bottom")
        }
        None => positive.to_string(),
    }
}

/// The builder sometimes drops her clothes or swaps in another outfit from
/// the card. Add a nude/topless tag the clothing state calls for, and when
/// most garments are missing, append them. Garments are matched by words, so
/// escaped parentheses and "blue and white" vs "blue, white" still count.
fn ensure_outfit(positive: &str, raw_state: &str) -> String {
    let clothing = crate::chat_prompts::parse_state_fields(raw_state)
        .into_iter()
        .filter(|f| {
            let name = f.name.to_lowercase();
            name.contains("outfit") || name.contains("cloth")
        })
        .map(|f| f.value.to_lowercase())
        .collect::<Vec<_>>()
        .join(", ");
    if clothing.trim().is_empty() {
        return positive.to_string();
    }
    let worn = worn_clothing(&clothing);
    let mut positive = positive.to_string();
    if let Some((tag, _)) = CLOTHING_STATE_TAGS
        .iter()
        .find(|(_, phrases)| phrases.iter().any(|p| clothing.contains(p)))
    {
        if !positive.to_lowercase().contains(tag) {
            log::info!("image prompt: clothing state says {tag}, appending it");
            positive = format!("{positive}, {tag}");
        }
        positive = strip_removed_garments(&positive, tag == &"nude", &worn);
        if *tag == "topless" {
            positive = ensure_bikini_bottom(&positive, &worn);
        }
    }
    let lower = positive.to_lowercase();
    if UNDRESS_WORDS.iter().any(|w| lower.contains(w)) {
        return clean_positive(&positive);
    }
    let garments: Vec<String> = worn
        .split([',', ';', '/', '.', '(', ')'])
        .flat_map(|t| t.split(" - "))
        .flat_map(|t| t.split(" and "))
        .flat_map(|t| t.split(" with "))
        .map(|t| t.trim().to_string())
        .filter(|t| {
            let words = norm_words(t);
            (1..=4).contains(&words.len())
                && t != "unknown"
                && !words
                    .iter()
                    .any(|w| CLOTHING_CHANGE_WORDS.contains(&w.as_str()))
        })
        .collect();
    let have: HashSet<String> = norm_words(&positive).into_iter().collect();
    let present = garments
        .iter()
        .filter(|g| {
            let words = norm_words(g);
            words.iter().filter(|w| have.contains(*w)).count() * 2 >= words.len()
        })
        .count();
    if garments.is_empty() || present * 2 >= garments.len() {
        return clean_positive(&positive);
    }
    log::info!("image prompt: outfit missing, appending the state's outfit");
    clean_positive(&format!("{positive}, {}", garments.join(", ")))
}

/// Lowercase alphanumeric words of `text`; backslashes, parentheses and other
/// punctuation only separate words.
fn norm_words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// The comma-separated tags of a card line that looks like a tag list (after a
/// `- ` bullet and a short `Label:` prefix): at least 3 tags of at most 6 words.
fn card_tag_line(line: &str) -> Option<Vec<&str>> {
    let mut rest = line.trim();
    rest = rest.strip_prefix("- ").unwrap_or(rest);
    if let Some((label, after)) = rest.split_once(':') {
        if label.split_whitespace().count() <= 8 {
            rest = after;
        }
    }
    let tags: Vec<&str> = rest
        .trim()
        .trim_end_matches('.')
        .split(',')
        .map(str::trim)
        .collect();
    let ok = tags.len() >= 3
        && tags
            .iter()
            .all(|t| !t.is_empty() && t.split_whitespace().count() <= 6);
    ok.then_some(tags)
}

/// Equal word sets, or one inside the other with the smaller at 2+ words.
fn tags_match(a: &HashSet<String>, b: &HashSet<String>) -> bool {
    a == b || (a.len() >= 2 && a.is_subset(b)) || (b.len() >= 2 && b.is_subset(a))
}

/// The builder sometimes copies another outfit's tags from the card (e.g. a
/// swimsuit while the state says t-shirt). Drop positive tags that match a
/// non-appearance card tag line unless `allowed_text` mentions all their words.
fn drop_stray_card_outfit(positive: &str, card: Option<&str>, allowed_text: &str) -> String {
    let Some(card) = card else {
        return positive.to_string();
    };
    let lines: Vec<Vec<HashSet<String>>> = card
        .lines()
        .filter_map(card_tag_line)
        .map(|tags| {
            tags.into_iter()
                .map(|t| norm_words(t).into_iter().collect())
                .collect()
        })
        .collect();
    let tags: Vec<&str> = positive.split(',').map(str::trim).collect();
    let Some(first) = tags
        .first()
        .map(|t| norm_words(t).into_iter().collect::<HashSet<_>>())
    else {
        return positive.to_string();
    };
    let Some(appearance) = lines.iter().find(|l| l.contains(&first)) else {
        return positive.to_string();
    };
    let outfit: Vec<&HashSet<String>> = lines
        .iter()
        .filter(|l| !std::ptr::eq(*l, appearance))
        .flatten()
        .collect();
    let allowed: HashSet<String> = norm_words(allowed_text).into_iter().collect();
    let kept: Vec<&str> = tags
        .iter()
        .copied()
        .filter(|tag| {
            let words: HashSet<String> = norm_words(tag).into_iter().collect();
            if words.is_empty() || appearance.contains(&words) || words.is_subset(&allowed) {
                return true;
            }
            !outfit.iter().any(|o| tags_match(&words, o))
        })
        .collect();
    let dropped = tags.len() - kept.len();
    if dropped == 0 {
        return positive.to_string();
    }
    log::info!("image prompt: dropped {dropped} stray outfit tag(s) from the card");
    kept.join(", ")
}

/// Garment words as stems (trailing "s" cut) in `text`, for comparing a state's
/// outfit with a prompt tag. "top", "bottom" and "tie" are left out: they show
/// up in non-garment tags ("top-down view", "hair tie").
fn garment_stems(text: &str) -> HashSet<String> {
    let stem = |w: &str| w.strip_suffix('s').unwrap_or(w).to_string();
    let lists: [&[&str]; 5] = [
        BOTTOM_WORDS,
        UNIFORM_WORDS,
        ONE_PIECE_WORDS,
        TOP_WORDS,
        LEGWEAR_WORDS,
    ];
    let known: HashSet<String> = lists
        .iter()
        .flat_map(|l| l.iter())
        .filter(|w| !matches!(**w, "top" | "bottom" | "tie"))
        .map(|w| stem(w))
        .collect();
    norm_words(text)
        .iter()
        .map(|w| stem(w))
        .filter(|w| known.contains(w))
        .collect()
}

/// `outfit` and `clothing_state` values of a raw state, lowercase, joined.
fn clothing_text(raw_state: &str) -> String {
    crate::chat_prompts::parse_state_fields(raw_state)
        .into_iter()
        .filter(|f| {
            let name = f.name.to_lowercase();
            name.contains("outfit") || name.contains("cloth")
        })
        .map(|f| f.value.to_lowercase())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Garments that must go missing between two states before the outfit counts
/// as changed (one lost accessory-like piece is not an undressing).
const OUTFIT_CHANGE_MIN_GARMENTS: usize = 2;

/// True when the outfit did not visibly change between two raw states: fewer
/// than `OUTFIT_CHANGE_MIN_GARMENTS` garment words lost, and no new
/// nude/topless clothing state. Activity is not compared: it is rewritten
/// every turn. False when either state has no outfit (nothing to compare).
pub fn outfit_unchanged(prev_raw: &str, cur_raw: &str) -> bool {
    let (prev, cur) = (clothing_text(prev_raw), clothing_text(cur_raw));
    if prev.trim().is_empty() || cur.trim().is_empty() {
        return false;
    }
    let lost = garment_stems(&prev)
        .difference(&garment_stems(&cur))
        .count();
    let nude_phrase = |text: &str| {
        CLOTHING_STATE_TAGS
            .iter()
            .any(|(_, phrases)| phrases.iter().any(|p| text.contains(p)))
    };
    lost < OUTFIT_CHANGE_MIN_GARMENTS && !(nude_phrase(&cur) && !nude_phrase(&prev))
}

/// Drops garment tags whose garment word is not in the state's outfit or
/// clothing state (a piece she took off, or one the builder recalled from the
/// card or an earlier photo). The first tag (the character), non-garment tags
/// and accessories stay. No-op when the state names no garment at all.
fn drop_stale_garments(positive: &str, raw_state: &str) -> String {
    let allowed = garment_stems(&worn_clothing(&clothing_text(raw_state)));
    if allowed.is_empty() {
        return positive.to_string();
    }
    let tags = split_tags_outside_parens(positive);
    let kept: Vec<&str> = tags
        .iter()
        .enumerate()
        .filter(|(i, tag)| {
            if *i == 0 || norm_words(tag).iter().any(|w| w == "hair") {
                return true;
            }
            let stale = garment_stems(tag).iter().any(|g| !allowed.contains(g));
            if stale {
                log::info!("image prompt: '{tag}' is not in her current outfit, dropping");
            }
            !stale
        })
        .map(|(_, t)| t.as_str())
        .collect();
    kept.join(", ")
}

/// A tag with this many words or more is a sentence, not a tag.
const TAG_SENTENCE_MIN_WORDS: usize = 6;

/// Words and phrases that make a tag a clause ("thighhigh left on the floor").
const CLAUSE_MARKERS: &[&str] = &[
    "left on", "while", "after", "because", "before", "when", "then",
];

/// Drops tags that read as sentences: six or more words, a clause marker, a
/// capitalised word (a name or title such as "Master" in "embracing Master"),
/// or a word of the persona's name. The first tag (the character), tags that
/// appear verbatim in the card and `(weight:1.2)` groups stay.
fn drop_sentence_tags(positive: &str, persona_name: Option<&str>, card: Option<&str>) -> String {
    let card = card.unwrap_or("").to_lowercase();
    let name_words: Vec<String> = persona_name
        .map(|n| {
            norm_words(n.split('(').next().unwrap_or(n))
                .into_iter()
                .filter(|w| w.len() >= 3)
                .collect()
        })
        .unwrap_or_default();
    let tags = split_tags_outside_parens(positive);
    let kept: Vec<&str> = tags
        .iter()
        .enumerate()
        .filter(|(i, tag)| {
            if *i == 0
                || card.contains(&tag.to_lowercase())
                || is_weight_group(tag.trim_matches(['(', ')']))
            {
                return true;
            }
            // Text inside parens ("\(fate\)") is a series name, not a title.
            let outside = tag.replace("\\(", "(");
            let outside = outside.split('(').next().unwrap_or("");
            let words = norm_words(tag);
            let padded = format!(" {} ", words.join(" "));
            let sentence = tag.split_whitespace().count() >= TAG_SENTENCE_MIN_WORDS
                || CLAUSE_MARKERS
                    .iter()
                    .any(|m| padded.contains(&format!(" {m} ")))
                || outside
                    .split_whitespace()
                    .any(|w| w.chars().next().is_some_and(char::is_uppercase))
                || words.iter().any(|w| name_words.contains(w));
            if sentence {
                log::info!("image prompt: dropping sentence-like tag '{tag}'");
            }
            !sentence
        })
        .map(|(_, t)| t.as_str())
        .collect();
    kept.join(", ")
}

/// The name after "Character:" in a card: up to the first ',', ". ", '(' or
/// " from ".
fn card_character_name(card: &str) -> Option<String> {
    let line = card.lines().find_map(|l| {
        let l = l.trim().trim_start_matches("- ").trim();
        l.strip_prefix("Character:")
    })?;
    let mut end = line.len();
    for sep in [",", ". ", "(", " from "] {
        if let Some(i) = line.find(sep) {
            end = end.min(i);
        }
    }
    let name = line[..end].trim().trim_end_matches('.').trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// An original character's name means nothing to the image model: a tag equal
/// to the persona's name, the card's "Character:" name or the first word of
/// either goes. Tags in the card's Appearance line, danbooru character tags
/// (with "\\(") and real danbooru tags stay.
fn drop_name_tags(
    positive: &str,
    persona_name: Option<&str>,
    card: Option<&str>,
    known: &dyn Fn(&str) -> bool,
) -> String {
    let mut names: Vec<String> = Vec::new();
    let mut add = |n: &str| {
        let n = n.trim().to_lowercase();
        if let Some(first) = n.split_whitespace().next() {
            names.push(first.to_string());
        }
        if !n.is_empty() {
            names.push(n);
        }
    };
    if let Some(n) = persona_name {
        add(n);
    }
    if let Some(n) = card.and_then(card_character_name) {
        add(&n);
    }
    if names.is_empty() {
        return positive.to_string();
    }
    let appearance: HashSet<String> = card
        .unwrap_or("")
        .lines()
        .filter(|l| l.to_lowercase().contains("appearance"))
        .filter_map(card_tag_line)
        .flatten()
        .map(str::to_lowercase)
        .collect();
    split_tags_outside_parens(positive)
        .into_iter()
        .filter(|tag| {
            let lower = tag.to_lowercase();
            let drop = names.contains(&lower)
                && !appearance.contains(&lower)
                && !tag.contains("\\(")
                && !known(tag);
            if drop {
                log::info!("image prompt: '{tag}' is a character name, dropping");
            }
            !drop
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Words that already place the picture somewhere.
const SCENE_WORDS: &[&str] = &[
    "bedroom",
    "bathroom",
    "kitchen",
    "classroom",
    "office",
    "room",
    "indoors",
    "outdoors",
    "beach",
    "street",
    "bed",
    "apartment",
    "park",
    "garden",
    "forest",
    "field",
    "city",
    "sky",
    "shower",
    "bath",
    "onsen",
    "pool",
    "poolside",
    "sofa",
    "couch",
    "house",
    "cafe",
    "shrine",
    "background",
    "scenery",
    "night",
    "ocean",
    "mountain",
    "balcony",
    "school",
    "schoolyard",
    "fountain",
];

/// (location words, scene tag, indoors?); first match wins.
const LOCATION_TAGS: &[(&[&str], &str, bool)] = &[
    // Before "school": school grounds are outside the classroom.
    (
        &["grounds", "schoolyard", "campus", "courtyard", "fountain"],
        "outdoors",
        false,
    ),
    (&["bedroom", "bed"], "bedroom", true),
    (&["bathroom", "shower", "bath", "onsen"], "bathroom", true),
    (&["kitchen"], "kitchen", true),
    (&["living", "lounge", "sofa", "couch"], "living room", true),
    (&["classroom", "school"], "classroom", true),
    (&["office"], "office", true),
    (
        &["beach", "shore", "seaside", "ocean", "sea"],
        "beach",
        false,
    ),
    (&["street", "road", "city", "alley"], "street", false),
    (
        &["park", "garden", "forest", "field", "meadow", "mountain"],
        "outdoors",
        false,
    ),
];

/// Place words that mean indoors when nothing else matched.
const INDOOR_WORDS: &[&str] = &[
    "apartment",
    "room",
    "house",
    "home",
    "inside",
    "hotel",
    "indoors",
    "flat",
    "dorm",
    "hall",
];

/// Adds a scene tag from the state's location when the prompt places the
/// picture nowhere. Tags: see `LOCATION_TAGS`; an unmatched indoor place
/// becomes "indoors"; anything else adds nothing.
fn ensure_location(positive: &str, raw_state: &str) -> String {
    let have: HashSet<String> = norm_words(positive).into_iter().collect();
    if SCENE_WORDS.iter().any(|w| have.contains(*w)) {
        return positive.to_string();
    }
    let location = crate::chat_prompts::parse_state_fields(raw_state)
        .into_iter()
        .find(|f| {
            let n = f.name.to_lowercase();
            n.contains("location") || n.contains("place") || n.contains("setting")
        })
        .map(|f| norm_words(&f.value))
        .unwrap_or_default();
    let has = |words: &[&str]| location.iter().any(|w| words.contains(&w.as_str()));
    let tags: Vec<&str> =
        if let Some((_, tag, indoors)) = LOCATION_TAGS.iter().find(|(words, _, _)| has(words)) {
            let side = if *indoors { "indoors" } else { "outdoors" };
            if *tag == side {
                vec![tag]
            } else {
                vec![tag, side]
            }
        } else if has(INDOOR_WORDS) {
            vec!["indoors"]
        } else {
            return positive.to_string();
        };
    log::info!("image prompt: no scene tag, adding {tags:?}");
    clean_positive(&format!("{positive}, {}", tags.join(", ")))
}

/// Framing words in the user's message and the tag they ask for, strongest first.
const FRAMING: &[(&[&str], &str)] = &[
    (
        &[
            "close",
            "closer",
            "closely",
            "close-up",
            "closeup",
            "close up",
            "in front of my face",
            "zoom",
            "zoomed",
            "zooming",
        ],
        "close-up",
    ),
    (&["full body", "whole body"], "full body"),
    (&["from behind", "your back"], "from behind"),
    (&["from above"], "from above"),
    (&["from below"], "from below"),
];

/// Append the framing tag the user's message asks for ("close", "full body",
/// "from behind", ...) when the positive lacks it. Only the first match counts.
fn ensure_framing(positive: &str, user_text: &str) -> String {
    let words: Vec<String> = user_text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    let padded = format!(" {} ", words.join(" "));
    let Some((_, tag)) = FRAMING
        .iter()
        .find(|(phrases, _)| phrases.iter().any(|p| padded.contains(&format!(" {p} "))))
    else {
        return positive.to_string();
    };
    let lower = positive.to_lowercase();
    let present = if *tag == "close-up" {
        lower.contains("close-up") || lower.contains("closeup")
    } else {
        lower.contains(tag)
    };
    if present {
        return positive.to_string();
    }
    log::info!("image prompt: adding framing tag {tag}");
    clean_positive(&format!("{positive}, {tag}"))
}

/// Words that mark a garment in a state's outfit as gone.
const GONE_WORDS: &[&str] = &["removed", "discarded", "tossed", "gone", "aside"];
/// Words a parenthesised note may hold when it only says the garment is gone.
const GONE_NOTE_WORDS: &[&str] = &[
    "removed",
    "discarded",
    "tossed",
    "gone",
    "aside",
    "taken",
    "off",
    "thrown",
    "and",
];

fn is_gone_phrase(words: &[String]) -> bool {
    words.iter().any(|w| GONE_WORDS.contains(&w.as_str()))
        || words.windows(2).any(|p| p[0] == "taken" && p[1] == "off")
        || words.last().is_some_and(|w| w == "off")
}

/// A state's clothing text without the garments it flags as gone: segments
/// like "white thighhighs removed" or "pink cape (discarded)", and the
/// parenthesised list after "completely naked (...)".
fn worn_clothing(text: &str) -> String {
    let flat = text.replace(';', ",").replace(" - ", ", ");
    let kept: Vec<String> = split_tags_outside_parens(&flat)
        .into_iter()
        .filter_map(|seg| {
            let Some((head, rest)) = seg.split_once('(') else {
                return (!is_gone_phrase(&norm_words(&seg))).then_some(seg);
            };
            if norm_words(head).iter().any(|w| w == "naked" || w == "nude") {
                return Some(head.trim().to_string());
            }
            let inner = norm_words(rest.rsplit_once(')').map_or(rest, |(i, _)| i));
            let gone_note = !inner.is_empty()
                && inner.iter().all(|w| GONE_NOTE_WORDS.contains(&w.as_str()))
                && is_gone_phrase(&inner);
            (!gone_note).then_some(seg)
        })
        .collect();
    kept.join(", ")
}

/// Drops prompt tags that say a garment is gone ("white thighhighs removed",
/// "pulling top off"). The first tag stays.
fn drop_removed_tags(positive: &str) -> String {
    let tags = split_tags_outside_parens(positive);
    let kept: Vec<&str> = tags
        .iter()
        .enumerate()
        .filter(|(i, tag)| {
            if *i == 0 {
                return true;
            }
            let words = norm_words(tag);
            let gone = words.iter().any(|w| w == "removed" || w == "discarded")
                || words.windows(2).any(|p| p[0] == "taken" && p[1] == "off")
                || words.last().is_some_and(|w| w == "off");
            if gone {
                log::info!("image prompt: dropping removed-garment tag '{tag}'");
            }
            !gone
        })
        .map(|(_, t)| t.as_str())
        .collect();
    kept.join(", ")
}

/// Words that make an activity a sex act.
const SEX_ACT_WORDS: &[&str] = &["riding", "intercourse", "sex"];
/// Words next to which "riding" is not a sex act.
const NON_SEX_RIDING: &[&str] = &[
    "horse",
    "bike",
    "bicycle",
    "motorcycle",
    "train",
    "wave",
    "waves",
    "bus",
];

/// Gear named "riding X": "holding the riding crop" is no act.
const RIDING_GEAR: &[&str] = &[
    "crop", "boots", "whip", "habit", "gloves", "jacket", "breeches", "pants", "helmet", "gear",
];

fn is_sex_act(text: &str) -> bool {
    let words = norm_words(text);
    words.iter().enumerate().any(|(i, w)| {
        SEX_ACT_WORDS.contains(&w.as_str())
            && !(w == "riding"
                && words
                    .get(i + 1)
                    .is_some_and(|n| RIDING_GEAR.contains(&n.as_str())))
    }) && !words.iter().any(|w| NON_SEX_RIDING.contains(&w.as_str()))
}

/// True for "naked", "nude" or "stripped bare" that is not negated.
fn says_naked(text: &str) -> bool {
    let words = norm_words(text);
    words.iter().enumerate().any(|(i, w)| {
        let hit = w == "naked"
            || w == "nude"
            || (w == "stripped" && words.get(i + 1).is_some_and(|n| n == "bare"));
        let negated = i > 0 && matches!(words[i - 1].as_str(), "not" | "never" | "no" | "t");
        // "his naked body", "Master's naked chest": someone else is naked.
        let someone_else = i > 0
            && (matches!(words[i - 1].as_str(), "his" | "your" | "their")
                || (words[i - 1] == "s" && i > 1 && USER_REFS.contains(&words[i - 2].as_str())));
        hit && !negated && !someone_else
    })
}

/// Her own narration: the `*...*` parts of a reply, or without any asterisks
/// the text outside double quotes.
fn narration(reply: &str) -> String {
    let stars: Vec<&str> = reply.split('*').collect();
    if stars.len() >= 3 {
        return stars
            .iter()
            .skip(1)
            .step_by(2)
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
    }
    reply.split('"').step_by(2).collect::<Vec<_>>().join(" ")
}

/// The state with its clothing set to "nude" when the state's activity or
/// clothing state is a sex act, or her latest reply narrates her naked; the
/// model often leaves the old garments in the outfit.
fn effective_state(raw_state: &str, reply: &str) -> String {
    let fields = crate::chat_prompts::parse_state_fields(raw_state);
    let sex_act = fields.iter().any(|f| {
        let name = f.name.to_lowercase();
        (name.contains("activity") || name.contains("cloth")) && is_sex_act(&f.value)
    });
    // `"outfit": "none"` means nothing on, not a garment called "none".
    let no_outfit = fields.iter().any(|f| {
        let words = norm_words(&f.value);
        f.name.to_lowercase().contains("outfit")
            && !words.is_empty()
            && words.iter().all(|w| {
                matches!(
                    w.as_str(),
                    "none" | "nothing" | "no" | "clothes" | "clothing" | "naked" | "nude"
                )
            })
    });
    if fields.is_empty() || !(sex_act || no_outfit || says_naked(&narration(reply))) {
        return raw_state.to_string();
    }
    log::info!("image prompt: state or reply says she is naked, treating the state as nude");
    let mut map = serde_json::Map::new();
    for f in fields {
        let name = f.name.to_lowercase();
        if !name.contains("outfit") && !name.contains("cloth") {
            map.insert(f.name, Value::String(f.value));
        }
    }
    map.insert("clothing_state".into(), Value::String("nude".into()));
    Value::Object(map).to_string()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Undress {
    Dressed,
    Topless,
    Nude,
}

fn undress_level(clothing: &str) -> Undress {
    match CLOTHING_STATE_TAGS
        .iter()
        .find(|(_, phrases)| phrases.iter().any(|p| clothing.contains(p)))
    {
        Some((tag, _)) if *tag == "nude" => Undress::Nude,
        Some(_) => Undress::Topless,
        None => Undress::Dressed,
    }
}

const NUDE_TAG_WORDS: &[&str] = &["nude", "naked", "topless", "bottomless"];
const NUDE_TAG_MODIFIERS: &[&str] = &["completely", "totally", "fully", "entirely"];

/// Drops nude/naked/topless/bottomless tags the state does not back (the
/// user's words, not her state); kept naked variants become "nude".
fn gate_nude_tags(positive: &str, level: Undress) -> String {
    let mut out: Vec<String> = Vec::new();
    for (i, tag) in split_tags_outside_parens(positive).into_iter().enumerate() {
        let words = norm_words(&tag);
        let nudeish = i > 0
            && words.iter().any(|w| NUDE_TAG_WORDS.contains(&w.as_str()))
            && words.iter().all(|w| {
                NUDE_TAG_WORDS.contains(&w.as_str()) || NUDE_TAG_MODIFIERS.contains(&w.as_str())
            });
        if !nudeish {
            out.push(tag);
            continue;
        }
        let topless = words.iter().any(|w| w == "topless");
        match level {
            Undress::Nude => out.push("nude".to_string()),
            Undress::Topless if topless => out.push("topless".to_string()),
            _ => log::info!("image prompt: state does not back '{tag}', dropping"),
        }
    }
    clean_tags(&out.join(", "))
}

/// A person's gender as far as photo tags care.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Gender {
    #[default]
    Female,
    Male,
}

impl Gender {
    /// "woman"/"female"/"girl" is female; anything else male (the setting's
    /// default is "man").
    fn from_setting(setting: &str) -> Self {
        match setting.trim().to_lowercase().as_str() {
            "woman" | "female" | "girl" | "lady" => Self::Female,
            _ => Self::Male,
        }
    }

    fn count_tag(self) -> &'static str {
        match self {
            Self::Female => "1girl",
            Self::Male => "1boy",
        }
    }

    fn plural_tag(self) -> &'static str {
        match self {
            Self::Female => "2girls",
            Self::Male => "2boys",
        }
    }

    fn on_top_tag(self) -> &'static str {
        match self {
            Self::Female => "girl on top",
            Self::Male => "boy on top",
        }
    }

    /// The pronoun set for model-facing text: (she, her).
    fn pronouns(self) -> (&'static str, &'static str) {
        match self {
            Self::Female => ("she", "her"),
            Self::Male => ("he", "his"),
        }
    }
}

/// The character's gender: the card's appearance tags hold "1girl" or "1boy".
/// Both or neither is female.
fn character_gender(card: Option<&str>) -> Gender {
    let lower = card.unwrap_or("").to_lowercase();
    let has = |tag: &str| {
        lower
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| w == tag)
    };
    match (has("1girl"), has("1boy")) {
        (false, true) => Gender::Male,
        _ => Gender::Female,
    }
}

/// The user's gender from a card line such as "User: woman", "User gender:
/// female" or "- The user is a man"; `None` without such a line.
fn card_user_gender(card: Option<&str>) -> Option<Gender> {
    const MALE: &[&str] = &["man", "male", "boy", "guy"];
    const FEMALE: &[&str] = &["woman", "female", "girl", "lady"];
    card.unwrap_or("").lines().find_map(|line| {
        let line = line
            .trim()
            .trim_start_matches(['-', '*'])
            .trim()
            .to_lowercase();
        let rest = line
            .strip_prefix("user")
            .or_else(|| line.strip_prefix("the user"))?;
        let words: Vec<&str> = rest
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        let male = words.iter().any(|w| MALE.contains(w));
        let female = words.iter().any(|w| FEMALE.contains(w));
        match (male, female) {
            (true, false) => Some(Gender::Male),
            (false, true) => Some(Gender::Female),
            _ => None,
        }
    })
}

/// The character and the user, which decide the people tags of a photo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pair {
    character: Gender,
    user: Gender,
}

impl Default for Pair {
    /// A female character and a male user.
    fn default() -> Self {
        Self {
            character: Gender::Female,
            user: Gender::Male,
        }
    }
}

impl Pair {
    /// A card line beats the setting for the user's gender.
    fn from_card(card: Option<&str>, user_setting: Gender) -> Self {
        Self {
            character: character_gender(card),
            user: card_user_gender(card).unwrap_or(user_setting),
        }
    }

    fn same_gender(self) -> bool {
        self.character == self.user
    }

    /// The tag for the user as a second person: their own count tag, or the
    /// plural when both are the same gender.
    fn partner_tag(self) -> &'static str {
        if self.same_gender() {
            self.user.plural_tag()
        } else {
            self.user.count_tag()
        }
    }

    /// Tags that go with a sex act: the pairing tag and the partner.
    fn sex_pair_tags(self) -> [&'static str; 2] {
        let pairing = match (self.character, self.user) {
            (Gender::Female, Gender::Female) => "yuri",
            (Gender::Male, Gender::Male) => "yaoi",
            _ => "hetero",
        };
        [pairing, self.partner_tag()]
    }

    /// Fills the placeholders of an `ACT_TAGS` entry.
    fn expand(self, tags: &[&str]) -> Vec<String> {
        tags.iter()
            .flat_map(|t| match *t {
                "@top" => vec![self.character.on_top_tag().to_string()],
                "@pair" => self.sex_pair_tags().iter().map(|t| t.to_string()).collect(),
                other => vec![other.to_string()],
            })
            .collect()
    }

    /// What a "man"/"woman" word in a prompt means: the partner when it
    /// matches the user's gender, else the character.
    fn person_word_tag(self, word: Gender) -> &'static str {
        if word == self.user {
            self.partner_tag()
        } else {
            word.count_tag()
        }
    }
}

/// Words after an object "her" ("tell her to", "give her a"); a possessive
/// "her" is followed by a noun or adjective.
const OBJECT_HER_NEXT: &[&str] = &[
    "a", "an", "the", "to", "and", "or", "up", "down", "in", "on", "at", "with", "from", "for",
    "back", "off", "out", "again", "now", "too", "that", "this",
];

/// Model-facing static text with "she/her" in the character's pronouns. A
/// male "her" is "his", or "him" before punctuation, the end or a word from
/// `OBJECT_HER_NEXT`.
fn gendered(text: &str, gender: Gender) -> String {
    if gender == Gender::Female {
        return text.to_string();
    }
    let (she, his) = gender.pronouns();
    let is_word = |c: char| c.is_alphabetic() || c == '\'';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(is_word) {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest.find(|c: char| !is_word(c)).unwrap_or(rest.len());
        let (word, after) = rest.split_at(end);
        let new = match word.to_lowercase().as_str() {
            "she" => she,
            "her" => {
                let next: String = after
                    .trim_start_matches(' ')
                    .chars()
                    .take_while(|c| is_word(*c))
                    .collect();
                if next.is_empty() || OBJECT_HER_NEXT.contains(&next.to_lowercase().as_str()) {
                    "him"
                } else {
                    his
                }
            }
            _ => "",
        };
        if new.is_empty() {
            out.push_str(word);
        } else if word.starts_with(char::is_uppercase) {
            let mut cs = new.chars();
            out.extend(cs.next().map(|c| c.to_ascii_uppercase()));
            out.push_str(cs.as_str());
        } else {
            out.push_str(new);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Words that name the user inside a tag.
const USER_REFS: &[&str] = &["master", "user", "you", "your"];
const NATIONALITY_WORDS: &[&str] = &["japanese", "american", "chinese", "british"];
const DANGLING_WORDS: &[&str] = &[
    "at", "on", "to", "with", "against", "for", "over", "from", "by", "up",
];
/// Phrases (matched at a word start) that name an act, and the tags for it.
/// "@top" and "@pair" expand per the character/user pair (`Pair::expand`).
const ACT_TAGS: &[(&[&str], &[&str])] = &[
    (&["riding"], &["@top", "sex", "@pair"]),
    (&["intercourse", "having sex"], &["sex", "@pair"]),
    (&["touching herself", "masturbat"], &["masturbation"]),
    (&["blush", "flush"], &["blush"]),
    (&["kissing"], &["kiss"]),
];
const MAN_TAGS: &[&str] = &["1 man", "1man", "a man", "man"];
const WOMAN_TAGS: &[&str] = &["1 woman", "1woman", "a woman", "woman"];
const LEADING_ADVERBS: &[&str] = &["highly", "very", "deeply", "intensely", "extremely"];
/// Feelings and sounds the builder writes as tags; not danbooru tags.
const ABSTRACT_NOUNS: &[&str] = &[
    "sigh",
    "gasp",
    "hesitation",
    "anticipation",
    "excitement",
    "eagerness",
    "desire",
    "arousal",
    "delight",
    "joy",
    "passion",
    "reluctance",
    "moment",
    "movement",
    "intent",
    "rhythm",
    "vibe",
    "atmosphere",
    "energy",
];
/// Places and furniture: "bed in apartment" is just "bed".
const PLACE_WORDS: &[&str] = &[
    "bed",
    "apartment",
    "room",
    "bedroom",
    "kitchen",
    "couch",
    "sofa",
    "floor",
    "house",
    "bathroom",
    "chamber",
    "hall",
    "office",
    "desk",
    "table",
    "chair",
    "the",
    "a",
];
/// Nouns that a split "A and B noun" shares ("loose and flowing hair").
const SHARED_NOUNS: &[&str] = &[
    "hair",
    "eyes",
    "smile",
    "expression",
    "look",
    "grin",
    "smirk",
    "tail",
    "ears",
    "skin",
    "lips",
];
/// Tags that need a second person in the picture.
const TWO_PERSON_TAGS: &[&str] = &[
    "sex",
    "girl on top",
    "boy on top",
    "kiss",
    "hug",
    "hetero",
    "2girls",
    "2boys",
    "yuri",
    "yaoi",
];
/// A body part followed by a verb makes a clause ("cheeks flushing").
const SUBJECT_NOUNS: &[&str] = &[
    "cheeks", "eyes", "tail", "body", "face", "hands", "lips", "chest", "breasts", "skin", "ears",
];
const PROSE_PRONOUNS: &[&str] = &["she", "he", "they", "is", "are", "was"];
/// A tag only counts as a known danbooru tag with this many posts.
const KNOWN_TAG_MIN_COUNT: u64 = 100;

/// Turns one tag into zero or more clean tags; `partner` is set when a user
/// reference was replaced and the tag kept.
fn refine_tag(
    tag: &str,
    name_words: &[String],
    known: &dyn Fn(&str) -> bool,
    pair: Pair,
    partner: &mut bool,
) -> Vec<String> {
    let lower = tag.trim().to_lowercase();
    let words = norm_words(&lower);
    if words.is_empty() || words.iter().all(|w| name_words.contains(w)) {
        return Vec::new();
    }
    if MAN_TAGS.contains(&lower.as_str()) {
        return vec![pair.person_word_tag(Gender::Male).to_string()];
    }
    if WOMAN_TAGS.contains(&lower.as_str()) {
        return vec![pair.person_word_tag(Gender::Female).to_string()];
    }
    let padded = format!(" {} ", words.join(" "));
    if let Some((_, tags)) = ACT_TAGS
        .iter()
        .find(|(phrases, _)| phrases.iter().any(|p| padded.contains(&format!(" {p}"))))
    {
        return pair.expand(tags);
    }
    let raw: Vec<&str> = tag.split_whitespace().collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut had_user = false;
    for (i, w) in raw.iter().enumerate() {
        let lower = w.to_lowercase().replace('’', "'");
        let bare = lower.trim_end_matches("'s");
        let next_is_user = raw.get(i + 1).is_some_and(|n| {
            USER_REFS.contains(&n.to_lowercase().replace('’', "'").trim_end_matches("'s"))
        });
        if USER_REFS.contains(&bare) {
            // "Master's apartment" names his place, not him in the picture.
            let possessive = bare.len() != lower.len() || bare == "your";
            had_user |= !possessive;
        } else if !(bare == "the" && next_is_user)
            // "american flag bikini" names a print, not a nationality.
            && !(NATIONALITY_WORDS.contains(&bare)
                && !raw.get(i + 1).is_some_and(|n| n.eq_ignore_ascii_case("flag")))
        {
            kept.push(w);
        }
    }
    while kept
        .first()
        .is_some_and(|w| LEADING_ADVERBS.contains(&w.to_lowercase().as_str()))
    {
        kept.remove(0);
    }
    if kept.len() > 1
        && kept
            .last()
            .is_some_and(|w| w.eq_ignore_ascii_case("setting"))
    {
        kept.pop();
    }
    let mut rest = kept.join(" ");
    if let Some((first, others)) = rest.split_once(" in ") {
        let places = |t: &str| {
            let w = norm_words(t);
            !w.is_empty() && w.iter().all(|x| PLACE_WORDS.contains(&x.as_str()))
        };
        if places(first) && places(others) {
            rest = first.to_string();
        }
    }
    let rest_words = norm_words(&rest);
    let Some(last) = rest_words.last() else {
        return Vec::new();
    };
    if had_user
        && ((rest_words.len() == 1 && last.ends_with("ing"))
            || DANGLING_WORDS.contains(&last.as_str()))
    {
        return Vec::new();
    }
    if known(&rest) {
        *partner |= had_user;
        return vec![rest];
    }
    let prose = rest_words.iter().any(|w| w == "against")
        || (last.len() > 3 && last.ends_with("ly"))
        || ABSTRACT_NOUNS.contains(&last.as_str())
        || rest_words
            .iter()
            .any(|w| PROSE_PRONOUNS.contains(&w.as_str()))
        || (rest_words.len() >= 3
            && SUBJECT_NOUNS.contains(&rest_words[0].as_str())
            && rest_words[1..].iter().any(|w| w.ends_with("ing")));
    if prose {
        return Vec::new();
    }
    let parts: Vec<&str> = if rest_words.len() >= 3 && rest.contains(" with ") {
        rest.split(" with ").collect()
    } else if rest_words.iter().any(|w| w == "and")
        && !rest_words
            .iter()
            .any(|w| COLOUR_WORDS.contains(&w.as_str()))
    {
        let mut parts: Vec<String> = rest.split(" and ").map(str::to_string).collect();
        if parts.iter().any(|p| p.split_whitespace().count() > 2) {
            return Vec::new();
        }
        // "loose and flowing hair": the first part shares the final noun.
        if let [first, last_part] = parts.as_mut_slice() {
            let noun = last_part.split_whitespace().last().unwrap_or("");
            if first.split_whitespace().count() == 1
                && last_part.split_whitespace().count() > 1
                && SHARED_NOUNS.contains(&noun.to_lowercase().as_str())
            {
                *first = format!("{first} {noun}");
            }
        }
        return parts
            .iter()
            .flat_map(|part| refine_tag(part, name_words, known, pair, partner))
            .collect();
    } else {
        *partner |= had_user;
        return vec![rest];
    };
    parts
        .into_iter()
        .flat_map(|part| refine_tag(part, name_words, known, pair, partner))
        .collect()
}

/// Prose tags become danbooru-style tags: user references go ("riding
/// Master" becomes the riding act), the persona's own name and clause-like
/// tags are dropped, acts map to their tags, "A and B" / "A with B" split,
/// and "man"/"woman" variants become people tags. The first tag, card tags and tags with
/// parentheses stay. `known` spares tags the tag data knows.
#[cfg(test)]
fn refine_prose_tags(
    positive: &str,
    persona_name: Option<&str>,
    card: Option<&str>,
    known: &dyn Fn(&str) -> bool,
) -> String {
    refine_prose_tags_flagged(positive, persona_name, card, known, Pair::default()).0
}

/// `refine_prose_tags` plus whether a user reference became the partner tag.
fn refine_prose_tags_flagged(
    positive: &str,
    persona_name: Option<&str>,
    card: Option<&str>,
    known: &dyn Fn(&str) -> bool,
    pair: Pair,
) -> (String, bool) {
    let card = card.unwrap_or("").to_lowercase();
    let name_words: Vec<String> = persona_name
        .map(|n| {
            norm_words(n.split('(').next().unwrap_or(n))
                .into_iter()
                .filter(|w| w.len() >= 3)
                .collect()
        })
        .unwrap_or_default();
    let mut partner = false;
    let mut out: Vec<String> = Vec::new();
    for (i, tag) in split_tags_outside_parens(positive).into_iter().enumerate() {
        let standalone_name = i > 0
            && !name_words.is_empty()
            && norm_words(&tag).iter().all(|w| name_words.contains(w));
        let exempt =
            i == 0 || tag.contains('(') || (card.contains(&tag.to_lowercase()) && !standalone_name);
        if exempt {
            out.push(tag);
            continue;
        }
        let refined = refine_tag(&tag, &name_words, known, pair, &mut partner);
        if refined.len() != 1 || refined[0] != tag {
            log::info!("image prompt: tag '{tag}' became {refined:?}");
        }
        out.extend(refined);
    }
    if partner {
        out.push(pair.partner_tag().to_string());
    }
    (clean_tags(&out.join(", ")), partner)
}

/// True when the state's activity names the user (not possessively) next to
/// an action.
fn activity_mentions_user(raw_state: &str) -> bool {
    crate::chat_prompts::parse_state_fields(raw_state)
        .into_iter()
        .filter(|f| f.name.to_lowercase().contains("activity"))
        .any(|f| {
            let words: Vec<String> = f
                .value
                .split_whitespace()
                .map(|w| {
                    w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'')
                        .to_lowercase()
                })
                .collect();
            words.len() >= 2
                && words
                    .iter()
                    .any(|w| matches!(w.as_str(), "master" | "user" | "you" | "him" | "his"))
        })
}

/// A solo photo keeps no partner tag (the user's "1boy"/"1girl", or the
/// plural "2girls"/"2boys"): it stays only with a two-person tag, a converted
/// user reference (`converted`) or a user-naming activity. The character's own
/// count tag is never dropped.
fn gate_partner_tag(positive: &str, converted: bool, raw_state: &str, pair: Pair) -> String {
    let tags = split_tags_outside_parens(positive);
    let lower: Vec<String> = tags.iter().map(|t| t.to_lowercase()).collect();
    let two_person = lower.iter().any(|t| TWO_PERSON_TAGS.contains(&t.as_str()));
    if converted || two_person || activity_mentions_user(raw_state) {
        return positive.to_string();
    }
    let partner = pair.partner_tag();
    if lower.iter().any(|t| t == partner) {
        log::info!("image prompt: solo photo, dropping '{partner}'");
    }
    tags.iter()
        .zip(&lower)
        .filter(|(_, l)| *l != partner)
        .map(|(t, _)| t.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Two people of one gender carry the plural count tag instead of two
/// singular ones ("2girls" replaces "1girl").
fn merge_same_gender_counts(positive: &str, pair: Pair) -> String {
    if !pair.same_gender() {
        return positive.to_string();
    }
    let tags = split_tags_outside_parens(positive);
    if !tags
        .iter()
        .any(|t| t.eq_ignore_ascii_case(pair.character.plural_tag()))
    {
        return positive.to_string();
    }
    tags.iter()
        .filter(|t| !t.eq_ignore_ascii_case(pair.character.count_tag()))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A topless photo is framed on the upper body: the builder's "lower body"
/// framing tags go.
fn drop_lower_body_framing(positive: &str, level: Undress) -> String {
    if level != Undress::Topless {
        return positive.to_string();
    }
    split_tags_outside_parens(positive)
        .into_iter()
        .filter(|t| !matches!(t.to_lowercase().as_str(), "lower body" | "lower body view"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Content of the newest message with `role`, or "".
fn last_content<'a>(history: &'a [ChatMessage], role: &str) -> &'a str {
    history
        .iter()
        .rev()
        .find(|m| m.role == role)
        .map_or("", |m| m.content.as_str())
}

/// Backstops applied to the builder's positive prompt, without tag data.
#[cfg(test)]
fn apply_backstops(positive: &str, input: &BuilderInput<'_>) -> String {
    apply_backstops_with(positive, input, &|_| false, Gender::Male)
}

/// Drops tags that echo a state field instead of describing the picture:
/// "loosened clothing state", or the clothing state's own value ("loosened").
/// A value that is itself a nude/topless word stays (`gate_nude_tags` owns it).
fn drop_state_echo(positive: &str, raw_state: &str) -> String {
    let clothing_state: Option<String> = crate::chat_prompts::parse_state_fields(raw_state)
        .into_iter()
        .find(|f| f.name.to_lowercase().contains("clothing_state"))
        .map(|f| f.value.trim().to_lowercase())
        .filter(|v| !v.is_empty() && !NUDE_TAG_WORDS.contains(&v.as_str()));
    let tags = split_tags_outside_parens(positive);
    let kept: Vec<&str> = tags
        .iter()
        .enumerate()
        .filter(|(i, tag)| {
            let lower = tag.to_lowercase();
            let echo = *i > 0
                && (lower.contains("clothing state") || clothing_state.as_deref() == Some(&lower));
            if echo {
                log::info!("image prompt: '{tag}' echoes the state, dropping");
            }
            !echo
        })
        .map(|(_, t)| t.as_str())
        .collect();
    kept.join(", ")
}

/// When her state's activity is a sex act and the prompt shows no second
/// person, append the act's tags (the builder often writes only "straddling").
fn ensure_act_tags(positive: &str, raw_state: &str, pair: Pair) -> String {
    let Some(activity) = crate::chat_prompts::parse_state_fields(raw_state)
        .into_iter()
        .find(|f| f.name.to_lowercase().contains("activity") && is_sex_act(&f.value))
        .map(|f| f.value.to_lowercase())
    else {
        return positive.to_string();
    };
    let tags = split_tags_outside_parens(positive);
    if tags
        .iter()
        .any(|t| TWO_PERSON_TAGS.contains(&t.to_lowercase().as_str()))
    {
        return positive.to_string();
    }
    let act: &[&str] = if activity.contains("riding") {
        &["@top", "sex", "@pair"]
    } else {
        &["sex", "@pair"]
    };
    let add = pair.expand(act).join(", ");
    log::info!("image prompt: activity is a sex act, adding '{add}'");
    clean_tags(&format!(
        "{positive}, {add}, {}",
        pair.character.count_tag()
    ))
}

/// Phrases in the user's message that ask for a photo of both of them.
const US_PHRASES: &[&str] = &[
    "of us",
    "both of us",
    "the two of us",
    "us together",
    "with me in it",
    "you and me",
    "selfie with me",
    "together",
];

/// A photo "of us" needs the partner in the prompt even without a sex act:
/// append the partner's count tag (and the character's own) when the user
/// asks for both of them and no two-person tag is there. No act tags.
fn ensure_us_pair(positive: &str, user_text: &str, pair: Pair) -> String {
    let words: Vec<String> = norm_words(user_text);
    let padded = format!(" {} ", words.join(" "));
    if !US_PHRASES
        .iter()
        .any(|p| padded.contains(&format!(" {p} ")))
    {
        return positive.to_string();
    }
    let tags = split_tags_outside_parens(positive);
    if tags
        .iter()
        .any(|t| TWO_PERSON_TAGS.contains(&t.to_lowercase().as_str()))
    {
        return positive.to_string();
    }
    log::info!("image prompt: photo of both of them, adding the partner");
    clean_tags(&format!(
        "{positive}, {}, {}",
        pair.partner_tag(),
        pair.character.count_tag()
    ))
}

/// The ruleset's "always use in negative" tags: the text after the first ':'
/// following the word "negative" (embedding names hold their own ':').
fn required_negative(style_rules: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in style_rules.lines() {
        // ASCII lowercase keeps byte offsets valid for slicing `line`.
        let lower = line.to_ascii_lowercase();
        if !lower.contains("always") {
            continue;
        }
        let Some(at) = lower.find("negative").map(|i| i + "negative".len()) else {
            continue;
        };
        let Some(colon) = line[at..].find(':') else {
            continue;
        };
        for tag in line[at + colon + 1..].split(',') {
            let is_junk = |c: char| c.is_whitespace() || c == '`';
            let tag = tag
                .trim_matches(is_junk)
                .trim_end_matches('.')
                .trim_matches(is_junk);
            if !tag.is_empty() && !out.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                out.push(tag.to_string());
            }
        }
    }
    out
}

/// The required negative tags first, then the model's extras; the builder
/// model sometimes drops items of the ruleset's list.
fn merge_required_negative(negative: Option<String>, style_rules: &str) -> Option<String> {
    let required = required_negative(style_rules);
    if required.is_empty() {
        return negative;
    }
    let model = negative
        .as_deref()
        .map(split_tags_outside_parens)
        .unwrap_or_default();
    let has = |list: &[String], t: &str| list.iter().any(|x| x.eq_ignore_ascii_case(t));
    let missing: Vec<&str> = required
        .iter()
        .filter(|t| !has(&model, t))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        log::info!("image prompt: adding required negative tags {missing:?}");
    }
    let mut out = required;
    for t in model {
        if !has(&out, &t) {
            out.push(t);
        }
    }
    Some(out.join(", "))
}

/// Single words that are never a tag on their own.
const FUNCTION_WORDS: &[&str] = &[
    "wearing", "with", "and", "in", "on", "at", "the", "a", "an", "her", "his", "of", "while",
    "holding",
];
/// Words of mood prose the builder invents ("playful shyness"); a tag with one
/// is dropped when the tag data doesn't know it.
const MOOD_WORDS: &[&str] = &[
    "playful",
    "playfully",
    "mischievous",
    "teasing",
    "boldly",
    "shyness",
    "giggle",
    "giggling",
    "shifting",
    "eagerness",
    "excitement",
    "glint",
    "anticipation",
    "eager",
];
/// Time-of-day words that make "afternoon sun" a plain "sunlight".
const TIME_WORDS: &[&str] = &[
    "morning",
    "afternoon",
    "evening",
    "noon",
    "midday",
    "early",
    "late",
];
/// "<adj> expression" tags with an obvious real tag.
const EXPRESSION_TAGS: &[(&str, &str)] = &[("excited", "excited"), ("eager", "smile")];

/// Vague prose tags become real tags or go: a lone function word, "<adj> glint
/// in eyes" (to "sparkling eyes"), "<time> sun" (to "sunlight"), "<adj>
/// expression" (a small table) and mood phrases the tag data doesn't know.
/// The data-dependent rules only run when the tag data answers at all
/// (`known("smile")`), so a missing database drops nothing. The first tag, card
/// tags, outfit words and tags with parentheses stay.
fn refine_vague_tags(
    positive: &str,
    card: Option<&str>,
    outfit: &str,
    known: &dyn Fn(&str) -> bool,
) -> String {
    let card = card.unwrap_or("").to_lowercase();
    let outfit = outfit.to_lowercase();
    let has_data = known("smile");
    let mut out: Vec<String> = Vec::new();
    for (i, tag) in split_tags_outside_parens(positive).into_iter().enumerate() {
        let lower = tag.trim().to_lowercase();
        // Before the card check: card prose contains "wearing", "with", ...
        if FUNCTION_WORDS.contains(&lower.as_str()) {
            log::info!("image prompt: vague tag '{tag}' dropped");
            continue;
        }
        if i == 0
            || tag.contains('(')
            || (!lower.is_empty() && (card.contains(&lower) || outfit.contains(&lower)))
        {
            out.push(tag);
            continue;
        }
        let words = norm_words(&lower);
        // None keeps the tag, Some(None) drops it, Some(Some(t)) replaces it.
        let fix: Option<Option<&str>> = if known(&lower) {
            None
        } else if words.len() >= 2
            && TIME_WORDS.contains(&words[0].as_str())
            && matches!(words[words.len() - 1].as_str(), "sun" | "light")
        {
            Some(Some("sunlight"))
        } else if words.iter().any(|w| w == "glint") && words.iter().any(|w| w == "eyes") {
            if !has_data {
                None
            } else if known("sparkling eyes") {
                Some(Some("sparkling eyes"))
            } else {
                Some(None)
            }
        } else if let Some((_, to)) = EXPRESSION_TAGS
            .iter()
            .find(|(adj, _)| words.len() == 2 && words[0] == *adj && words[1] == "expression")
        {
            Some(Some(*to))
        } else if has_data && words.iter().any(|w| MOOD_WORDS.contains(&w.as_str())) {
            Some(None)
        } else {
            None
        };
        match fix {
            None => out.push(tag),
            Some(None) => log::info!("image prompt: vague tag '{tag}' dropped"),
            Some(Some(to)) => {
                log::info!("image prompt: vague tag '{tag}' became '{to}'");
                out.push(to.to_string());
            }
        }
    }
    clean_tags(&out.join(", "))
}

/// A selfie faces the camera, so it can't be "from behind": when the user
/// asked for the back view the selfie tags go, otherwise "from behind" does.
fn resolve_selfie_view(positive: &str, user_text: &str) -> String {
    let tags = split_tags_outside_parens(positive);
    let is_back = |t: &String| t.eq_ignore_ascii_case("from behind");
    let is_selfie = |t: &String| t.to_lowercase().contains("selfie");
    if !tags.iter().any(is_back) || !tags.iter().any(is_selfie) {
        return positive.to_string();
    }
    let user = user_text.to_lowercase();
    let asked_back = user.contains("from behind") || user.contains("your back");
    log::info!(
        "image prompt: selfie and 'from behind' clash, dropping the {}",
        if asked_back {
            "selfie tags"
        } else {
            "back view"
        }
    );
    tags.iter()
        .filter(|t| {
            if asked_back {
                !is_selfie(t)
            } else {
                !is_back(t)
            }
        })
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Backstops applied to the builder's positive prompt; `known` says whether a
/// phrase is a real danbooru tag; `user_setting` is the configured gender of
/// the user (a card line overrides it).
fn apply_backstops_with(
    positive: &str,
    input: &BuilderInput<'_>,
    known: &dyn Fn(&str) -> bool,
    user_setting: Gender,
) -> String {
    let pair = Pair::from_card(input.persona, user_setting);
    let reply = last_content(input.history, "assistant");
    let state = effective_state(input.raw_state, reply);
    let outfit = crate::chat_prompts::parse_state_fields(&state)
        .into_iter()
        .find(|f| f.name.eq_ignore_ascii_case("outfit"))
        .map(|f| worn_clothing(&f.value.to_lowercase()))
        .unwrap_or_default();
    let user = last_content(input.history, "user");
    // Only her reply and state allow outfit tags; the user's request must not
    // dress or undress her before she (or the state) has.
    let allowed = format!("{outfit} {reply}");
    let positive = drop_stray_card_outfit(positive, input.persona, &allowed);
    let positive = drop_removed_tags(&positive);
    let (positive, converted) =
        refine_prose_tags_flagged(&positive, input.persona_name, input.persona, known, pair);
    let positive = refine_vague_tags(&positive, input.persona, &outfit, known);
    let positive = gate_partner_tag(&positive, converted, &state, pair);
    let positive = drop_state_echo(&positive, &state);
    let positive = ensure_act_tags(&positive, &state, pair);
    let asked = format!("{user} {}", input.hint.unwrap_or(""));
    let positive = ensure_us_pair(&positive, &asked, pair);
    let positive = merge_same_gender_counts(&positive, pair);
    let positive = drop_name_tags(&positive, input.persona_name, input.persona, known);
    let positive = drop_sentence_tags(&positive, input.persona_name, input.persona);
    let level = undress_level(&clothing_text(&state));
    let positive = gate_nude_tags(&positive, level);
    let positive = drop_lower_body_framing(&positive, level);
    let positive = drop_stale_garments(&positive, &state);
    let positive = ensure_outfit(&positive, &state);
    let positive = ensure_location(&positive, &state);
    let positive = ensure_framing(&positive, &asked);
    let positive = resolve_selfie_view(&positive, &asked);
    drop_offframe_garments(&positive)
}

/// Footwear tags; a close frame can't show them.
const OFFFRAME_FOOTWEAR: &[&str] = &[
    "loafers",
    "shoes",
    "shoe",
    "boots",
    "boot",
    "sneakers",
    "heels",
    "sandals",
    "slippers",
    "footwear",
    "barefoot",
    "socks",
    "kneehighs",
];
/// Leg-wear tags; only a face/chest frame can't show them.
const OFFFRAME_LEGWEAR: &[&str] = &["thighhighs", "stockings", "pantyhose", "legwear"];

/// A close-up (or portrait / upper body / cowboy shot) photo drops garments
/// below its frame: the image model otherwise squeezes them in, e.g. loafers
/// on her chest. Kept when the picture is about feet or legs.
fn drop_offframe_garments(positive: &str) -> String {
    let tags = split_tags_outside_parens(positive);
    let lower: Vec<String> = tags.iter().map(|t| t.to_lowercase()).collect();
    // Substring: the builder writes "close-up selfie", "upper body shot".
    let has = |names: &[&str]| lower.iter().any(|t| names.iter().any(|n| t.contains(n)));
    let tight = has(&[
        "close-up",
        "closeup",
        "portrait",
        "upper body",
        "face focus",
        "breast focus",
    ]);
    let waist_up = tight || has(&["cowboy shot"]);
    if !waist_up
        || lower
            .iter()
            .any(|t| t.contains("feet") || t.contains("foot") || t.contains("legs"))
    {
        return positive.to_string();
    }
    let is_word = |tag: &str, words: &[&str]| tag.split_whitespace().any(|w| words.contains(&w));
    let kept: Vec<&str> = tags
        .iter()
        .zip(&lower)
        .filter(|(_, l)| {
            let drop = is_word(l, OFFFRAME_FOOTWEAR) || (tight && is_word(l, OFFFRAME_LEGWEAR));
            if drop {
                log::info!("image prompt: '{l}' is out of frame, dropping");
            }
            !drop
        })
        .map(|(t, _)| t.as_str())
        .collect();
    kept.join(", ")
}

/// One schema-constrained call; any failure or an empty `positive` falls back
/// to `fallback_fields`.
pub async fn build_image_prompt(cfg: &AppConfig, input: &BuilderInput<'_>) -> ImagePromptFields {
    let gender = character_gender(input.persona);
    let messages = vec![
        ChatMessage::text(
            "system",
            gendered(&builder_system_content(input.style_rules), gender),
        ),
        ChatMessage::text(
            "user",
            builder_user_content(
                gender,
                input.persona,
                input.raw_state,
                input.history,
                input.photo_reason,
                input.hint,
                input.previous_prompt,
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
    let tag_data = crate::danbooru::TagData::open(cfg);
    let user_setting = Gender::from_setting(&cfg.chat_user_gender);
    let known = |tag: &str| {
        tag_data.as_ref().is_some_and(|data| {
            data.resolve(tag).is_some_and(|(info, _)| {
                info.category == crate::danbooru::Category::General
                    && info.count >= KNOWN_TAG_MIN_COUNT
            })
        })
    };
    let mut fields = match built {
        Ok(v) => fields_from_value(&v)
            .map(|mut f| {
                f.positive = f
                    .positive
                    .map(|p| apply_backstops_with(&p, input, &known, user_setting));
                f
            })
            .unwrap_or_else(|| {
                log::warn!("image prompt builder returned no positive prompt, using the fallback");
                fallback_fields(input.persona_name, input.raw_state, input.hint)
            }),
        Err(e) => {
            log::warn!("image prompt builder failed, using the fallback: {e}");
            fallback_fields(input.persona_name, input.raw_state, input.hint)
        }
    };
    fields.negative = merge_required_negative(fields.negative.take(), input.style_rules);
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_negative_keeps_embedding_colons() {
        let rules = "- Use `positive` for tags\n- Always use in `negative`: embedding:lazyhand_1, embedding:lazywet, no pink smoke, strange pose.\n- never mind";
        assert_eq!(
            required_negative(rules),
            [
                "embedding:lazyhand_1",
                "embedding:lazywet",
                "no pink smoke",
                "strange pose"
            ]
        );
        assert!(required_negative("nothing here").is_empty());
    }

    #[test]
    fn merge_required_negative_orders_and_dedupes() {
        let rules = "Always use in negative: embedding:a, strange pose";
        let out = merge_required_negative(Some("blurry, Strange Pose, bad hands".into()), rules);
        assert_eq!(
            out.as_deref(),
            Some("embedding:a, strange pose, blurry, bad hands")
        );
        let out = merge_required_negative(None, rules);
        assert_eq!(out.as_deref(), Some("embedding:a, strange pose"));
    }

    #[test]
    fn merge_required_negative_without_rules_is_unchanged() {
        assert_eq!(
            merge_required_negative(Some("blurry,  bad".into()), "no rules"),
            Some("blurry,  bad".to_string())
        );
        assert_eq!(merge_required_negative(None, "no rules"), None);
    }

    fn data_known(tag: &str) -> bool {
        matches!(
            tag,
            "smile"
                | "school uniform"
                | "close-up"
                | "white socks"
                | "sparkling eyes"
                | "sunlight"
                | "1girl"
        )
    }

    #[test]
    fn vague_tags_become_real_or_go() {
        let pos = "1girl, mischievous glint in eyes, afternoon sun, surprised giggle, \
                   playful shyness, shifting weight, boldly teasing, eager expression, \
                   excited expression, wearing, school uniform, close-up, \
                   asuna \\(blue archive\\), white socks";
        let out = refine_vague_tags(pos, None, "", &data_known);
        assert_eq!(
            out,
            "1girl, sparkling eyes, sunlight, smile, excited, school uniform, close-up, \
             asuna \\(blue archive\\), white socks"
        );
    }

    #[test]
    fn vague_tags_survive_without_tag_data() {
        let pos = "1girl, playful shyness, mischievous glint in eyes, wearing, afternoon sun";
        let out = refine_vague_tags(pos, None, "", &|_| false);
        assert_eq!(
            out,
            "1girl, playful shyness, mischievous glint in eyes, sunlight"
        );
    }

    #[test]
    fn vague_tags_spare_card_and_outfit() {
        let out = refine_vague_tags(
            "1girl, playful shyness, teasing smile",
            Some("playful shyness"),
            "teasing smile",
            &data_known,
        );
        assert_eq!(out, "1girl, playful shyness, teasing smile");
    }

    #[test]
    fn selfie_drops_back_view_unless_asked() {
        let pos = "1girl, from behind, walking, phone held out taking selfie";
        assert_eq!(
            resolve_selfie_view(pos, "send me a selfie"),
            "1girl, walking, phone held out taking selfie"
        );
        assert_eq!(
            resolve_selfie_view(pos, "show me from behind"),
            "1girl, from behind, walking"
        );
        assert_eq!(
            resolve_selfie_view("1girl, from behind", "x"),
            "1girl, from behind"
        );
    }

    #[test]
    fn ensure_outfit_appends_only_when_missing() {
        let state =
            r#"{"outfit": "white t-shirt and denim shorts with sneakers", "mood": "eager"}"#;
        let added = ensure_outfit("musashi, beach, flustered expression", state);
        assert!(added.contains("white t-shirt") && added.contains("denim shorts"));
        assert!(added.contains("sneakers"));
        let kept = "musashi, white t-shirt, denim shorts, smile";
        assert_eq!(ensure_outfit(kept, state), kept);
        let nude = "musashi, topless, beach";
        assert_eq!(ensure_outfit(nude, state), nude);
        assert_eq!(ensure_outfit("musashi", r#"{"mood": "calm"}"#), "musashi");
        // Real case: no duplicate uniform, no state sentences, topless added.
        let asuna = r#"{"outfit": "school uniform (blue and white) - top completely off, falling to the floor; bottom remains on.", "clothing_state": "top entirely off and discarded nearby"}"#;
        let out = ensure_outfit(
            "asuna \\(blue archive\\), school uniform \\(blue and white\\), smile",
            asuna,
        );
        assert_eq!(
            out,
            "asuna \\(blue archive\\), blue pleated skirt, smile, topless"
        );
    }

    const ASUNA_TOPLESS: &str = r#"{"outfit": "school uniform (blue and white) - top completely off, falling to the floor; bottom remains on.", "clothing_state": "top entirely off and discarded nearby. Only school uniform bottom remains."}"#;

    #[test]
    fn ensure_outfit_topless_swaps_the_real_uniform_for_its_bottom() {
        let out = ensure_outfit(
            "asuna \\(blue archive\\), 1girl, large breasts, school uniform \\(blue, white\\) - top completely off, halo, hair ribbon, blush",
            ASUNA_TOPLESS,
        );
        assert!(!out.contains("uniform"), "{out}");
        for kept in [
            "blue pleated skirt",
            "halo",
            "hair ribbon",
            "topless",
            "large breasts",
        ] {
            assert!(out.contains(kept), "{kept} lost: {out}");
        }
        // Tops, neckwear and one-piece garments go; bottoms and legwear stay.
        let out = ensure_outfit(
            "musashi, white shirt, black bowtie, pleated skirt, black thighhighs, choker",
            ASUNA_TOPLESS,
        );
        assert_eq!(
            out,
            "musashi, pleated skirt, black thighhighs, choker, topless"
        );
        let out = ensure_outfit("mika, red dress, hair tie, smile", ASUNA_TOPLESS);
        assert_eq!(out, "mika, hair tie, smile, topless");
    }

    #[test]
    fn ensure_outfit_nude_drops_clothes_but_keeps_accessories() {
        let state = r#"{"outfit": "nothing on, completely naked"}"#;
        let out = ensure_outfit(
            "asuna, school uniform, pleated skirt, white panties, black thighhighs, hair ribbon, choker, halo, glasses",
            state,
        );
        assert_eq!(out, "asuna, hair ribbon, choker, halo, glasses, nude");
        let out = ensure_outfit(
            "asuna, white socks, choker",
            r#"{"outfit": "nude except white socks"}"#,
        );
        assert_eq!(out, "asuna, white socks, choker, nude");
    }

    #[test]
    fn ensure_outfit_leaves_a_clothed_state_alone() {
        let state = r#"{"outfit": "school uniform (blue and white)"}"#;
        let pos = "asuna, school uniform \\(blue, white\\), halo, smile";
        assert_eq!(ensure_outfit(pos, state), pos);
    }

    const MUSASHI_CARD: &str = "Appearance (use these tags for photos, always the same):\nmiyamoto musashi \\(fate\\), 1girl, pink hair, long hair, blue eyes, large breasts, toned body.\nWardrobe (photos use the CURRENT outfit from her state, never a fixed one):\n- Summer swimsuit (her usual at the beach): american flag bikini, white shrug \\(clothing\\), black choker, black thighhighs, wooden swords at her hip.\n- Casual: white t-shirt, denim shorts, sneakers, hair in a high ponytail.\n";
    const MUSASHI_POS: &str = "miyamoto musashi \\(fate\\), 1girl, pink hair, blue eyes, large breasts, white t-shirt, denim shorts, sneakers, american flag bikini, white shrug clothing, black choker, black thighhighs, wooden swords at hip, crossing arms defensively";

    #[test]
    fn drop_stray_card_outfit_removes_other_outfits() {
        let allowed = "white t-shirt and denim shorts with sneakers";
        let out = drop_stray_card_outfit(MUSASHI_POS, Some(MUSASHI_CARD), allowed);
        for gone in ["bikini", "shrug", "choker", "thighhighs", "swords"] {
            assert!(!out.contains(gone), "{gone} stayed: {out}");
        }
        for kept in [
            "miyamoto musashi \\(fate\\)",
            "pink hair",
            "large breasts",
            "white t-shirt",
            "denim shorts",
            "sneakers",
            "crossing arms defensively",
        ] {
            assert!(out.contains(kept), "{kept} lost: {out}");
        }
    }

    #[test]
    fn drop_stray_card_outfit_keeps_mentioned_and_ignores_missing_cards() {
        let allowed = "white t-shirt. she says: i put on my black choker";
        let out = drop_stray_card_outfit(MUSASHI_POS, Some(MUSASHI_CARD), allowed);
        assert!(out.contains("black choker") && !out.contains("bikini"));
        assert_eq!(drop_stray_card_outfit(MUSASHI_POS, None, ""), MUSASHI_POS);
        let prose =
            "Asuna is a cheerful girl.\nAppearance: asuna \\(blue archive\\), light brown hair.";
        let pos = "asuna \\(blue archive\\), light brown hair, smile";
        assert_eq!(drop_stray_card_outfit(pos, Some(prose), ""), pos);
        assert_eq!(drop_stray_card_outfit(pos, Some(""), ""), pos);
    }

    #[test]
    fn lone_function_word_goes_even_when_the_card_says_it() {
        let card = "Clothing: what she is currently wearing.";
        assert_eq!(
            refine_vague_tags("1girl, wearing, school uniform", Some(card), "", &|_| false),
            "1girl, school uniform"
        );
    }

    #[test]
    fn riding_gear_is_not_a_sex_act() {
        assert!(!is_sex_act(
            "striking a confident pose while holding the riding crop"
        ));
        assert!(!is_sex_act("pulling on her riding boots"));
        assert!(is_sex_act("riding you slowly"));
        assert!(is_sex_act("riding him, riding crop in hand"));
    }

    #[test]
    fn close_frames_drop_footwear() {
        let pos = "1girl, school uniform, white socks, loafers, smile, close-up";
        assert_eq!(
            drop_offframe_garments(pos),
            "1girl, school uniform, smile, close-up"
        );
        assert_eq!(
            drop_offframe_garments("1girl, black thighhighs, loafers, cowboy shot"),
            "1girl, black thighhighs, cowboy shot"
        );
        assert_eq!(
            drop_offframe_garments("1girl, black thighhighs, close-up selfie"),
            "1girl, close-up selfie"
        );
        let full = "1girl, loafers, full body";
        assert_eq!(drop_offframe_garments(full), full);
        let feet = "1girl, loafers, close-up, feet";
        assert_eq!(drop_offframe_garments(feet), feet);
    }

    #[test]
    fn ensure_framing_follows_the_user() {
        let added = ensure_framing(
            "1girl, smile",
            "can i see really close, in front of my face!",
        );
        assert!(added.ends_with("close-up"), "{added}");
        assert_eq!(
            ensure_framing("1girl, smile", "please change clothes"),
            "1girl, smile"
        );
        assert_eq!(
            ensure_framing("1girl, close-up", "come closer"),
            "1girl, close-up"
        );
        assert!(ensure_framing("1girl", "show your back").ends_with("from behind"));
        assert!(ensure_framing("1girl", "a full body shot").ends_with("full body"));
    }

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
            "size": " Portrait ", "seed": "SAME",
            "sampler": " euler ", "scheduler": "", "checkpoint": "m.safetensors"
        }))
        .unwrap();
        assert_eq!(f.positive.as_deref(), Some("1girl, red dress, selfie"));
        assert_eq!(f.negative.as_deref(), Some("blurry, bad hands"));
        assert_eq!((f.width, f.height, f.steps), (None, None, None));
        assert_eq!(f.size, Some(SizePreset::Portrait));
        assert_eq!(f.seed_mode, Some(SeedMode::Same));
        let g = fields_from_value(&json!({"positive": "a", "size": "huge", "seed": 5})).unwrap();
        assert_eq!((g.size, g.seed_mode), (None, None));
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
            "Asuna, beach, happy, red dress, looking at viewer, on the beach"
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
            Gender::Female,
            Some("Asuna, long hair"),
            r#"{"Outfit":"dress"}"#,
            &[m("user", "show me")],
            "a selfie in a dress",
            Some("at the beach"),
            None,
        );
        assert!(
            user.contains("Asuna, long hair") && user.contains("What the photo shows: a selfie")
        );
        assert!(user.contains("The user asked to see: at the beach"));
        assert!(user.contains("never from this request"));
        assert!(!user.contains("PREVIOUS PHOTO"));
    }

    #[test]
    fn user_request_does_not_undress_the_photo_but_still_frames_it() {
        let history = [
            m("user", "take your top off, full body please"),
            m("assistant", "\"Really? You want that?\""),
        ];
        let input = BuilderInput {
            persona_name: Some("Asuna"),
            persona: None,
            raw_state: r#"{"outfit": "school uniform (blue and white)"}"#,
            history: &history,
            photo_reason: "",
            hint: Some("take your top off"),
            style_rules: "",
            previous_prompt: None,
        };
        let out = apply_backstops("asuna, school uniform \\(blue, white\\), smile", &input);
        assert!(out.contains("school uniform"), "{out}");
        assert!(!out.contains("topless") && !out.contains("nude"), "{out}");
        assert!(out.ends_with("full body"), "{out}");
    }

    const REAL_STATE: &str = r#"{"activity":"staring intensely at the phone screen","arousal":"100%","clothing_state":"top is straining under tension","location":"bed (implied)","mind":"overwhelmed by desire","mood":"ecstatic and desperate for intimacy","outfit":"Blue Archive standard uniform","state":"fully overwhelmed","user_state":"direct and demanding (requesting nudity)"}"#;

    #[test]
    fn visual_state_keeps_only_what_can_be_drawn() {
        let out = visual_state(REAL_STATE);
        for kept in [
            "outfit:",
            "clothing_state:",
            "location:",
            "activity:",
            "mood:",
        ] {
            assert!(out.contains(kept), "{kept} missing in {out}");
        }
        for dropped in ["mind:", "\nstate:", "user_state", "arousal"] {
            assert!(!out.contains(dropped), "{dropped} present in {out}");
        }
        assert!(!out.starts_with("state:"));
        assert_eq!(visual_state(r#"{"mind":"x","outfit":"none"}"#), "");
    }

    #[test]
    fn positive_cleanup_drops_copied_state_and_shape_words() {
        let raw = "asuna (blue archive), extremely aroused, outfit: Blue Archive standard uniform, \
top partially pulled down to reveal cleavage, mind: overwhelmed with arousal and eager anticipation \
of further attention, state: extremely aroused, actively trying to provoke a reaction while \
displaying herself eagerly awaiting visual confirmation, user_state: challenging/teasing (winking), \
(smile:1.2), masterpiece; best quality. score_9, portrait";
        let out = clean_positive(raw);
        let tags: Vec<&str> = out.split(", ").collect();
        for keep in [
            "asuna \\(blue archive\\)",
            "(smile:1.2)",
            "Blue Archive standard uniform",
            "masterpiece",
            "best quality",
            "score_9",
        ] {
            assert!(tags.contains(&keep), "{keep} missing in {out}");
        }
        for gone in ["mind", "user_state", "challenging", "portrait", "eagerly"] {
            assert!(!out.contains(gone), "{gone} present in {out}");
        }
        assert!(!out.contains("outfit:"));
        let second = clean_positive(
            "smile, challenging/teasing (winking) user state, landscape orientation",
        );
        assert_eq!(second, "smile");
        assert!(fields_from_value(&json!({"positive": "portrait, landscape"})).is_none());
        assert_eq!(
            clean_positive("embedding:lazyhand_1450646, rating:explicit, asuna \\(blue archive\\)"),
            "embedding:lazyhand_1450646, rating:explicit, asuna \\(blue archive\\)"
        );
    }

    #[test]
    fn photo_flow_decides_who_triggers_the_photo() {
        use PhotoMode::*;
        let f = |m, c, forced| photo_flow(m, c, forced);
        let every = f(EveryReply, true, false);
        assert!(every.force && every.auto_images && !every.director_signals);
        assert!(!f(EveryReply, false, false).force);
        let auto = f(Auto, true, false);
        assert!(!auto.force && auto.auto_images && auto.director_signals);
        assert!(!f(Auto, true, true).director_signals && f(Auto, true, true).force);
        let off = f(Off, true, false);
        assert!(!off.force && !off.auto_images && !off.director_signals);
        assert!(f(Off, true, true).force);
        let mut cfg = AppConfig {
            chat_photo_every_reply: true,
            ..Default::default()
        };
        assert_eq!(PhotoMode::from_config(&cfg), EveryReply);
        cfg.chat_auto_images = false;
        assert_eq!(PhotoMode::from_config(&cfg), Off);
    }

    #[test]
    fn escape_parens_cases() {
        assert_eq!(
            escape_parens("asuna (blue archive), smile"),
            "asuna \\(blue archive\\), smile"
        );
        let done = "asuna \\(blue archive\\), smile";
        assert_eq!(escape_parens(done), done);
        assert_eq!(
            escape_parens("(smile:1.2), (a b: .5)"),
            "(smile:1.2), (a b: .5)"
        );
        assert_eq!(
            escape_parens("musashi (swimsuit berserker) (fate), (blush:1.1)"),
            "musashi \\(swimsuit berserker\\) \\(fate\\), (blush:1.1)"
        );
        assert_eq!(escape_parens("(a (b:1.2))"), "\\(a (b:1.2)\\)");
        assert_eq!(escape_parens("oops (open, x) y"), "oops \\(open, x\\) y");
        assert_eq!(escape_parens("a (b"), "a \\(b");
        let once = escape_parens("x (y) (z:2) )");
        assert_eq!(escape_parens(&once), once);
    }

    #[test]
    fn builder_user_content_puts_this_photo_first_and_adds_previous_prompt() {
        let history = [m("user", "show me"), m("assistant", "Here, look at me!")];
        let user = builder_user_content(
            Gender::Female,
            Some("Asuna"),
            REAL_STATE,
            &history,
            "",
            None,
            Some("1girl, beach, selfie"),
        );
        let photo = user.find("THIS PHOTO").unwrap();
        assert!(photo < user.find("CHARACTER CARD").unwrap());
        assert!(user.contains("Her latest message: Here, look at me!"));
        assert!(user.contains("PREVIOUS PHOTO'S PROMPT"));
        assert!(user.contains("1girl, beach, selfie"));
        assert!(!user.contains("arousal"));
    }

    const LOG_CARD: &str = "Name: Miyamoto Musashi\n- Appearance: miyamoto musashi \\(fate\\), 1girl, pink hair, long hair, blue eyes, large breasts\n";
    const LOG_STATE_4: &str = r#"{"outfit": "summer swimsuit (american flag bikini), black choker, wooden swords at hip", "clothing_state": null, "location": "Master's apartment bedroom", "activity": "hopping onto a bed"}"#;
    const LOG_STATE_5: &str = r#"{"outfit": "summer swimsuit (american flag bikini), black choker, wooden swords at hip", "clothing_state": "swimsuit clinging to her body; wet patches on skin", "location": "Master's apartment bedroom", "activity": "embracing the user"}"#;

    fn log_input<'a>(raw_state: &'a str, history: &'a [ChatMessage]) -> BuilderInput<'a> {
        BuilderInput {
            persona_name: Some("Miyamoto Musashi (Berserker)"),
            persona: Some(LOG_CARD),
            raw_state,
            history,
            photo_reason: "",
            hint: None,
            style_rules: "",
            previous_prompt: None,
        }
    }

    #[test]
    fn real_turn_four_prompt_loses_stale_garments_and_gains_a_location() {
        let history = [
            m("user", "come lie on the bed"),
            m("assistant", "*hops on the bed*"),
        ];
        let raw = "miyamoto musashi \\(fate\\), 1girl, pink hair, long hair, blue eyes, large breasts, summer swimsuit \\(american flag bikini\\), black choker, wooden swords at hip, shrug off one shoulder, thighhigh left on the floor, flustered expression, eager smile, close-up";
        let out = apply_backstops(raw, &log_input(LOG_STATE_4, &history));
        assert!(
            !out.contains("shrug") && !out.contains("thighhigh"),
            "{out}"
        );
        assert!(
            out.contains("black choker") && out.contains("wooden swords at hip"),
            "{out}"
        );
        assert!(
            out.contains("pink hair") && out.contains("flustered expression"),
            "{out}"
        );
        assert!(out.contains("bedroom") && out.contains("indoors"), "{out}");
    }

    #[test]
    fn real_turn_five_prompt_loses_names_and_clauses() {
        let history = [
            m("user", "get on top of me"),
            m("assistant", "*climbs on Master*"),
        ];
        let raw = "miyamoto musashi \\(fate\\), 1girl, pink hair, long hair, blue eyes, large breasts, summer swimsuit \\(american flag bikini\\), black choker, wooden swords at hip, shrug completely off one shoulder, thighhigh left on the floor, embracing Master, wet patches on skin, flustered expression, excited smile";
        let out = apply_backstops(raw, &log_input(LOG_STATE_5, &history));
        for gone in ["shrug", "thighhigh", "Master", "embracing"] {
            assert!(!out.contains(gone), "{gone}: {out}");
        }
        assert!(
            out.contains("wet patches on skin") && out.contains("excited smile"),
            "{out}"
        );
        assert!(out.starts_with("miyamoto musashi"), "{out}");
    }

    #[test]
    fn sentence_tags_are_dropped_conservatively() {
        let out = drop_sentence_tags(
            "asuna, blue eyes, she looks at Master, holding a sword while smiling at you, hand on own hip, asuna's sword, sitting on the floor",
            Some("Asuna"),
            None,
        );
        assert_eq!(
            out,
            "asuna, blue eyes, hand on own hip, sitting on the floor"
        );
    }

    #[test]
    fn location_is_added_only_when_missing() {
        let state = r#"{"location": "Master's apartment bedroom"}"#;
        assert_eq!(
            ensure_location("1girl, smile", state),
            "1girl, smile, bedroom, indoors"
        );
        assert_eq!(ensure_location("1girl, on bed", state), "1girl, on bed");
        let beach = r#"{"location": "sunny beach"}"#;
        assert_eq!(ensure_location("1girl", beach), "1girl, beach, outdoors");
        let flat = r#"{"location": "Master's apartment"}"#;
        assert_eq!(ensure_location("1girl", flat), "1girl, indoors");
        assert_eq!(
            ensure_location("1girl", r#"{"location": "unknown"}"#),
            "1girl"
        );
        assert_eq!(ensure_location("1girl", "{}"), "1girl");
        // Real case: "school grounds" became "classroom, indoors".
        let grounds = r#"{"location": "school grounds"}"#;
        assert_eq!(ensure_location("1girl", grounds), "1girl, outdoors");
        assert_eq!(
            ensure_location("1girl, school grounds", grounds),
            "1girl, school grounds"
        );
        let class = r#"{"location": "school classroom"}"#;
        assert_eq!(ensure_location("1girl", class), "1girl, classroom, indoors");
    }

    fn backstop_input<'a>(
        name: &'a str,
        card: &'a str,
        raw_state: &'a str,
        history: &'a [ChatMessage],
    ) -> BuilderInput<'a> {
        BuilderInput {
            persona_name: Some(name),
            persona: Some(card),
            raw_state,
            history,
            photo_reason: "",
            hint: None,
            style_rules: "",
            previous_prompt: None,
        }
    }

    const TAMAMO_CARD: &str = "Name: Tamamo no Mae\n- Appearance: tamamo no mae \\(fate/extra\\), 1girl, pink hair, fox ears, fox tail, yellow eyes, large breasts\n- Outfit: blue kimono, detached sleeves, red obi, white thighhighs\n";
    const MEDB_CARD: &str = "Name: Medb\n- Appearance: medb \\(fate\\), 1girl, pink hair, long hair, yellow eyes, large breasts\n";
    const TAMAMO_NAKED: &str = r#"{"outfit": "completely naked (blue Japanese kimono, red obi, white thighhighs removed)", "clothing_state": null, "location": "user's apartment", "activity": "stripping completely", "mood": "ecstatic and highly aroused"}"#;

    #[test]
    fn worn_clothing_drops_garments_flagged_as_gone() {
        let medb =
            "queen outfit: white crop top, white thighhighs (removed), pink cape (discarded)";
        assert_eq!(worn_clothing(medb), "queen outfit: white crop top");
        let tamamo = "completely naked (blue japanese kimono, red obi, white thighhighs removed)";
        assert_eq!(worn_clothing(tamamo), "completely naked");
        assert_eq!(
            worn_clothing("white gloves, pink cape tossed aside, black choker"),
            "white gloves, black choker"
        );
        // Not a removal: a colour note and a half-open garment stay.
        assert_eq!(
            worn_clothing("school uniform (blue and white)"),
            "school uniform (blue and white)"
        );
        assert_eq!(
            worn_clothing("kimono (top partially off), red obi"),
            "kimono (top partially off), red obi"
        );
    }

    #[test]
    fn removed_markers_do_not_reach_the_prompt() {
        let history = [
            m("user", "now strip completely"),
            m("assistant", "*she lets the fabric fall away*"),
        ];
        let raw = "tamamo no mae \\(fate/extra\\), 1girl, pink hair, large breasts, ecstatic and highly aroused, completely naked, red obi, white thighhighs removed, delighted gasp, cheeks flushing bright pink, giggling, fox tail flicking with excitement, full body, nude, indoors";
        let out = apply_backstops(
            raw,
            &backstop_input("Tamamo no Mae", TAMAMO_CARD, TAMAMO_NAKED, &history),
        );
        for gone in [
            "removed",
            "obi",
            "thighhighs",
            "gasp",
            "excitement",
            "completely",
        ] {
            assert!(!out.contains(gone), "{gone}: {out}");
        }
        for kept in [
            "nude",
            "full body",
            "ecstatic",
            "aroused",
            "blush",
            "indoors",
        ] {
            assert!(out.contains(kept), "{kept}: {out}");
        }
        assert!(out.starts_with("tamamo no mae"), "{out}");
    }

    #[test]
    fn state_echo_goes_and_sex_activity_adds_act_tags() {
        let medb = r#"{"outfit": "white gloves, white thighhighs, pink cape", "clothing_state": "loosened", "activity": "touching herself"}"#;
        let out = drop_state_echo(
            "medb, 1girl, white gloves, masturbation, loosened clothing state, loosened",
            medb,
        );
        assert_eq!(out, "medb, 1girl, white gloves, masturbation");
        let tamamo = r#"{"outfit": "none", "clothing_state": "nude", "activity": "riding master and taking a photo"}"#;
        assert_eq!(drop_state_echo("tamamo, nude", tamamo), "tamamo, nude");
        let out = ensure_act_tags(
            "tamamo, 1girl, straddling hips, nude",
            tamamo,
            Pair::default(),
        );
        assert!(out.contains("girl on top") && out.contains("sex") && out.contains("1boy"));
        let had = "medb, girl on top, sex, 1boy";
        assert_eq!(ensure_act_tags(had, tamamo, Pair::default()), had);
        let posing = r#"{"activity": "posing for a photo"}"#;
        assert_eq!(
            ensure_act_tags("x, nude", posing, Pair::default()),
            "x, nude"
        );
    }

    #[test]
    fn us_photo_adds_the_partner_without_an_act() {
        let base = "1girl, pink hair, on bed";
        let out = ensure_us_pair(base, "touch me. send me a photo of us", Pair::default());
        assert!(out.contains("1boy") && out.contains("1girl"), "{out}");
        assert!(!out.contains("sex") && !out.contains("hetero"), "{out}");
        let same = Pair {
            character: Gender::Female,
            user: Gender::Female,
        };
        let out = merge_same_gender_counts(&ensure_us_pair(base, "a selfie together", same), same);
        assert!(out.contains("2girls") && !out.contains("1girl"), "{out}");
        assert_eq!(ensure_us_pair(base, "send a photo", Pair::default()), base);
        let had = "1girl, 1boy, hug";
        assert_eq!(ensure_us_pair(had, "photo of us", Pair::default()), had);
    }

    #[test]
    fn character_names_are_not_photo_tags() {
        let none = |_: &str| false;
        let card = "Character: Kaito Mori, an original character.\n- Appearance: 1boy, black hair, blue eyes\n";
        let out = drop_name_tags(
            "Kaito Mori, 1boy, black hair, kaito",
            Some("Kaito Mori"),
            Some(card),
            &none,
        );
        assert_eq!(out, "1boy, black hair");
        let out = drop_name_tags(
            "medb \\(fate\\), 1girl",
            Some("Medb"),
            Some("Character: Medb from Fate."),
            &none,
        );
        assert_eq!(out, "medb \\(fate\\), 1girl");
        let out = drop_name_tags(
            "asuna \\(blue archive\\), asuna, smile",
            Some("Asuna (Blue Archive)"),
            None,
            &none,
        );
        assert_eq!(out, "asuna \\(blue archive\\), smile");
        let card =
            "Character: Nova, an original character.\n- Appearance: nova, 1girl, silver hair\n";
        assert_eq!(
            drop_name_tags("nova, 1girl", Some("Nova"), Some(card), &none),
            "nova, 1girl"
        );
    }

    #[test]
    fn american_flag_print_keeps_its_name() {
        let none = |_: &str| false;
        let out = refine_prose_tags(
            "musashi, american flag bikini bottom, blue Japanese kimono",
            None,
            None,
            &none,
        );
        assert!(out.contains("american flag bikini bottom"), "{out}");
        assert!(out.contains("blue kimono"), "{out}");
    }

    #[test]
    fn an_outfit_of_none_means_nude() {
        let state = r#"{"outfit": "none", "clothing_state": null, "location": "Master's apartment", "activity": "Touching herself while posing for a photo"}"#;
        let eff = effective_state(state, "*She smiles.*");
        let out = ensure_outfit("tamamo no mae, 1girl, close-up, masturbation, on bed", &eff);
        assert!(out.contains("nude"), "{out}");
        assert!(
            !split_tags_outside_parens(&out).iter().any(|t| t == "none"),
            "{out}"
        );
        let dressed = r#"{"outfit": "red obi, white thighhighs"}"#;
        assert_eq!(effective_state(dressed, ""), dressed);
    }

    #[test]
    fn someone_elses_nudity_is_not_hers() {
        assert!(!says_naked("She traces Master's naked chest"));
        assert!(!says_naked("She stares at his naked body"));
        assert!(says_naked("She's naked now, her naked skin glowing"));
    }

    #[test]
    fn reply_saying_naked_beats_a_state_that_keeps_garments() {
        let state = r#"{"outfit": "Queen outfit: white crop top, white thighhighs (removed), pink cape (discarded)", "clothing_state": null, "location": "Medb's chamber", "activity": "posing for a full body photo"}"#;
        let history = [
            m("user", "strip completely"),
            m("assistant", "*She tosses the cape aside.* \"Here you go, Master!\" *She strikes a pose, completely naked, beaming at the camera.*"),
        ];
        let raw = "medb \\(fate\\), 1girl, pink hair, long hair, yellow eyes, large breasts, white crop top, ecstatic, proud, full body";
        let out = apply_backstops(raw, &backstop_input("Medb", MEDB_CARD, state, &history));
        assert!(!out.contains("crop top"), "{out}");
        assert!(out.contains("nude") && out.contains("full body"), "{out}");
    }

    #[test]
    fn sex_act_activity_means_nude_and_maps_the_act() {
        let state = r#"{"outfit": "Summer swimsuit (american flag bikini, black choker, black thighhighs)", "clothing_state": null, "location": "Master's apartment bedroom", "activity": "riding Master", "mood": "flustered, excited"}"#;
        let history = [
            m("user", "ride my cock"),
            m("assistant", "*she pushes herself onto him*"),
        ];
        let raw = "miyamoto musashi \\(fate\\), 1girl, pink hair, long hair, blue eyes, large breasts, riding Master, flustered, excited, summer swimsuit, american flag bikini, black choker, black thighhighs, bedroom, indoors";
        let out = apply_backstops(raw, &log_input(state, &history));
        for gone in ["Master", "bikini", "swimsuit", "thighhighs", "riding"] {
            assert!(!out.contains(gone), "{gone}: {out}");
        }
        for kept in [
            "girl on top",
            "sex",
            "1boy",
            "nude",
            "black choker",
            "bedroom",
        ] {
            assert!(out.contains(kept), "{kept}: {out}");
        }
        assert_eq!(out.matches("1boy").count(), 1, "{out}");
    }

    #[test]
    fn nude_tags_from_the_users_words_are_dropped_while_dressed() {
        let state = r#"{"outfit": "Summer swimsuit (american flag bikini, black choker, black thighhighs)", "clothing_state": null, "location": "Master's apartment", "activity": "striking a pose after removing her shrug", "mood": "flustered, smug"}"#;
        let history = [
            m("user", "strip completely, I want you totally naked"),
            m(
                "assistant",
                "*she shrugs off the white shrug and lets it drop* \"Alright, Master! Here!\"",
            ),
        ];
        let raw = "miyamoto musashi \\(fate\\), 1girl, pink hair, long hair, blue eyes, large breasts, totally naked, full body, flustered, seductive smile, indoors";
        let out = apply_backstops(raw, &log_input(state, &history));
        for gone in ["naked", "nude", "topless"] {
            assert!(!out.contains(gone), "{gone}: {out}");
        }
        assert!(out.contains("bikini") && out.contains("full body"), "{out}");
        assert_eq!(
            gate_nude_tags("a, completely naked, smile", Undress::Nude),
            "a, nude, smile"
        );
        assert_eq!(
            gate_nude_tags("a, nude, topless", Undress::Topless),
            "a, topless"
        );
    }

    #[test]
    fn a_possessive_user_reference_adds_no_boy() {
        let none = |_: &str| false;
        let out = refine_prose_tags(
            "tamamo, 1girl, Master's apartment, selfie, your bed",
            None,
            None,
            &none,
        );
        assert!(out.contains("apartment") && out.contains("bed"), "{out}");
        assert!(!out.contains("1boy"), "{out}");
        let out = refine_prose_tags("tamamo, riding master", None, None, &none);
        assert!(out.contains("1boy"), "{out}");
    }

    #[test]
    fn names_become_one_boy_and_the_persona_name_goes() {
        let state = r#"{"outfit": "Queen outfit: white crop top, white miniskirt", "clothing_state": null, "location": "Master's room", "activity": "hugging Master tightly"}"#;
        let history = [m("user", "come here"), m("assistant", "*she smiles*")];
        let raw = "medb \\(fate\\), 1girl, pink hair, white crop top, white miniskirt, ecstatic, demanding, triumphant smirk, Medb, 1 man, looking at Master, indoors";
        let out = apply_backstops(raw, &backstop_input("Medb", MEDB_CARD, state, &history));
        assert!(!out.contains("Medb") && !out.contains("1 man"), "{out}");
        assert!(!out.contains("looking at"), "{out}");
        assert_eq!(out.matches("1boy").count(), 1, "{out}");
        assert!(out.starts_with("medb \\(fate\\)"), "{out}");
        assert!(out.contains("triumphant smirk"), "{out}");
    }

    #[test]
    fn prose_tags_become_tags_or_go() {
        let none = |_: &str| false;
        let out = refine_prose_tags(
            "tamamo no mae \\(fate/extra\\), blue Japanese kimono with detached sleeves, blushes deeply, sparkling yellow eyes, eager sigh, playful hesitation, fully engaged in intercourse, body slick against the user, ecstatic and highly aroused, ecstatic and intensely passionate, touching herself, kissing, black and white ribbon",
            Some("Tamamo no Mae"),
            Some(TAMAMO_CARD),
            &none,
        );
        assert_eq!(
            out,
            "tamamo no mae \\(fate/extra\\), blue kimono, detached sleeves, blush, sparkling yellow eyes, sex, hetero, 1boy, ecstatic, aroused, passionate, masturbation, kiss, black and white ribbon"
        );
        // Tags the tag data knows are spared the prose heuristics.
        let known = |t: &str| t == "eager sigh";
        let out = refine_prose_tags("a, eager sigh", None, None, &known);
        assert_eq!(out, "a, eager sigh");
    }

    const MUSASHI_E4B_TOPLESS: &str = r#"{"outfit": "American flag bikini, black choker, black thighhighs, wooden swords at hip", "clothing_state": "Bikini top removed", "location": "Apartment", "activity": "Posing provocatively, waiting for Master's reaction", "mood": "Flustered, Teasing"}"#;

    #[test]
    fn vague_and_setting_tags_are_cleaned() {
        let none = |_: &str| false;
        let out = refine_prose_tags(
            "a, intense moment, demanding movement, playful intent, apartment setting, bed setting, bed in apartment, heavy breathing",
            None,
            None,
            &none,
        );
        assert_eq!(out, "a, apartment, bed, heavy breathing");
        let known = |t: &str| t == "heavy breathing" || t == "intense moment";
        let out = refine_prose_tags("a, intense moment, heavy breathing", None, None, &known);
        assert_eq!(out, "a, intense moment, heavy breathing");
    }

    #[test]
    fn and_split_shares_the_final_noun() {
        let none = |_: &str| false;
        let out = refine_prose_tags("a, loose and flowing hair", None, None, &none);
        assert_eq!(out, "a, loose hair, flowing hair");
    }

    #[test]
    fn solo_photo_drops_one_boy() {
        let state = r#"{"outfit": "blue kimono, red obi", "clothing_state": "wearing", "location": "Master's apartment", "activity": "holding up an imaginary phone"}"#;
        let history = [m("user", "send a pic"), m("assistant", "*snaps a selfie*")];
        let raw = "tamamo no mae \\(fate/extra\\), 1girl, pink hair, blue kimono, red obi, selfie, apartment, 1boy, 1man";
        let out = apply_backstops(
            raw,
            &backstop_input("Tamamo no Mae", TAMAMO_CARD, state, &history),
        );
        assert!(!out.contains("1boy"), "{out}");
        let kept = gate_partner_tag("a, sex, 1boy", false, state, Pair::default());
        assert_eq!(kept, "a, sex, 1boy");
        assert_eq!(
            gate_partner_tag("a, kiss, 1boy", false, state, Pair::default()),
            "a, kiss, 1boy"
        );
        assert_eq!(
            gate_partner_tag("a, 1boy", true, state, Pair::default()),
            "a, 1boy"
        );
        let with_user = r#"{"activity": "hugging Master tightly"}"#;
        assert_eq!(
            gate_partner_tag("a, 1boy", false, with_user, Pair::default()),
            "a, 1boy"
        );
        assert_eq!(
            gate_partner_tag("a, 1boy", false, state, Pair::default()),
            "a"
        );
    }

    #[test]
    fn topless_bikini_keeps_its_bottom_and_drops_lower_body_framing() {
        let history = [
            m("user", "take your top off"),
            m("assistant", "*she pulls the bikini top off*"),
        ];
        let raw = "miyamoto musashi \\(fate\\), 1girl, pink hair, long hair, blue eyes, large breasts, black choker, black thighhighs, wooden swords at hip, apartment setting, selfie, flustered, teasing, lower body view, provocative pose, loose flowing hair, topless";
        let out = apply_backstops(raw, &log_input(MUSASHI_E4B_TOPLESS, &history));
        assert!(out.contains("american flag bikini bottom"), "{out}");
        assert!(
            out.contains("topless") && out.contains("black choker"),
            "{out}"
        );
        assert!(
            !out.contains("lower body") && !out.contains("setting"),
            "{out}"
        );
        // A bikini tag the builder kept becomes its bottom.
        let swapped = strip_removed_garments("a, american flag bikini, choker", false, "bikini");
        assert_eq!(swapped, "a, american flag bikini bottom, choker");
    }

    const ARTHUR_CARD: &str =
        "Name: Arthur\n- Appearance: arthur pendragon \\(fate\\), 1boy, blonde hair, green eyes\n";
    const RIDING_STATE: &str = r#"{"outfit": "white shirt", "clothing_state": null, "location": "bedroom", "activity": "riding the user"}"#;
    const SELFIE_STATE: &str = r#"{"outfit": "white shirt", "clothing_state": null, "location": "bedroom", "activity": "posing"}"#;

    fn tag_set(out: &str) -> Vec<String> {
        out.split(", ").map(str::to_lowercase).collect()
    }

    #[test]
    fn character_gender_reads_the_card_tags() {
        assert_eq!(character_gender(Some(MEDB_CARD)), Gender::Female);
        assert_eq!(character_gender(Some(ARTHUR_CARD)), Gender::Male);
        assert_eq!(
            character_gender(Some("Name: X\n- nothing here")),
            Gender::Female
        );
        assert_eq!(character_gender(Some("1girl, 1boy")), Gender::Female);
        assert_eq!(character_gender(None), Gender::Female);
    }

    #[test]
    fn user_gender_card_line_beats_the_setting() {
        let card = format!("{MEDB_CARD}- User gender: female\n");
        assert_eq!(
            Pair::from_card(Some(&card), Gender::Male).user,
            Gender::Female
        );
        let card = format!("{MEDB_CARD}- The user is a man\n");
        assert_eq!(
            Pair::from_card(Some(&card), Gender::Female).user,
            Gender::Male
        );
        assert_eq!(card_user_gender(Some("user: Woman")), Some(Gender::Female));
        assert_eq!(
            Pair::from_card(Some(MEDB_CARD), Gender::Female).user,
            Gender::Female
        );
        assert_eq!(
            Pair::from_card(Some(MEDB_CARD), Gender::Male).user,
            Gender::Male
        );
    }

    #[test]
    fn male_card_keeps_his_own_count_tag_on_a_solo_photo() {
        let history = [m("user", "selfie?"), m("assistant", "*smiles*")];
        let input = backstop_input("Arthur", ARTHUR_CARD, SELFIE_STATE, &history);
        let out = apply_backstops(
            "arthur pendragon \\(fate\\), 1boy, blonde hair, selfie",
            &input,
        );
        assert!(tag_set(&out).contains(&"1boy".to_string()), "{out}");
    }

    #[test]
    fn male_card_and_woman_user_riding_uses_boy_on_top_and_hetero() {
        let history = [m("user", "come here"), m("assistant", "*rides*")];
        let input = backstop_input("Arthur", ARTHUR_CARD, RIDING_STATE, &history);
        let out = apply_backstops_with(
            "arthur pendragon \\(fate\\), 1boy, blonde hair",
            &input,
            &|_| false,
            Gender::Female,
        );
        let tags = tag_set(&out);
        for want in ["boy on top", "sex", "hetero", "1boy", "1girl"] {
            assert!(tags.contains(&want.to_string()), "{want} missing: {out}");
        }
        assert!(!tags.contains(&"girl on top".to_string()), "{out}");
    }

    #[test]
    fn female_card_and_woman_user_sex_uses_2girls_and_yuri() {
        let history = [m("user", "come here"), m("assistant", "*moans*")];
        let state = r#"{"outfit": "none", "clothing_state": null, "location": "bedroom", "activity": "having sex with the user"}"#;
        let input = backstop_input("Medb", MEDB_CARD, state, &history);
        let out = apply_backstops_with(
            "medb \\(fate\\), 1girl, pink hair",
            &input,
            &|_| false,
            Gender::Female,
        );
        let tags = tag_set(&out);
        assert!(tags.contains(&"2girls".to_string()), "{out}");
        assert!(tags.contains(&"yuri".to_string()), "{out}");
        assert!(!tags.contains(&"1girl".to_string()), "{out}");
        assert!(!tags.contains(&"1boy".to_string()), "{out}");
    }

    #[test]
    fn gendered_swaps_she_and_her_for_a_male_character() {
        assert_eq!(
            gendered("Her outfit: she wears her hat", Gender::Male),
            "His outfit: he wears his hat"
        );
        assert_eq!(gendered("Her outfit", Gender::Female), "Her outfit");
        assert_eq!(gendered("there, shell", Gender::Male), "there, shell");
        assert_eq!(
            gendered("Tell her to fix her hat, then look at her.", Gender::Male),
            "Tell him to fix his hat, then look at him."
        );
        assert_eq!(gendered("give her a hat", Gender::Male), "give him a hat");
    }
}
