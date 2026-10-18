//! Chat mode's prompts and the pure text helpers around them: the turn-1
//! system message, the state-sheet helpers, and the one tolerant cleaner
//! every model reply goes through.

use crate::rules::collapse_blank_runs;

/// Keeps a persona's internal-stats section (field names, percentages,
/// thresholds written to brief the separate state-update call) out of the
/// visible reply.
pub const CHAT_STATE_LEAK_GUARD_PROMPT: &str =
    "Your character sheet may describe internal stats, meters, or levels it tracks for memory \
purposes (a percentage, a named level, a threshold like \"at 85% she does X\") -- that tracking \
happens invisibly in a separate step, never in this reply. Let those stats shape your behavior, \
tone, and what you choose to do naturally, but never state their literal field name or numeric \
value in the narration or dialogue itself -- never write something like \"_Arousal_ is 70%\" or \
\"her Trust level rises to 40\". Show the effect, not the bookkeeping: write only what someone \
actually in the scene would say or perceive.";

/// Stops the character from writing a fake attachment into her own text; the
/// app decides separately whether a photo is attached.
pub const CHAT_PHOTO_GUARD_PROMPT: &str =
    "You can't attach files yourself; if you decide to share \
a photo, just say so in character -- the app attaches it.";

/// Added to turn 1's system message when the user forced a photo (`/pic`).
pub const CHAT_FORCED_PHOTO_LINE: &str = "The user asked for a photo; one is being sent along \
with your reply. React naturally (e.g. 'here you go...'); don't describe the photo in detail.";

/// The state-update call's instructions. The JSON shape itself is enforced by
/// the request's schema, so this only says what to track.
/// The four baseline fields are required by the schema too (`state_schema`
/// in chat_turn.rs): a card with no stats of its own still needs outfit and
/// place tracked, or the image prompt has nothing current to draw from.
pub const CHAT_STATE_UPDATE_PROMPT: &str =
    "You are not replying to the user -- nothing you write here is ever shown to them, and you are \
not continuing the conversation. Given the exchange below and your previous state, return your \
complete, current state as one flat JSON object with string values.\n\n\
Always fill these four fields with your character's CURRENT situation:\n\
- outfit: what you are wearing right now\n\
- location: where you are right now\n\
- activity: what you are doing right now\n\
- mood: how you feel right now\n\
Also carry every field your character sheet itself defines, if it defines any. Never invent other \
fields. Carry every field forward unchanged unless this exchange changed it; clothing, position and \
place change the moment the conversation says so. Use \"unknown\" only if there is truly no \
information.\n\n\
Read the user's own message too, not just your reply -- if it wraps part of itself in `*asterisks*` \
(older chats used `//...//`; same meaning), that's the user describing their own action, not \
something spoken aloud. Inside that action, \"you\" or your character's name means you (the \
persona) are the one being acted upon, and \"I\"/\"me\" means the user -- never swap the two. For \
example, \"*offering you an ice cream* *blushing*\" means the user is offering YOU an ice cream, \
and the user is the one blushing.";

/// Narrative fields only; precise ones never reach the summarizer
/// (`is_precise_field`), so it can't drift on data it never sees.
pub const CHAT_STATE_SUMMARY_PROMPT: &str =
    "You'll be given some fields from a character's current state, as JSON. Write a short, natural \
paragraph (2-4 sentences) that reads as flowing description, not a labeled list -- but don't invent \
new details and don't omit any field's actual information, just phrase it as prose instead of \
separate lines. Output only the paragraph itself, nothing else.";

/// Every reply uses the common roleplay style the UI renders as separate
/// blocks (`splitChatBlocks`): `*action*`, `"speech"`, `(thought)`. Small
/// models already write this way, so the prompt stays short and concrete;
/// unmarked text counts as speech, so no format is an error. "You" is pinned
/// to the human so a reader never has to guess who is acting. The old
/// `//`/`||` markers are named only to forbid them -- they leaked into the
/// display as stray slashes.
pub const CHAT_NARRATION_PROMPT: &str = "Write your replies in roleplay style:\n\
- Actions and the scene go between single asterisks: *Mika smiles and tilts her head*\n\
- Spoken words go in double quotes: \"Hello!\"\n\
- Private thoughts go in parentheses: (I hope he likes it)\n\
Example reply: *Mika smiles* \"Hello!\" (I hope he likes it)\n\
Never use `//` or `||`.\n\n\
Be clear about who acts. \"You\" always means the real person you're talking to, never your own \
character. Write your own character's actions in the third person by name (or \"she\"/\"he\"), \
never as \"you\": *Elara leans back and smiles* is your action, and *You unzip the satchel* is \
only for something the real person did.";

