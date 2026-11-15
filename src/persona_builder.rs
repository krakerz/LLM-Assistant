//! Persona builder: turns a short brief into a persona card (`personas/*.md`,
//! the Musashi card shape), one section per model call.
//!
//! The model only fills small slots; `render` writes the card from a fixed
//! template, so names, the adult line, the photo-habit level, the eight base
//! stats, the format rules and the content-level rule are never model output.
//! A failed or unusable call yields a deterministic fallback section (flagged
//! for the UI). Post-processing is structural only (trim, single line, caps,
//! de-dup): nothing is filtered or rewritten for its content at any level.
//!
//! The backend is stateless: the draft lives in the UI and comes back with
//! every call. The only state is a cancel registry keyed by the client's
//! `build_id`. Builder-made personas get a sidecar draft
//! (`personas/.builder/<name>.json`) so they can be reopened.

use crate::chat_director as director;
use crate::config::AppConfig;
use crate::danbooru::{self, TagCheck, TagClass, TagInfo, TagSource, TagStatus};
use crate::llm::{self, ChatMessage};
use crate::paths::{sanitize_filename, write_atomic};
use crate::sandbox::CancelToken;
use crate::{comfyui, persona, ruleset, searxng};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Per model call; the sections are short, so this only guards a hung server.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const SIDECAR_VERSION: u32 = 1;
const MAX_OUTFITS: usize = 5;
const MAX_OUTFIT_ITEMS: usize = 8;
const MAX_TRAITS: usize = 6;
/// Longest draft context (identity summary + personality) sent with a section call.
const CONTEXT_CHARS: usize = 1200;
const SNIPPET_COUNT: usize = 5;
const SNIPPET_CHARS: usize = 300;
const SNIPPETS_TOTAL_CHARS: usize = 1500;
/// A body tag is suggested when it co-occurs with at least this share of the
/// character's posts. Both thresholds sit above the bundled list's pruning
/// ratio (0.30), so the bundled and the folder layer give the same answer.
const BODY_TAG_RATIO: f64 = 0.35;
const OUTFIT_TAG_RATIO: f64 = 0.5;
const MAX_BODY_TAGS: usize = 6;
const TEST_PHOTO_HINT: &str = "a simple portrait to check her look";

/// The eight stats every card has; model-made extra stats may not reuse them.
const BASE_STATS: [&str; 8] = [
    "Outfit",
    "Clothing State",
    "Location",
    "Activity",
    "Mood",
    "Hair",
    "Affection",
    "Mind",
];