// --- Reply cleaning ---

/// Fence tags a chat reply should never contain; stripped if a model emits
/// one anyway.
const STRAY_FENCE_TAGS: &[&str] = &["state", "image-prompt", "web-search", "ruleset"];

/// Removes every fenced block whose info line starts with one of `tags`
/// (case-insensitive; trailing spaces and CRLF tolerated; extra words after
/// the tag allowed). An unterminated block runs to the end of the text. Other
/// fences are left alone. Blank runs are collapsed and the result trimmed.
pub fn strip_fences(text: &str, tags: &[&str]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut skipping = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if skipping {
            if trimmed == "```" {
                skipping = false;
            }
            continue;
        }
        if let Some(info) = trimmed.strip_prefix("```") {
            let info_lower = info.to_ascii_lowercase();
            let matches_tag = tags.iter().any(|t| {
                info_lower.strip_prefix(t).is_some_and(|rest| {
                    rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace() || c == '`')
                })
            });
            if matches_tag {
                // Opened and closed on the same line.
                skipping = !info.contains("```");
                continue;
            }
        }
        out.push_str(line);
    }
    collapse_blank_runs(&out).trim().to_string()
}

/// The one cleaner for any model reply that becomes chat text: reasoning
/// split off, stray protocol fences dropped. Returns `(reply, thinking)`.
pub fn clean_reply(raw: &str) -> (String, Option<String>) {
    let (thinking, content) = crate::llm::split_thinking(raw);
    (strip_fences(&content, STRAY_FENCE_TAGS), thinking)
}

// --- System content ---

/// Appends `state` capped at `state_max_tokens` (0 = no cap) with an explicit
/// truncation note: the system message is never trimmed by
/// `context::fit_to_budget`, so this is the only thing keeping it in check.
fn append_state_block(out: &mut String, state: &str, state_max_tokens: u32) {
    let state = state.trim();
    if state.is_empty() {
        return;
    }
    let capped = if state_max_tokens == 0 {
        state.to_string()
    } else {
        crate::context::truncate_with_note(state, (state_max_tokens as usize) * 4, "state")
    };
    out.push_str("\n\n## Your persistent state from earlier in this conversation\n");
    out.push_str(&capped);
}

/// The system message for every in-character reply (turn 1, image reaction,
/// search answer): narration format, photo and state-leak guards, persona,
/// then the compact `state.md`. `photo_forced` adds the one line telling the
/// character a photo is being sent with this reply.
pub fn build_chat_system_content(
    persona: Option<&str>,
    state: &str,
    state_max_tokens: u32,
    photo_forced: bool,
) -> String {
    let mut out = CHAT_NARRATION_PROMPT.to_string();
    out.push_str("\n\n");
    out.push_str(CHAT_PHOTO_GUARD_PROMPT);
    out.push_str("\n\n");
    out.push_str(CHAT_STATE_LEAK_GUARD_PROMPT);
    if photo_forced {
        out.push_str("\n\n");
        out.push_str(CHAT_FORCED_PHOTO_LINE);
    }
    if let Some(persona) = persona {
        out.push_str("\n\n");
        out.push_str(persona);
    }
    append_state_block(&mut out, state, state_max_tokens);
    out
}

// --- State sheet ---

/// One field of the state JSON, in the order `serde_json` read them.
pub struct StateField {
    pub name: String,
    pub value: String,
}

/// Flat JSON object -> ordered field list. Non-string scalars are
/// stringified (a bare number is an easy near-miss, not a reason to lose the
/// field); nested values are skipped; anything that isn't an object is empty.
pub fn parse_state_fields(raw_json: &str) -> Vec<StateField> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(raw_json)
    else {
        return Vec::new();
    };
    flat_fields(map)
}

/// Same flattening for an already-parsed object.
pub fn flat_fields(map: serde_json::Map<String, serde_json::Value>) -> Vec<StateField> {
    map.into_iter()
        .filter_map(|(name, value)| {
            let value = match value {
                serde_json::Value::String(s) => Some(s),
                serde_json::Value::Number(n) => Some(n.to_string()),
                serde_json::Value::Bool(b) => Some(b.to_string()),
                serde_json::Value::Null => Some("none".to_string()),
                serde_json::Value::Object(_) | serde_json::Value::Array(_) => None,
            }?;
            Some(StateField { name, value })
        })
        .collect()
}