// --- Types ---

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppearanceStyle {
    #[default]
    Tags,
    Plain,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhotoHabit {
    Often,
    #[default]
    Normal,
    Rarely,
}

impl PhotoHabit {
    fn word(self) -> &'static str {
        match self {
            Self::Often => "often",
            Self::Normal => "normal",
            Self::Rarely => "rarely",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentLevel {
    AllAges,
    #[default]
    Suggestive,
    Mature,
}

/// The rule line written for `ContentLevel::Mature`; also the default for
/// cards that state no content level.
pub(crate) const MATURE_RULE_LINE: &str = "Explicit content is allowed when the story leads there.";

impl ContentLevel {
    fn label(self) -> &'static str {
        match self {
            Self::AllAges => "all ages",
            Self::Suggestive => "suggestive",
            Self::Mature => "mature",
        }
    }

    fn rule_line(self) -> &'static str {
        match self {
            Self::AllAges => "Keep everything non-explicit; intimate scenes fade to black.",
            Self::Suggestive => "Flirty and teasing is fine, never explicit.",
            Self::Mature => MATURE_RULE_LINE,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gender {
    #[default]
    Female,
    Male,
    Other,
}

impl Gender {
    fn title(self) -> &'static str {
        match self {
            Self::Female => "Female",
            Self::Male => "Male",
            Self::Other => "Other",
        }
    }

    fn photo_tag(self) -> Option<&'static str> {
        match self {
            Self::Female => Some("1girl"),
            Self::Male => Some("1boy"),
            Self::Other => None,
        }
    }

    fn pronouns(self) -> Pronouns {
        match self {
            Self::Female => Pronouns {
                her: "her",
                she: "she",
                is: "is",
                s: "s",
            },
            Self::Male => Pronouns {
                her: "his",
                she: "he",
                is: "is",
                s: "s",
            },
            Self::Other => Pronouns {
                her: "their",
                she: "they",
                is: "are",
                s: "",
            },
        }
    }
}

/// Words of the card template that change with the character's gender.
struct Pronouns {
    her: &'static str,
    she: &'static str,
    /// "is" / "are".
    is: &'static str,
    /// Verb ending: "s" for he/she, "" for they.
    s: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Identity,
    Appearance,
    Wardrobe,
    Personality,
    PhotoHabit,
    Stats,
    Rules,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveMode {
    Create,
    Update,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Brief {
    pub name: String,
    pub source: String,
    pub vibe: String,
    pub content_level: ContentLevel,
    pub photo_habit: PhotoHabit,
    pub appearance_style: AppearanceStyle,
}

impl Brief {
    /// No source, or the user typed "original".
    pub fn is_original(&self) -> bool {
        let s = self.source.trim();
        s.is_empty() || s.eq_ignore_ascii_case("original")
    }

    /// Trimmed, single-line, capped; every command applies this first.
    pub fn sanitized(mut self) -> Self {
        self.name = cap(&single_line(&self.name), 80);
        self.source = cap(&single_line(&self.source), 120);
        self.vibe = cap(&single_line(&self.vibe), 600);
        self
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Identity {
    pub summary: String,
    pub gender: Gender,
    pub speaks_as: String,
    pub calls_user: String,
    pub short_name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub character_tag: String,
    pub tags: Vec<String>,
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Outfit {
    pub name: String,
    pub items: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stat {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    pub mood: String,
    pub affection_mid: String,
    pub affection_high: String,
    pub extra: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Draft {
    pub identity: Identity,
    pub appearance: Appearance,
    pub wardrobe: Vec<Outfit>,
    pub personality: Vec<String>,
    pub photo_reason: String,
    pub extra_stats: Vec<Stat>,
    pub rules: Rules,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snippet {
    pub title: String,
    pub url: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SectionOutcome {
    pub section: Section,
    /// The value of the `Draft` field this section belongs to.
    pub value: Value,
    pub fallback: bool,
    pub stopped: bool,
    pub warning: Option<String>,
    /// Appearance / Wardrobe in Tags style; empty otherwise.
    pub tag_checks: Vec<TagCheck>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Suggestion {
    pub character: TagInfo,
    /// Display form.
    pub body_tags: Vec<String>,
    pub outfits: Vec<Outfit>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub tag_source: TagSource,
    pub online_lookup: bool,
    pub web_lookup: bool,
    pub test_photo: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedBuild {
    pub version: u32,
    pub persona: String,
    pub brief: Brief,
    pub draft: Draft,
    /// The card text exactly as the builder saved it.
    pub content: String,
    /// Set by `load`: the card on disk differs from `content`.
    #[serde(default)]
    pub hand_edited: bool,
}

// --- Small text helpers (structural only) ---

fn single_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cap(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

/// One line without list markers, numbering or wrapping double quotes.
fn clean_line(s: &str) -> String {
    let mut t = single_line(s);
    loop {
        let before = t.len();
        t = t
            .trim_start_matches(['-', '*', '\u{2022}'])
            .trim_start()
            .to_string();
        let digits = t.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && digits <= 2 {
            let rest = &t[digits..];
            if let Some(rest) = rest.strip_prefix(['.', ')']) {
                if rest.starts_with(' ') {
                    t = rest.trim_start().to_string();
                }
            }
        }
        if t.len() == before {
            break;
        }
    }
    let quote = |c: char| matches!(c, '"' | '\u{201c}' | '\u{201d}');
    if t.chars().count() >= 2 && t.starts_with(quote) && t.ends_with(quote) {
        t = t.trim_matches(quote).trim().to_string();
    }
    t
}

fn dedup_ci(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|s| !s.is_empty() && seen.insert(s.to_lowercase()))
        .collect()
}

fn str_field(raw: &Value, key: &str) -> String {
    raw.get(key)
        .and_then(Value::as_str)
        .map(clean_line)
        .unwrap_or_default()
}

fn str_array(raw: &Value, key: &str) -> Vec<String> {
    raw.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(clean_line)
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn to_value<T: Serialize>(t: &T) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
}

fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}

// --- Status ---

pub fn status(cfg: &AppConfig) -> Status {
    let web_lookup = searxng::load_or_init()
        .map(|c| !c.base_url.trim().is_empty())
        .unwrap_or(false);
    let test_photo = comfyui::load_or_init()
        .map(|c| !c.base_url.trim().is_empty() && !c.workflow_json.trim().is_empty())
        .unwrap_or(false);
    Status {
        tag_source: danbooru::tag_source(cfg),
        online_lookup: cfg.danbooru_online_lookup,
        web_lookup,
        test_photo,
    }
}

// --- Cancel registry ---

fn registry() -> &'static Mutex<HashMap<String, (u64, CancelToken)>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, (u64, CancelToken)>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

fn lock_registry() -> std::sync::MutexGuard<'static, HashMap<String, (u64, CancelToken)>> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// Registers a build id for `stop`; unregisters it on drop.
struct BuildGuard {
    build_id: String,
    seq: u64,
    cancel: CancelToken,
}

impl BuildGuard {
    fn register(build_id: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let cancel = CancelToken::new();
        lock_registry().insert(build_id.to_string(), (seq, cancel.clone()));
        Self {
            build_id: build_id.to_string(),
            seq,
            cancel,
        }
    }
}

impl Drop for BuildGuard {
    fn drop(&mut self) {
        let mut map = lock_registry();
        if map.get(&self.build_id).is_some_and(|(s, _)| *s == self.seq) {
            map.remove(&self.build_id);
        }
    }
}

/// Stops the running call of this build; `false` when none is running.
pub fn stop(build_id: &str) -> bool {
    match lock_registry().get(build_id) {
        Some((_, cancel)) => {
            cancel.cancel();
            true
        }
        None => false,
    }
}

// --- Model calls ---

enum Reply {
    Value(Value),
    Stopped,
    Failed(String),
}

#[allow(clippy::too_many_arguments)]
async fn ask(
    cfg: &AppConfig,
    cancel: &CancelToken,
    system: &str,
    user: String,
    temperature: f32,
    max_tokens: u32,
    schema_name: &str,
    schema: &Value,
) -> Reply {
    if cancel.is_cancelled() {
        return Reply::Stopped;
    }
    let messages = [
        ChatMessage::text("system", system),
        ChatMessage::text("user", user),
    ];
    let call = llm::send_chat_json(
        &cfg.endpoint,
        &cfg.model,
        &cfg.api_key,
        &messages,
        temperature,
        Some(max_tokens),
        schema_name,
        schema,
    );
    tokio::select! {
        _ = cancel.cancelled() => Reply::Stopped,
        r = tokio::time::timeout(CALL_TIMEOUT, call) => match r {
            Ok(Ok(v)) => Reply::Value(v),
            Ok(Err(e)) => Reply::Failed(format!("{e:#}")),
            Err(_) => Reply::Failed("the model took too long".to_string()),
        },
    }
}

// --- Research ---

/// Up to five web snippets about the character; empty for an original
/// character or when no SearXNG instance is configured. Stop-aware.
pub async fn research(
    _cfg: &AppConfig,
    build_id: &str,
    brief: &Brief,
) -> anyhow::Result<Vec<Snippet>> {
    let brief = brief.clone().sanitized();
    if brief.is_original() || brief.name.is_empty() {
        return Ok(Vec::new());
    }
    let search_cfg = searxng::load_or_init()?;
    if search_cfg.base_url.trim().is_empty() {
        return Ok(Vec::new());
    }
    let guard = BuildGuard::register(build_id);
    let query = format!(
        "\"{}\" {} character personality appearance",
        brief.name, brief.source
    );
    let results = tokio::select! {
        _ = guard.cancel.cancelled() => return Ok(Vec::new()),
        r = searxng::search(&search_cfg, &query) => r?,
    };
    let mut total = 0;
    let mut out = Vec::new();
    for r in results.into_iter().take(SNIPPET_COUNT) {
        let room = SNIPPETS_TOTAL_CHARS.saturating_sub(total);
        if room == 0 {
            break;
        }
        let content = cap(&single_line(&r.content), SNIPPET_CHARS.min(room));
        total += content.chars().count();
        out.push(Snippet {
            title: cap(&single_line(&r.title), 120),
            url: r.url,
            content,
        });
    }
    Ok(out)
}

// --- Tag suggestions ---

/// Co-occurrence group of a body tag: only the top tag of each group is kept
/// (one hair colour, one hair length, one eye colour, one breast size).
fn body_group(name: &str) -> String {
    const HAIR_LENGTHS: [&str; 5] = [
        "short_hair",
        "medium_hair",
        "long_hair",
        "very_long_hair",
        "absurdly_long_hair",
    ];
    const COLORS: [&str; 20] = [
        "black",
        "brown",
        "blonde",
        "red",
        "pink",
        "blue",
        "green",
        "purple",
        "white",
        "silver",
        "grey",
        "gray",
        "orange",
        "aqua",
        "multicolored",
        "two-tone",
        "gradient",
        "streaked",
        "colored",
        "light",
    ];
    if HAIR_LENGTHS.contains(&name) {
        "hair_length".to_string()
    } else if name.ends_with("_hair") {
        let first = name.split('_').next().unwrap_or("");
        if COLORS.contains(&first) {
            "hair_color".to_string()
        } else {
            name.to_string()
        }
    } else if name.ends_with("_eyes") {
        "eyes".to_string()
    } else if name == "flat_chest" || name.ends_with("_breasts") {
        "breasts".to_string()
    } else {
        name.to_string()
    }
}

/// The first parenthesised group of a tag, spaced: `x_(swimsuit_berserker)_(fate)`
/// gives "swimsuit berserker".
fn first_group(name: &str) -> Option<String> {
    let start = name.find('(')?;
    let end = name[start..].find(')')? + start;
    let group = name[start + 1..end].replace('_', " ");
    (!group.trim().is_empty()).then_some(group)
}

fn build_suggestion(
    data: &danbooru::TagData,
    character_tag: &str,
) -> anyhow::Result<Option<Suggestion>> {
    let Some((info, _)) = data.resolve(character_tag) else {
        return Ok(None);
    };
    let Some(general) = data.general_character(&info.name) else {
        return Ok(None);
    };
    let variants = data.variants_of(&general.name, 3);
    let mut wanted = vec![general.name.clone()];
    wanted.extend(variants.iter().map(|v| v.name.clone()));
    let cooc = data.cooccurring(&wanted)?;

    let ratio = |count: u64, of: u64| {
        if of == 0 {
            0.0
        } else {
            count as f64 / of as f64
        }
    };

    let mut body_tags = Vec::new();
    let mut groups = HashSet::new();
    for (tag, count) in cooc.get(&general.name).map(Vec::as_slice).unwrap_or(&[]) {
        if body_tags.len() >= MAX_BODY_TAGS {
            break;
        }
        if tag == "breasts"
            || ratio(*count, general.count) < BODY_TAG_RATIO
            || danbooru::classify(tag) != TagClass::Body
        {
            continue;
        }
        if groups.insert(body_group(tag)) {
            body_tags.push(danbooru::display(tag));
        }
    }

    let mut outfits = Vec::new();
    for variant in &variants {
        let Some(name) = first_group(&variant.name) else {
            continue;
        };
        let items: Vec<String> = cooc
            .get(&variant.name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .filter(|(tag, count)| {
                ratio(*count, variant.count) >= OUTFIT_TAG_RATIO
                    && danbooru::classify(tag) == TagClass::Outfit
            })
            .take(MAX_OUTFIT_ITEMS)
            .map(|(tag, _)| danbooru::display(tag))
            .collect();
        if !items.is_empty() {
            outfits.push(Outfit { name, items });
        }
    }
    Ok(Some(Suggestion {
        character: general,
        body_tags,
        outfits,
    }))
}

/// Body tags and outfits for a character tag from the local tag data; `None`
/// when there is no local layer or the tag is not a known character.
pub async fn suggest(cfg: &AppConfig, character_tag: &str) -> anyhow::Result<Option<Suggestion>> {
    let cfg = cfg.clone();
    let tag = character_tag.to_string();
    tokio::task::spawn_blocking(move || {
        let Some(data) = danbooru::TagData::open(&cfg) else {
            return Ok(None);
        };
        build_suggestion(&data, &tag)
    })
    .await?
}

// --- Sections: schemas and prompts ---

fn temperature(section: Section) -> f32 {
    match section {
        Section::Identity | Section::Appearance => 0.2,
        _ => 0.7,
    }
}

fn max_tokens(section: Section) -> u32 {
    match section {
        Section::Identity => 220,
        Section::Appearance => 150,
        Section::Wardrobe => 450,
        Section::Personality => 400,
        Section::PhotoHabit => 120,
        Section::Stats => 300,
        Section::Rules => 350,
    }
}

fn schema_name(section: Section) -> &'static str {
    match section {
        Section::Identity => "persona_identity",
        Section::Appearance => "persona_appearance",
        Section::Wardrobe => "persona_wardrobe",
        Section::Personality => "persona_personality",
        Section::PhotoHabit => "persona_photo_habit",
        Section::Stats => "persona_stats",
        Section::Rules => "persona_rules",
    }
}

fn strings_schema(max: usize) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "maxItems": max })
}

fn schema(section: Section, tags_style: bool) -> Value {
    let (properties, required): (Value, &[&str]) = match section {
        Section::Identity => (
            json!({
                "summary": { "type": "string" },
                "gender": { "type": "string", "enum": ["female", "male", "other"] },
                "speaks_as": { "type": "string" },
                "calls_user": { "type": "string" },
                "short_name": { "type": "string" },
            }),
            &["summary", "gender", "speaks_as", "calls_user", "short_name"],
        ),
        Section::Appearance if tags_style => (
            json!({
                "hair_color": { "type": "string" },
                "eye_color": { "type": "string" },
                "hair_length": { "type": "string", "enum": ["short", "medium", "long", "very long"] },
                "build": { "type": "string", "enum": [
                    "flat chest", "small breasts", "medium breasts", "large breasts",
                    "muscular", "slim", "none"
                ] },
                "extra": strings_schema(4),
            }),
            &["hair_color", "eye_color", "hair_length", "build", "extra"],
        ),
        Section::Appearance => (
            json!({ "description": { "type": "string" } }),
            &["description"],
        ),
        Section::Wardrobe => (
            json!({
                "outfits": {
                    "type": "array",
                    "minItems": 3,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "items": strings_schema(MAX_OUTFIT_ITEMS),
                        },
                        "required": ["name", "items"],
                        "additionalProperties": false,
                    },
                },
            }),
            &["outfits"],
        ),
        Section::Personality => (
            json!({
                "traits": { "type": "array", "items": { "type": "string" }, "minItems": 4, "maxItems": 6 },
            }),
            &["traits"],
        ),
        Section::PhotoHabit => (json!({ "reason": { "type": "string" } }), &["reason"]),
        Section::Stats => (
            json!({
                "stats": {
                    "type": "array",
                    "maxItems": 3,
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "description": { "type": "string" },
                        },
                        "required": ["name", "description"],
                        "additionalProperties": false,
                    },
                },
            }),
            &["stats"],
        ),
        Section::Rules => (
            json!({
                "mood": { "type": "string" },
                "affection_mid": { "type": "string" },
                "affection_high": { "type": "string" },
                "extra": strings_schema(3),
            }),
            &["mood", "affection_mid", "affection_high", "extra"],
        ),
    };
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn system_prompt(section: Section, tags_style: bool) -> &'static str {
    match section {
        Section::Identity => {
            "You fill in the identity fields of a roleplay character sheet. \
             summary: one or two sentences about who the character is, under 200 characters. \
             speaks_as: how the character talks, a few words. \
             calls_user: what the character calls the user. \
             short_name: the one word of the name used in actions."
        }
        Section::Appearance if tags_style => {
            "You describe a character's body for an image generator: hair color, eye color, \
             hair length and build. One plain word or phrase per field. No clothing."
        }
        Section::Appearance => {
            "You describe a character's looks in one short sentence for an image generator: \
             hair, eyes, build. No clothing."
        }
        Section::Wardrobe => {
            "You list the outfits a character wears. Three or four outfits, each with a short \
             name and its garments and accessories, with colors. Short items, no sentences."
        }
        Section::Personality => {
            "You list personality traits of a roleplay character as four to six short bullet \
             points. Each is one concrete sentence about how the character behaves."
        }
        Section::PhotoHabit => {
            "In one short sentence, say why or when this character sends photos of herself or \
             himself in chat."
        }
        Section::Stats => {
            "You suggest up to three extra stats to track for this character, beyond the \
             standard ones. Each has a short name (at most three words) and a one-line \
             description of what to track. An empty list is fine."
        }
        Section::Rules => {
            "You write short behavior rules for a roleplay character. mood: how the mood \
             changes with the story. affection_mid: how the character acts at medium \
             affection. affection_high: how the character acts at high affection. extra: up to \
             three more character-specific rules. One sentence each."
        }
    }
}

fn user_prompt(brief: &Brief, section: Section, draft: &Draft, snippets: &[Snippet]) -> String {
    let mut lines = vec![format!("Character name: {}", brief.name)];
    if brief.is_original() {
        lines.push("Source: original character".to_string());
    } else {
        lines.push(format!("Source: {}", brief.source));
    }
    if !brief.vibe.is_empty() {
        lines.push(format!("Vibe: {}", brief.vibe));
    }
    lines.push(format!("Content level: {}", brief.content_level.label()));

    let mut context = String::new();
    if !draft.identity.summary.is_empty() {
        context.push_str(&format!("Summary: {}\n", draft.identity.summary));
    }
    for t in &draft.personality {
        context.push_str(&format!("- {t}\n"));
    }
    if !context.is_empty() && section != Section::Identity {
        lines.push(format!(
            "Character so far:\n{}",
            cap(context.trim_end(), CONTEXT_CHARS)
        ));
    }
    if matches!(
        section,
        Section::Identity | Section::Personality | Section::Wardrobe
    ) && !snippets.is_empty()
    {
        lines.push(
            "Reference notes from a web search. They may be wrong or irrelevant; use only facts \
             about the character."
                .to_string(),
        );
        for s in snippets {
            lines.push(format!("- {}: {}", s.title, s.content));
        }
    }
    lines.join("\n")
}

// --- Sections: checks and assembly of the model's answer ---

fn gender_from(raw: &Value) -> Gender {
    raw.get("gender")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

/// The word of `name` the model picked as short name, else the full name.
fn pick_short_name(candidate: &str, name: &str) -> String {
    let trim = |s: &str| {
        s.trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase()
    };
    let wanted = trim(candidate);
    name.split_whitespace()
        .find(|w| !wanted.is_empty() && trim(w) == wanted)
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .unwrap_or_else(|| name.to_string())
}

fn finish_identity(raw: &Value, brief: &Brief) -> Option<Value> {
    let summary = cap(&str_field(raw, "summary"), 200);
    if summary.is_empty() {
        return None;
    }
    let calls_user = str_field(raw, "calls_user");
    Some(to_value(&Identity {
        summary,
        gender: gender_from(raw),
        speaks_as: cap(&str_field(raw, "speaks_as"), 160),
        calls_user: if calls_user.is_empty() {
            "you".to_string()
        } else {
            cap(&calls_user, 40)
        },
        short_name: pick_short_name(&str_field(raw, "short_name"), &brief.name),
    }))
}

fn strip_suffix_word(s: &str, word: &str) -> String {
    let lower = s.to_lowercase();
    match lower.strip_suffix(word) {
        Some(rest) if rest.ends_with(' ') => rest.trim().to_string(),
        _ => lower,
    }
}

/// Body tags composed by code from the model's slots; clothing tags are dropped.
fn compose_body_tags(raw: &Value) -> Vec<String> {
    let mut tags = Vec::new();
    let hair_color = strip_suffix_word(&str_field(raw, "hair_color"), "hair");
    if !hair_color.is_empty() {
        tags.push(format!("{hair_color} hair"));
    }
    let length = str_field(raw, "hair_length").to_lowercase();
    if !length.is_empty() {
        tags.push(format!("{length} hair"));
    }
    let eye_color = strip_suffix_word(&str_field(raw, "eye_color"), "eyes");
    if !eye_color.is_empty() {
        tags.push(format!("{eye_color} eyes"));
    }
    let build = str_field(raw, "build").to_lowercase();
    if !build.is_empty() && build != "none" {
        tags.push(build);
    }
    tags.extend(str_array(raw, "extra").into_iter().take(4));
    dedup_ci(
        tags.into_iter()
            .map(|t| t.to_lowercase())
            .filter(|t| danbooru::classify(&danbooru::normalize(t)) != TagClass::Outfit)
            .collect(),
    )
}

fn finish_appearance(raw: &Value, tags_style: bool, draft: &Draft) -> Option<Value> {
    let mut appearance = draft.appearance.clone();
    if tags_style {
        let tags = compose_body_tags(raw);
        if tags.is_empty() {
            return None;
        }
        appearance.tags = tags;
    } else {
        let description = cap(&str_field(raw, "description"), 300);
        if description.is_empty() {
            return None;
        }
        appearance.description = description;
    }
    Some(to_value(&appearance))
}

fn finish_wardrobe(raw: &Value) -> Option<Value> {
    let outfits = raw.get("outfits")?.as_array()?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for o in outfits {
        let name = cap(&str_field(o, "name"), 40);
        if name.is_empty() || !seen.insert(name.to_lowercase()) {
            continue;
        }
        let items = dedup_ci(str_array(o, "items"))
            .into_iter()
            .take(MAX_OUTFIT_ITEMS)
            .map(|i| cap(&i, 80))
            .collect();
        out.push(Outfit { name, items });
        if out.len() >= MAX_OUTFITS {
            break;
        }
    }
    (!out.is_empty()).then(|| to_value(&out))
}

fn finish_personality(raw: &Value) -> Option<Value> {
    let traits: Vec<String> = dedup_ci(
        str_array(raw, "traits")
            .into_iter()
            .map(|t| cap(&t, 160))
            .collect(),
    )
    .into_iter()
    .take(MAX_TRAITS)
    .collect();
    (traits.len() >= 2).then(|| to_value(&traits))
}

fn finish_photo_reason(raw: &Value) -> Option<Value> {
    let reason = cap(&str_field(raw, "reason"), 240);
    (!reason.is_empty()).then_some(Value::String(reason))
}

fn finish_stats(raw: &Value) -> Option<Value> {
    let stats = raw.get("stats")?.as_array()?;
    let mut seen: HashSet<String> = BASE_STATS.iter().map(|s| s.to_lowercase()).collect();
    let mut out = Vec::new();
    for s in stats {
        let name = single_line(&str_field(s, "name"))
            .split_whitespace()
            .take(3)
            .map(title_case)
            .collect::<Vec<_>>()
            .join(" ");
        let description = cap(&str_field(s, "description"), 160);
        if name.is_empty() || description.is_empty() || !seen.insert(name.to_lowercase()) {
            continue;
        }
        out.push(Stat { name, description });
        if out.len() >= 3 {
            break;
        }
    }
    Some(to_value(&out))
}

fn finish_rules(raw: &Value) -> Option<Value> {
    let rules = Rules {
        mood: cap(&str_field(raw, "mood"), 200),
        affection_mid: cap(&str_field(raw, "affection_mid"), 200),
        affection_high: cap(&str_field(raw, "affection_high"), 200),
        extra: str_array(raw, "extra")
            .into_iter()
            .take(3)
            .map(|r| cap(&r, 200))
            .collect(),
    };
    let empty =
        rules.mood.is_empty() && rules.affection_mid.is_empty() && rules.affection_high.is_empty();
    (!empty).then(|| to_value(&rules))
}

/// The model's raw answer as the `Draft` field value, `None` when unusable.
fn finish_section(section: Section, raw: &Value, brief: &Brief, draft: &Draft) -> Option<Value> {
    match section {
        Section::Identity => finish_identity(raw, brief),
        Section::Appearance => {
            finish_appearance(raw, brief.appearance_style == AppearanceStyle::Tags, draft)
        }
        Section::Wardrobe => finish_wardrobe(raw),
        Section::Personality => finish_personality(raw),
        Section::PhotoHabit => finish_photo_reason(raw),
        Section::Stats => finish_stats(raw),
        Section::Rules => finish_rules(raw),
    }
}

/// Deterministic value used when the model gave nothing usable.
fn fallback_value(section: Section, brief: &Brief, draft: &Draft) -> Value {
    match section {
        Section::Identity => to_value(&Identity {
            short_name: brief.name.clone(),
            ..Default::default()
        }),
        Section::Appearance => to_value(&Appearance {
            character_tag: draft.appearance.character_tag.clone(),
            ..Default::default()
        }),
        Section::Wardrobe => to_value(&vec![Outfit {
            name: "Casual".to_string(),
            items: Vec::new(),
        }]),
        Section::Personality => to_value(&Vec::<String>::new()),
        Section::PhotoHabit => Value::String(String::new()),
        Section::Stats => to_value(&Vec::<Stat>::new()),
        Section::Rules => to_value(&Rules::default()),
    }
}

/// The draft's current value of a section (returned untouched on Stop).
fn draft_value(section: Section, draft: &Draft) -> Value {
    match section {
        Section::Identity => to_value(&draft.identity),
        Section::Appearance => to_value(&draft.appearance),
        Section::Wardrobe => to_value(&draft.wardrobe),
        Section::Personality => to_value(&draft.personality),
        Section::PhotoHabit => Value::String(draft.photo_reason.clone()),
        Section::Stats => to_value(&draft.extra_stats),
        Section::Rules => to_value(&draft.rules),
    }
}

/// Data outfits first, then the model's, no duplicate names, at most five.
fn merge_outfits(data: Vec<Outfit>, generated: Vec<Outfit>) -> Vec<Outfit> {
    let mut seen = HashSet::new();
    data.into_iter()
        .chain(generated)
        .filter(|o| seen.insert(o.name.to_lowercase()))
        .take(MAX_OUTFITS)
        .collect()
}

/// Checks `tags`; found tags become their canonical display form, the rest
/// are kept (and flagged by the returned checks), never dropped.
async fn canonicalize_tags(cfg: &AppConfig, tags: Vec<String>) -> (Vec<String>, Vec<TagCheck>) {
    if tags.is_empty() {
        return (tags, Vec::new());
    }
    let (_, checks) = danbooru::check_tags(cfg, &tags).await;
    let found: HashMap<&str, &str> = checks
        .iter()
        .filter(|c| {
            matches!(
                c.status,
                TagStatus::Verified | TagStatus::Alias | TagStatus::Online
            )
        })
        .map(|c| (c.input.trim(), c.display.as_str()))
        .collect();
    let out = tags
        .iter()
        .map(|t| found.get(t.trim()).map_or_else(|| t.trim(), |d| d))
        .map(str::to_string)
        .collect();
    (dedup_ci(out), checks)
}

fn outcome(section: Section, value: Value) -> SectionOutcome {
    SectionOutcome {
        section,
        value,
        fallback: false,
        stopped: false,
        warning: None,
        tag_checks: Vec::new(),
    }
}

/// One section by one model call (one retry, then the fallback), with the
/// tag checks of Tags style applied to appearance and wardrobe.
pub async fn generate_section(
    cfg: &AppConfig,
    build_id: &str,
    brief: &Brief,
    section: Section,
    draft: &Draft,
    snippets: &[Snippet],
) -> SectionOutcome {
    let brief = brief.clone().sanitized();
    let guard = BuildGuard::register(build_id);
    let tags_style = brief.appearance_style == AppearanceStyle::Tags;
    let character_tag = draft.appearance.character_tag.trim();

    let suggestion = if tags_style
        && matches!(section, Section::Appearance | Section::Wardrobe)
        && !character_tag.is_empty()
    {
        suggest(cfg, character_tag).await.unwrap_or_else(|e| {
            log::warn!("persona builder: tag suggestions failed: {e:#}");
            None
        })
    } else {
        None
    };

    let mut value: Option<Value> = None;
    let mut warning: Option<String> = None;
    let data_body_tags = suggestion
        .as_ref()
        .filter(|_| section == Section::Appearance)
        .map(|s| s.body_tags.clone())
        .filter(|t| !t.is_empty());
    if let Some(tags) = data_body_tags {
        let mut appearance = draft.appearance.clone();
        appearance.tags = tags;
        value = Some(to_value(&appearance));
    } else {
        let schema = schema(section, tags_style);
        for _ in 0..2 {
            let reply = ask(
                cfg,
                &guard.cancel,
                system_prompt(section, tags_style),
                user_prompt(&brief, section, draft, snippets),
                temperature(section),
                max_tokens(section),
                schema_name(section),
                &schema,
            )
            .await;
            match reply {
                Reply::Stopped => {
                    return SectionOutcome {
                        stopped: true,
                        ..outcome(section, draft_value(section, draft))
                    };
                }
                Reply::Value(raw) => match finish_section(section, &raw, &brief, draft) {
                    Some(v) => {
                        value = Some(v);
                        warning = None;
                        break;
                    }
                    None => {
                        warning = Some("the model's answer was not usable".to_string());
                    }
                },
                Reply::Failed(e) => {
                    log::warn!("persona builder: {section:?} call failed: {e}");
                    warning = Some(e);
                }
            }
        }
    }

    let fallback = value.is_none();
    let mut value = value.unwrap_or_else(|| fallback_value(section, &brief, draft));
    if section == Section::Wardrobe {
        let data = suggestion.map(|s| s.outfits).unwrap_or_default();
        let generated: Vec<Outfit> = serde_json::from_value(value.clone()).unwrap_or_default();
        let generated = if fallback && !data.is_empty() {
            Vec::new()
        } else {
            generated
        };
        value = to_value(&merge_outfits(data, generated));
    }

    let mut tag_checks = Vec::new();
    if tags_style && section == Section::Appearance {
        let mut appearance: Appearance = serde_json::from_value(value.clone()).unwrap_or_default();
        let (tags, checks) = canonicalize_tags(cfg, appearance.tags).await;
        appearance.tags = tags;
        value = to_value(&appearance);
        tag_checks = checks;
    } else if tags_style && section == Section::Wardrobe {
        let mut outfits: Vec<Outfit> = serde_json::from_value(value.clone()).unwrap_or_default();
        let all: Vec<String> = outfits.iter().flat_map(|o| o.items.clone()).collect();
        let (_, checks) = if all.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            canonicalize_tags(cfg, all).await
        };
        for o in &mut outfits {
            o.items = canonicalize_with(&o.items, &checks);
        }
        value = to_value(&outfits);
        tag_checks = checks;
    }

    SectionOutcome {
        fallback,
        warning: fallback.then(|| warning.unwrap_or_else(|| "used a default".to_string())),
        tag_checks,
        ..outcome(section, value)
    }
}

/// Replaces found tags with their display form using already computed checks.
fn canonicalize_with(items: &[String], checks: &[TagCheck]) -> Vec<String> {
    let found: HashMap<&str, &str> = checks
        .iter()
        .filter(|c| {
            matches!(
                c.status,
                TagStatus::Verified | TagStatus::Alias | TagStatus::Online
            )
        })
        .map(|c| (c.input.trim(), c.display.as_str()))
        .collect();
    dedup_ci(
        items
            .iter()
            .map(|t| {
                found
                    .get(t.trim())
                    .map_or_else(|| t.trim(), |d| d)
                    .to_string()
            })
            .collect(),
    )
}

// --- Card template ---

/// A tag as it goes into the card: display form, except weighted tags and
/// `lora:` / `embedding:` entries, which pass through untouched.
fn render_tag(raw: &str) -> String {
    let t = raw.trim();
    let lower = t.to_lowercase();
    let weighted = t.starts_with('(') && t.ends_with(')') && t.contains(':');
    if weighted || lower.starts_with("lora:") || lower.starts_with("embedding:") {
        t.to_string()
    } else {
        danbooru::display(&danbooru::normalize(t))
    }
}

pub fn render(brief: &Brief, draft: &Draft) -> String {
    let brief = brief.clone().sanitized();
    let tags_style = brief.appearance_style == AppearanceStyle::Tags;
    let gender = draft.identity.gender;
    let p = gender.pronouns();
    let (her, she, is, s) = (p.her, p.she, p.is, p.s);
    let name = brief.name.as_str();
    let calls_user = match draft.identity.calls_user.trim() {
        "" => "you",
        c => c,
    };
    let mut sections: Vec<String> = Vec::new();

    // Identity
    let summary = draft.identity.summary.trim();
    let mut character = if brief.is_original() {
        format!("Character: {name}, an original character.")
    } else {
        format!("Character: {name} from {}.", brief.source)
    };
    if !summary.is_empty() {
        character.push(' ');
        character.push_str(summary);
    }
    let speaks = draft.identity.speaks_as.trim();
    let speaks_line = if speaks.is_empty() {
        format!("Calls the user \"{calls_user}\".")
    } else {
        format!("Speaks as: {speaks}; calls the user \"{calls_user}\".")
    };
    sections.push(format!(
        "Identity:\n{character}\nGender: {}, adult.\n{speaks_line}",
        gender.title()
    ));

    // Appearance
    let short = draft.identity.short_name.trim();
    let action_line = if short.is_empty() || short == name {
        format!("In actions, call {her} \"{name}\".")
    } else {
        format!("In actions, call {her} \"{short}\", never {her} full name.")
    };
    let appearance = if tags_style {
        let mut parts: Vec<String> = Vec::new();
        if !draft.appearance.character_tag.trim().is_empty() {
            parts.push(render_tag(&draft.appearance.character_tag));
        }
        parts.extend(gender.photo_tag().map(str::to_string));
        parts.extend(
            draft
                .appearance
                .tags
                .iter()
                .filter(|t| !t.trim().is_empty())
                .map(|t| render_tag(t)),
        );
        let mut text = String::from("Appearance (use these tags for photos, always the same):\n");
        if !parts.is_empty() {
            text.push_str(&format!("{}.\n", parts.join(", ")));
        }
        text.push_str(&action_line);
        text
    } else {
        let mut text =
            format!("Appearance (describe {her} like this in photos, always the same):\n");
        if !draft.appearance.description.trim().is_empty() {
            text.push_str(&format!("{}\n", draft.appearance.description.trim()));
        }
        text.push_str(&action_line);
        text
    };
    sections.push(appearance);

    // Wardrobe
    let mut wardrobe =
        format!("Wardrobe (photos use the CURRENT outfit from {her} state, never a fixed one):");
    for o in &draft.wardrobe {
        let items: Vec<String> = o
            .items
            .iter()
            .filter(|i| !i.trim().is_empty())
            .map(|i| {
                if tags_style {
                    render_tag(i)
                } else {
                    i.trim().to_string()
                }
            })
            .collect();
        if items.is_empty() {
            wardrobe.push_str(&format!("\n- {}", o.name.trim()));
        } else {
            wardrobe.push_str(&format!("\n- {}: {}.", o.name.trim(), items.join(", ")));
        }
    }
    sections.push(wardrobe);

    // Personality
    let mut personality = String::from("Personality:");
    for t in draft.personality.iter().filter(|t| !t.trim().is_empty()) {
        personality.push_str(&format!("\n- {}", t.trim()));
    }
    sections.push(personality);

    // Photo habit
    sections.push(
        format!(
            "Photo habit: {}. {}",
            brief.photo_habit.word(),
            draft.photo_reason.trim()
        )
        .trim_end()
        .to_string(),
    );

    // Stats
    let mut stats = format!(
        "Stats (update every one of them every turn):\n\
         Outfit: what {she} {is} wearing right now, with colors and accessories.\n\
         Clothing State: how it is worn right now (neat, wet, loosened, etc.).\n\
         Location: where {she} {is} right now.\n\
         Activity: what {her} body is doing right now (pose and action).\n\
         Mood: one or two emotion words for {her} face right now.\n\
         Hair: how {her} hair looks right now.\n\
         Affection: 0-100%, how close {she} feel{s} to {calls_user}.\n\
         Mind: a short note of what {she} {is} thinking about."
    );
    for st in &draft.extra_stats {
        if !st.name.trim().is_empty() {
            stats.push_str(&format!("\n{}: {}", st.name.trim(), st.description.trim()));
        }
    }
    sections.push(stats);

    // Behavior rules
    let mut rules = vec![
        format!(
            "Write {her} actions as *action*, {her} inner thoughts as (thought), and {her} speech as plain text in quotes."
        ),
        "Keep replies short: 2-5 sentences.".to_string(),
        "Change Location, Activity and Outfit when the story moves; never keep a stale scene."
            .to_string(),
    ];
    let generated = [
        &draft.rules.mood,
        &draft.rules.affection_mid,
        &draft.rules.affection_high,
    ];
    rules.extend(generated.iter().map(|r| r.trim().to_string()));
    rules.push(format!(
        "When {she} send{s} a photo, {she} say{s} so in {her} reply."
    ));
    rules.push(brief.content_level.rule_line().to_string());
    rules.extend(draft.rules.extra.iter().map(|r| r.trim().to_string()));
    let mut text = String::from("Behavior rules:");
    for r in rules.iter().filter(|r| !r.is_empty()) {
        text.push_str(&format!("\n- {r}"));
    }
    sections.push(text);

    format!("{}\n", sections.join("\n\n"))
}

// --- Test photo ---

/// Flat state JSON for the image prompt builder: the first outfit, a plain scene.
pub fn test_photo_state(_brief: &Brief, draft: &Draft) -> String {
    let outfit = draft
        .wardrobe
        .first()
        .map(|o| o.items.join(", "))
        .unwrap_or_default();
    json!({
        "outfit": outfit,
        "location": "plain room",
        "activity": "standing, looking at the viewer",
        "mood": "smiling",
    })
    .to_string()
}

/// The image prompt for a test photo, through the normal image prompt
/// builder (ruleset and deterministic fallback included); `None` when stopped.
pub async fn test_photo_fields(
    cfg: &AppConfig,
    build_id: &str,
    brief: &Brief,
    draft: &Draft,
) -> Option<comfyui::ImagePromptFields> {
    let brief = brief.clone().sanitized();
    let guard = BuildGuard::register(build_id);
    let card = render(&brief, draft);
    let state = test_photo_state(&brief, draft);
    let style_rules =
        ruleset::load_ruleset(ruleset::IMAGE_GENERATION_RULESET_NAME).unwrap_or_default();
    let name = (!brief.name.is_empty()).then_some(brief.name.as_str());
    let input = director::BuilderInput {
        persona_name: name,
        persona: Some(&card),
        raw_state: &state,
        history: &[],
        photo_reason: "",
        hint: Some(TEST_PHOTO_HINT),
        style_rules: &style_rules,
        previous_prompt: None,
    };
    let call = director::build_image_prompt(cfg, &input);
    tokio::select! {
        _ = guard.cancel.cancelled() => None,
        fields = call => (!guard.cancel.is_cancelled()).then_some(fields),
    }
}

// --- Saving and the sidecar draft ---

fn sidecar_dir() -> PathBuf {
    persona::personas_dir().join(".builder")
}

/// `<name>` must already be a sanitized persona name: the path guard.
fn sidecar_path(name: &str) -> anyhow::Result<PathBuf> {
    if name.is_empty() || sanitize_filename(name, "persona") != name {
        anyhow::bail!("\"{name}\" is not a valid persona name");
    }
    Ok(sidecar_dir().join(format!("{name}.json")))
}

fn write_sidecar(name: &str, brief: &Brief, draft: &Draft, content: &str) -> anyhow::Result<()> {
    let path = sidecar_path(name)?;
    std::fs::create_dir_all(sidecar_dir())?;
    let saved = SavedBuild {
        version: SIDECAR_VERSION,
        persona: name.to_string(),
        brief: brief.clone(),
        draft: draft.clone(),
        content: content.to_string(),
        hand_edited: false,
    };
    write_atomic(
        &path,
        serde_json::to_string_pretty(&saved)?.as_bytes(),
        None,
    )?;
    Ok(())
}

/// Saves the card (and its sidecar draft). The card is the product: a
/// sidecar failure after it was written is logged, not returned.
pub fn save(
    mode: SaveMode,
    persona_name: &str,
    brief: &Brief,
    draft: &Draft,
    content: &str,
) -> anyhow::Result<persona::PersonaSummary> {
    let brief = brief.clone().sanitized();
    let summary = match mode {
        SaveMode::Create => persona::save_new_persona(&brief.name, content)?,
        SaveMode::Update => {
            let path = sidecar_path(persona_name)?;
            if !path.exists() {
                anyhow::bail!("\"{persona_name}\" was not made with the builder");
            }
            persona::update_persona(persona_name, content)?;
            persona::PersonaSummary {
                name: persona_name.to_string(),
            }
        }
    };
    if let Err(e) = write_sidecar(&summary.name, &brief, draft, content) {
        log::warn!("persona builder: couldn't save the draft next to the card: {e:#}");
    }
    Ok(summary)
}

/// The saved draft of a builder-made persona; `None` when there is none, the
/// persona is gone or the sidecar can't be read.
pub fn load(persona_name: &str) -> anyhow::Result<Option<SavedBuild>> {
    let Ok(path) = sidecar_path(persona_name) else {
        return Ok(None);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let Ok(card) = persona::load_persona(persona_name) else {
        return Ok(None);
    };
    let mut saved: SavedBuild = match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("persona builder: unreadable draft for \"{persona_name}\": {e}");
            return Ok(None);
        }
    };
    if saved.version != SIDECAR_VERSION {
        log::warn!(
            "persona builder: draft of \"{persona_name}\" has version {}, expected {SIDECAR_VERSION}",
            saved.version
        );
        return Ok(None);
    }
    saved.hand_edited = card != saved.content;
    Ok(Some(saved))
}

/// Best effort; called when a persona is deleted.
pub fn remove_sidecar(persona_name: &str) {
    if let Ok(path) = sidecar_path(persona_name) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-assistant-pbuilder-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        persona::set_test_personas_dir(dir.join("personas"));
        dir
    }

    fn musashi_brief() -> Brief {
        Brief {
            name: "Miyamoto Musashi".into(),
            source: "Fate".into(),
            ..Default::default()
        }
    }

    fn musashi_draft() -> Draft {
        Draft {
            identity: Identity {
                summary: "A carefree swordswoman.".into(),
                gender: Gender::Female,
                speaks_as: "casual and playful".into(),
                calls_user: "Master".into(),
                short_name: "Musashi".into(),
            },
            appearance: Appearance {
                character_tag: "miyamoto_musashi_(fate)".into(),
                tags: vec!["pink hair".into(), "blue eyes".into()],
                description: String::new(),
            },
            wardrobe: vec![
                Outfit {
                    name: "Swimsuit".into(),
                    items: vec!["american_flag_bikini".into(), "bun_cover".into()],
                },
                Outfit {
                    name: "Casual".into(),
                    items: vec![],
                },
            ],
            personality: vec!["Cheerful and blunt".into(), "Loves a good duel".into()],
            photo_reason: "She likes showing off new outfits.".into(),
            extra_stats: vec![Stat {
                name: "Sword Focus".into(),
                description: "how sharp her focus is".into(),
            }],
            rules: Rules {
                mood: "Her mood shifts with the story.".into(),
                affection_mid: "At medium affection she teases.".into(),
                affection_high: "At high affection she is bold.".into(),
                extra: vec!["Never forgets her swords.".into()],
            },
        }
    }

    fn photo_brief() -> Brief {
        Brief {
            photo_habit: PhotoHabit::Often,
            ..musashi_brief()
        }
    }

    fn pos(text: &str, needle: &str) -> usize {
        text.find(needle)
            .unwrap_or_else(|| panic!("missing {needle:?} in:\n{text}"))
    }

    #[test]
    fn render_follows_the_card_shape() {
        let card = render(&photo_brief(), &musashi_draft());
        let order = [
            "Identity:\nCharacter: Miyamoto Musashi from Fate. A carefree swordswoman.\nGender: Female, adult.\nSpeaks as: casual and playful; calls the user \"Master\".",
            "Appearance (use these tags for photos, always the same):\nmiyamoto musashi \\(fate\\), 1girl, pink hair, blue eyes.\nIn actions, call her \"Musashi\", never her full name.",
            "Wardrobe (photos use the CURRENT outfit from her state, never a fixed one):\n- Swimsuit: american flag bikini, bun cover.\n- Casual",
            "Personality:\n- Cheerful and blunt\n- Loves a good duel",
            "Photo habit: often. She likes showing off new outfits.",
            "Stats (update every one of them every turn):",
            "Behavior rules:",
        ];
        let mut last = 0;
        for needle in order {
            let at = pos(&card, needle);
            assert!(at >= last, "{needle:?} out of order");
            last = at;
        }
        assert!(card.ends_with('\n') && !card.ends_with("\n\n"));
        assert!(card.contains("\n\nPhoto habit: often."));
        assert!(card.contains("Sword Focus: how sharp her focus is"));
    }

    #[test]
    fn render_always_has_the_eight_base_stats() {
        let card = render(&Brief::default(), &Draft::default());
        for stat in BASE_STATS {
            assert!(card.contains(&format!("\n{stat}: ")), "missing {stat}");
        }
        assert!(card.contains("Gender: Female, adult."));
    }

    #[test]
    fn render_plain_style_original_and_other_gender() {
        let brief = Brief {
            name: "Nova".into(),
            appearance_style: AppearanceStyle::Plain,
            ..Default::default()
        };
        let mut draft = Draft::default();
        draft.identity.gender = Gender::Other;
        draft.identity.summary = "A drifter.".into();
        draft.appearance.description = "Short silver hair, grey eyes.".into();
        draft.appearance.tags = vec!["ignored tag".into()];
        let card = render(&brief, &draft);
        assert!(card.contains("Character: Nova, an original character. A drifter."));
        assert!(card.contains("Gender: Other, adult."));
        assert!(card.contains(
            "Appearance (describe their like this in photos, always the same):\nShort silver hair, grey eyes.\nIn actions, call their \"Nova\"."
        ));
        assert!(!card.contains("ignored tag"));
        assert!(!card.contains("1girl") && !card.contains("1boy"));
        assert!(card.contains("When they send a photo, they say so in their reply."));
    }

    #[test]
    fn render_tags_style_other_gender_has_no_person_tag() {
        let mut draft = Draft::default();
        draft.identity.gender = Gender::Other;
        draft.appearance.tags = vec!["short hair".into()];
        let card = render(&Brief::default(), &draft);
        assert!(card.contains("\nshort hair.\n"));
    }

    #[test]
    fn render_short_name_equal_to_name() {
        let mut draft = musashi_draft();
        draft.identity.short_name = "Miyamoto Musashi".into();
        let card = render(&musashi_brief(), &draft);
        assert!(card.contains("In actions, call her \"Miyamoto Musashi\".\n"));
        assert!(!card.contains("never her full name"));
    }

    #[test]
    fn render_empty_sections_keep_their_headings() {
        let card = render(&musashi_brief(), &Draft::default());
        for heading in ["Wardrobe (", "Personality:", "Photo habit: normal."] {
            assert!(card.contains(heading), "{heading}");
        }
        assert!(card.contains("Calls the user \"you\"."));
        // No empty bullets from the empty model-supplied rules.
        assert!(!card.contains("\n- \n") && !card.lines().any(|l| l.trim() == "-"));
    }

    #[test]
    fn render_content_level_lines() {
        for (level, line) in [
            (
                ContentLevel::AllAges,
                "Keep everything non-explicit; intimate scenes fade to black.",
            ),
            (
                ContentLevel::Suggestive,
                "Flirty and teasing is fine, never explicit.",
            ),
            (
                ContentLevel::Mature,
                "Explicit content is allowed when the story leads there.",
            ),
        ] {
            let brief = Brief {
                content_level: level,
                ..musashi_brief()
            };
            let card = render(&brief, &musashi_draft());
            assert!(card.contains(&format!("\n- {line}\n")), "{level:?}");
        }
    }

    #[test]
    fn render_leaves_weighted_tags_untouched() {
        let mut draft = Draft::default();
        draft.appearance.tags = vec!["(long hair:1.2)".into(), "lora:foo".into()];
        let card = render(&Brief::default(), &draft);
        assert!(card.contains("(long hair:1.2), lora:foo."));
    }

    #[test]
    fn identity_short_name_backstop() {
        let brief = musashi_brief();
        let raw = json!({
            "summary": "A swordswoman.", "gender": "female", "speaks_as": "playful",
            "calls_user": "", "short_name": "Bunny"
        });
        let v = finish_identity(&raw, &brief).unwrap();
        assert_eq!(v["short_name"], "Miyamoto Musashi");
        assert_eq!(v["calls_user"], "you");
        let raw = json!({ "summary": "x", "gender": "male", "speaks_as": "", "calls_user": "u", "short_name": "musashi" });
        let v = finish_identity(&raw, &brief).unwrap();
        assert_eq!(v["short_name"], "Musashi");
        assert_eq!(v["gender"], "male");
        assert!(finish_identity(&json!({ "summary": "  " }), &brief).is_none());
    }

    #[test]
    fn appearance_composition_drops_outfit_tags() {
        let raw = json!({
            "hair_color": "Pink hair", "eye_color": "blue", "hair_length": "long",
            "build": "none", "extra": ["ahoge", "red bikini", "Pink Hair"]
        });
        let tags = compose_body_tags(&raw);
        assert_eq!(tags, vec!["pink hair", "long hair", "blue eyes", "ahoge"]);
    }

    #[test]
    fn wardrobe_caps_and_names() {
        let outfits: Vec<Value> = (0..8)
            .map(|i| {
                json!({
                    "name": format!("Outfit number {i} with a very long name that goes on and on"),
                    "items": (0..12).map(|j| format!("item {j}")).collect::<Vec<_>>(),
                })
            })
            .collect();
        let v = finish_wardrobe(&json!({ "outfits": outfits })).unwrap();
        let out: Vec<Outfit> = serde_json::from_value(v).unwrap();
        assert_eq!(out.len(), MAX_OUTFITS);
        assert!(out.iter().all(|o| o.name.chars().count() <= 40));
        assert!(out.iter().all(|o| o.items.len() == MAX_OUTFIT_ITEMS));
        assert!(finish_wardrobe(&json!({ "outfits": [] })).is_none());
    }

    #[test]
    fn personality_dedups_strips_bullets_and_needs_two() {
        let raw = json!({ "traits": ["- Kind", "1. kind", "* \"Brave\"", "2) Loud"] });
        let v = finish_personality(&raw).unwrap();
        assert_eq!(v, json!(["Kind", "Brave", "Loud"]));
        assert!(finish_personality(&json!({ "traits": ["Only one"] })).is_none());
    }

    #[test]
    fn strong_language_survives_post_processing() {
        let raw =
            json!({ "traits": ["Swears like a sailor: damn, hell and worse", "Fucking brave"] });
        let v = finish_personality(&raw).unwrap();
        assert_eq!(
            v,
            json!([
                "Swears like a sailor: damn, hell and worse",
                "Fucking brave"
            ])
        );
    }

    #[test]
    fn extra_stats_cannot_reuse_the_base_stats() {
        let raw = json!({ "stats": [
            { "name": "mood", "description": "dup" },
            { "name": "sword focus level now", "description": "how sharp" },
        ] });
        let v = finish_stats(&raw).unwrap();
        let stats: Vec<Stat> = serde_json::from_value(v).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].name, "Sword Focus Level");
    }

    #[test]
    fn brief_sanitized_and_is_original() {
        let b = Brief {
            name: format!("  A\n  {} ", "b".repeat(200)),
            source: " Original ".into(),
            vibe: "x".repeat(900),
            ..Default::default()
        }
        .sanitized();
        assert!(!b.name.contains('\n') && b.name.chars().count() == 80);
        assert_eq!(b.vibe.chars().count(), 600);
        assert!(b.is_original());
        assert!(Brief::default().is_original());
        assert!(!musashi_brief().is_original());
    }

    #[test]
    fn test_photo_state_parses() {
        for draft in [Draft::default(), musashi_draft()] {
            let state = test_photo_state(&musashi_brief(), &draft);
            let fields = crate::chat_prompts::parse_state_fields(&state);
            let outfit = fields.iter().find(|f| f.name == "outfit").unwrap();
            if draft.wardrobe.is_empty() {
                assert_eq!(outfit.value, "");
            } else {
                assert_eq!(outfit.value, "american_flag_bikini, bun_cover");
            }
            assert!(fields.iter().any(|f| f.name == "location"));
        }
    }

    #[test]
    fn stop_registry() {
        assert!(!stop("pb-unknown-id"));
        let guard = BuildGuard::register("pb-test-id");
        assert!(!guard.cancel.is_cancelled());
        assert!(stop("pb-test-id"));
        assert!(guard.cancel.is_cancelled());
        drop(guard);
        assert!(!stop("pb-test-id"));
    }

    #[tokio::test]
    async fn dead_endpoint_gives_a_renderable_fallback() {
        let cfg = AppConfig {
            endpoint: "http://127.0.0.1:9".into(),
            ..AppConfig::default()
        };
        let brief = musashi_brief();
        let out = generate_section(
            &cfg,
            "pb-dead",
            &brief,
            Section::Identity,
            &Draft::default(),
            &[],
        )
        .await;
        assert!(out.fallback && !out.stopped);
        assert!(out.warning.is_some());
        let identity: Identity = serde_json::from_value(out.value).unwrap();
        assert_eq!(identity.short_name, "Miyamoto Musashi");
        let draft = Draft {
            identity,
            ..Default::default()
        };
        assert!(render(&brief, &draft).contains("Character: Miyamoto Musashi from Fate."));
    }

    #[tokio::test]
    async fn test_photo_fields_fall_back_offline_and_stop_returns_none() {
        // Offline: the image prompt builder's own fallback still gives a prompt.
        let dead = AppConfig {
            endpoint: "http://127.0.0.1:9".into(),
            ..AppConfig::default()
        };
        let (brief, draft) = (musashi_brief(), musashi_draft());
        let fields = test_photo_fields(&dead, "pb-photo-fallback", &brief, &draft)
            .await
            .expect("not stopped");
        assert!(fields.positive.is_some());

        // A server that accepts and never answers, then a Stop.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let hung = AppConfig {
            endpoint: format!("http://{}", listener.local_addr().unwrap()),
            ..AppConfig::default()
        };
        let stopper = async {
            while !stop("pb-photo-stop") {
                tokio::task::yield_now().await;
            }
        };
        let (fields, ()) = tokio::join!(
            test_photo_fields(&hung, "pb-photo-stop", &brief, &draft),
            stopper
        );
        assert!(fields.is_none());
    }

    #[tokio::test]
    async fn research_is_empty_for_an_original_character() {
        let cfg = AppConfig::default();
        let found = research(&cfg, "pb-research", &Brief::default())
            .await
            .unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn sidecar_round_trip_update_and_hand_edit() {
        let dir = scratch("sidecar");
        let (brief, draft) = (musashi_brief(), musashi_draft());
        let content = render(&brief, &draft);
        let saved = save(SaveMode::Create, "", &brief, &draft, &content).unwrap();
        assert_eq!(saved.name, "Miyamoto Musashi");
        let loaded = load("Miyamoto Musashi").unwrap().unwrap();
        assert_eq!(loaded.content, content);
        assert_eq!(loaded.draft.identity.calls_user, "Master");
        assert!(!loaded.hand_edited);

        let mut changed = draft.clone();
        changed.identity.calls_user = "Boss".into();
        let content2 = render(&brief, &changed);
        save(
            SaveMode::Update,
            "Miyamoto Musashi",
            &brief,
            &changed,
            &content2,
        )
        .unwrap();
        let loaded = load("Miyamoto Musashi").unwrap().unwrap();
        assert_eq!(loaded.draft.identity.calls_user, "Boss");
        assert_eq!(persona::load_persona("Miyamoto Musashi").unwrap(), content2);

        persona::update_persona("Miyamoto Musashi", "edited by hand").unwrap();
        assert!(load("Miyamoto Musashi").unwrap().unwrap().hand_edited);

        remove_sidecar("Miyamoto Musashi");
        assert!(load("Miyamoto Musashi").unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_without_a_sidecar_or_with_a_bad_name_fails() {
        let dir = scratch("sidecar-guard");
        persona::save_new_persona("Plain", "text").unwrap();
        let (brief, draft) = (musashi_brief(), Draft::default());
        assert!(save(SaveMode::Update, "Plain", &brief, &draft, "x").is_err());
        assert_eq!(persona::load_persona("Plain").unwrap(), "text");
        assert!(save(SaveMode::Update, "../evil", &brief, &draft, "x").is_err());
        assert!(load("../evil").unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_a_persona_drops_its_draft() {
        let dir = scratch("sidecar-delete");
        let (brief, draft) = (musashi_brief(), musashi_draft());
        let content = render(&brief, &draft);
        save(SaveMode::Create, "", &brief, &draft, &content).unwrap();
        persona::delete_persona("Miyamoto Musashi").unwrap();
        assert!(load("Miyamoto Musashi").unwrap().is_none());
        persona::save_new_persona("Miyamoto Musashi", "new card").unwrap();
        assert!(load("Miyamoto Musashi").unwrap().is_none());
        assert!(!sidecar_dir().join("Miyamoto Musashi.json").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_ignores_the_sidecar_folder() {
        let dir = scratch("sidecar-list");
        let (brief, draft) = (musashi_brief(), musashi_draft());
        save(SaveMode::Create, "", &brief, &draft, "card").unwrap();
        let names: Vec<String> = persona::list_personas()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["Miyamoto Musashi".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn suggestions_from_the_bundled_list() {
        let dir = scratch("suggest");
        let cfg = AppConfig::default();
        let data = danbooru::TagData::open_in(&cfg, &dir.join("config")).expect("bundled list");
        let s = build_suggestion(&data, "miyamoto_musashi_(swimsuit_berserker)_(fate)")
            .unwrap()
            .expect("known character");
        assert_eq!(s.character.name, "miyamoto_musashi_(fate)");
        assert!(
            s.body_tags.contains(&"pink hair".to_string()),
            "{:?}",
            s.body_tags
        );
        assert!(
            s.body_tags.contains(&"blue eyes".to_string()),
            "{:?}",
            s.body_tags
        );
        assert!(!s.body_tags.contains(&"breasts".to_string()));
        assert!(!s.outfits.is_empty());
        assert!(build_suggestion(&data, "not_a_real_tag_zzz")
            .unwrap()
            .is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn body_groups_and_first_group() {
        assert_eq!(body_group("pink_hair"), "hair_color");
        assert_eq!(body_group("long_hair"), "hair_length");
        assert_eq!(body_group("blue_eyes"), "eyes");
        assert_eq!(body_group("large_breasts"), "breasts");
        assert_eq!(
            first_group("miyamoto_musashi_(swimsuit_berserker)_(fate)").as_deref(),
            Some("swimsuit berserker")
        );
        assert_eq!(first_group("1girl"), None);
    }
}