/// `true` for a field precise enough that summarizing it risks losing the
/// one thing worth tracking: the value is essentially a number, fraction or
/// percentage, or the name says it anchors the current moment ("action").
/// Such fields are kept verbatim and never reach the summarizer.
pub fn is_precise_field(name: &str, value: &str) -> bool {
    name.to_lowercase().contains("action") || looks_quantitative(value)
}

/// Deliberately tight (digits, `digits%`, `digits/digits`) so a sentence that
/// merely mentions a number isn't pulled out of its context.
fn looks_quantitative(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    let digits_only = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if let Some(pct) = value.strip_suffix('%') {
        return digits_only(pct);
    }
    if let Some((whole, part)) = value.split_once('/') {
        return digits_only(whole) && digits_only(part);
    }
    digits_only(value)
}

/// (precise, narrative), each side keeping its original order.
pub fn partition_state_fields(fields: Vec<StateField>) -> (Vec<StateField>, Vec<StateField>) {
    fields
        .into_iter()
        .partition(|f| is_precise_field(&f.name, &f.value))
}

/// `state.md`: precise fields bolded and verbatim first, then the narrative
/// paragraph. Either half may be empty.
pub fn build_state_markdown(precise: &[StateField], narrative_summary: &str) -> String {
    let mut lines: Vec<String> = precise
        .iter()
        .map(|f| format!("**{}** : {}", f.name, f.value))
        .collect();
    let narrative_summary = narrative_summary.trim();
    if !narrative_summary.is_empty() {
        lines.push(narrative_summary.to_string());
    }
    lines.join("\n")
}

/// Fallback for `build_state_markdown` when the summarize call fails: a plain
/// listing, so the narrative fields aren't lost.
pub fn plain_field_listing(fields: &[StateField]) -> String {
    fields
        .iter()
        .map(|f| format!("**{}** : {}", f.name, f.value))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_fences_removes_a_tagged_block_and_keeps_the_rest() {
        let reply = "Here's what happened.\n```state\n{\"HP\": \"85\"}\n```\nAnything else?";
        let out = strip_fences(reply, &["state"]);
        assert!(!out.contains("```"), "{out}");
        assert!(!out.contains("HP"), "{out}");
        assert!(out.contains("Here's what happened.") && out.contains("Anything else?"));
    }

    #[test]
    fn strip_fences_is_a_no_op_without_a_match() {
        assert_eq!(strip_fences("just a reply", &["state"]), "just a reply");
        let code = "look:\n```python\nprint(1)\n```";
        assert_eq!(strip_fences(code, &["state"]), code);
    }

    #[test]
    fn strip_fences_tolerates_case_crlf_and_trailing_spaces() {
        let reply = "hi\r\n```State  \r\n{\"a\":\"b\"}\r\n```  \r\nbye";
        let out = strip_fences(reply, &["state"]);
        assert!(!out.contains("```") && !out.contains("\"a\""), "{out:?}");
        assert!(out.contains("hi") && out.contains("bye"));
    }

    #[test]
    fn strip_fences_handles_inline_and_unterminated_blocks() {
        assert_eq!(
            strip_fences("a\n```ruleset image-generation-prompt```\nb", &["ruleset"]),
            "a\nb"
        );
        assert_eq!(
            strip_fences("a\n```image-prompt\npositive: x", &["image-prompt"]),
            "a"
        );
    }

    #[test]
    fn strip_fences_matches_whole_tags_only() {
        let reply = "```stately\nkeep\n```";
        assert_eq!(strip_fences(reply, &["state"]), reply);
    }

    #[test]
    fn clean_reply_splits_reasoning_and_drops_stray_fences() {
        let raw = "<think>she should smile</think>|| Hi! ||\n```web-search\nquery: x\n```\n\n\n\n// waves //";
        let (reply, thinking) = clean_reply(raw);
        assert_eq!(thinking.as_deref(), Some("she should smile"));
        assert_eq!(reply, "|| Hi! ||\n\n// waves //");
    }

    #[test]
    fn clean_reply_ignores_a_fence_that_only_lived_in_the_reasoning() {
        let raw = "<think>maybe ```image-prompt\npositive: x\n```</think>|| ok ||";
        let (reply, thinking) = clean_reply(raw);
        assert_eq!(reply, "|| ok ||");
        assert!(thinking.is_some());
    }

    #[test]
    fn clean_reply_handles_a_lone_closing_think_tag() {
        let (reply, thinking) = clean_reply("reasoning here</think>|| hello ||");
        assert_eq!(reply, "|| hello ||");
        assert_eq!(thinking.as_deref(), Some("reasoning here"));
    }

    #[test]
    fn is_precise_field_rules() {
        assert!(is_precise_field("Arousal", "75%"));
        assert!(is_precise_field("HP", "85/100"));
        assert!(is_precise_field("Level", "12"));
        assert!(is_precise_field("Action", "waits for the request"));
        assert!(is_precise_field("User Action", "asks for a photo"));
        assert!(!is_precise_field("Mind", "cheerful and a little nervous"));
        assert!(!is_precise_field(
            "Backstory",
            "she's 8 years older than her brother"
        ));
    }

    #[test]
    fn parse_state_fields_reads_a_flat_object_and_stringifies_non_strings() {
        let fields = parse_state_fields(r#"{"HP": "85/100", "Level": 3, "Alive": true}"#);
        let mut pairs: Vec<(String, String)> =
            fields.into_iter().map(|f| (f.name, f.value)).collect();
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                ("Alive".to_string(), "true".to_string()),
                ("HP".to_string(), "85/100".to_string()),
                ("Level".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn parse_state_fields_is_empty_for_invalid_or_non_object_json() {
        assert!(parse_state_fields("not json").is_empty());
        assert!(parse_state_fields("[1, 2, 3]").is_empty());
    }

    #[test]
    fn partition_state_fields_splits_precise_from_narrative() {
        let fields =
            parse_state_fields(r#"{"HP": "85/100", "Mind": "cheerful", "Action": "waiting"}"#);
        let (precise, narrative) = partition_state_fields(fields);
        let precise_names: Vec<&str> = precise.iter().map(|f| f.name.as_str()).collect();
        let narrative_names: Vec<&str> = narrative.iter().map(|f| f.name.as_str()).collect();
        assert!(precise_names.contains(&"HP") && precise_names.contains(&"Action"));
        assert_eq!(narrative_names, vec!["Mind"]);
    }

    #[test]
    fn build_state_markdown_bolds_precise_fields_and_appends_the_summary() {
        let precise = vec![StateField {
            name: "HP".to_string(),
            value: "85/100".to_string(),
        }];
        assert_eq!(
            build_state_markdown(&precise, "She seems cheerful today."),
            "**HP** : 85/100\nShe seems cheerful today."
        );
        assert_eq!(build_state_markdown(&precise, ""), "**HP** : 85/100");
    }

    #[test]
    fn system_content_has_guards_persona_and_state() {
        let out =
            build_chat_system_content(Some("You are Aria, a shopkeeper."), "HP: 90", 0, false);
        assert!(out.contains(CHAT_NARRATION_PROMPT));
        assert!(out.contains(CHAT_PHOTO_GUARD_PROMPT));
        assert!(out.contains(CHAT_STATE_LEAK_GUARD_PROMPT));
        assert!(out.contains("You are Aria, a shopkeeper.") && out.contains("HP: 90"));
        assert!(!out.contains(CHAT_FORCED_PHOTO_LINE));
    }

    #[test]
    fn narration_prompt_teaches_roleplay_style_and_forbids_old_markers() {
        let p = CHAT_NARRATION_PROMPT;
        assert!(
            p.contains("*Mika smiles*")
                && p.contains("\"Hello!\"")
                && p.contains("(I hope he likes it)")
        );
        assert!(p.contains("Never use `//` or `||`"));
        // The old markers appear only in that prohibition.
        assert_eq!(p.matches("//").count(), 1, "{p}");
        assert!(!CHAT_STATE_UPDATE_PROMPT.contains("`||`"));
        assert!(CHAT_STATE_UPDATE_PROMPT.contains("*asterisks*"));
    }

    #[test]
    fn forced_photo_adds_exactly_one_line() {
        let out = build_chat_system_content(None, "", 0, true);
        assert!(out.contains(CHAT_FORCED_PHOTO_LINE));
    }

    #[test]
    fn system_content_omits_empty_state_and_ruleset_talk() {
        let out = build_chat_system_content(None, "   ", 0, false);
        assert!(
            !out.contains("persistent state") && !out.contains("ruleset"),
            "{out}"
        );
    }

    #[test]
    fn system_content_caps_a_long_state_and_says_so() {
        let long_state = "x".repeat(10_000);
        let out = build_chat_system_content(None, &long_state, 50, false);
        assert!(out.len() < long_state.len());
        assert!(out.contains("condensed away"), "{out}");
    }
}
