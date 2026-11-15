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
place change the moment the conversation says so. If you agreed to change clothes or said you \
changed, outfit must name the NEW clothes concretely (if the reply doesn't say which, pick one \
that fits your character sheet and the request, e.g. its casual outfit) -- never keep the old \
outfit then. List in outfit only garments you are still wearing (drop removed ones, no \"removed\" \
notes); when you are naked, set clothing_state to \"nude\". Never leave location unknown: if nothing says where you are, pick a plausible place \
that fits your character and the moment (e.g. her room) and keep it from then on. Use \"unknown\" \
only if there is truly no information.\n\n\
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

/// Conversational manners. Goes before the persona, so a card can override.
pub const CHAT_NATURAL_PROMPT: &str = "Talk like a real person in a private chat, not like an \
assistant or a storyteller:\n\
- Respond to what they just said first, in your own voice and mood.\n\
- Keep it short, the length of a chat message, unless they ask for more or the moment calls for it.\n\
- Don't repeat their words back, don't sum things up, don't offer help, don't write lists.\n\
- Ask a question only when you're curious, and only one.\n\
- When they clearly ask for something your character's personality and content level allow, do it in this reply and describe the action. At most one playful hesitation, and never ask \"are you sure?\" or \"you want that?\" two turns in a row.\n\
Your character sheet below wins wherever it says otherwise.";

const PHOTO_NOTE_USER_MAX_CHARS: usize = 300;

/// The ephemeral note asking for the line she says along with her photo. It
/// travels in the user role (the only one that can carry an image), so it
/// says outright that the photo is hers: a small model otherwise answers as
/// someone who was just shown a picture. `user_text` ties the line to what
/// the user asked; `optional` allows the "none" veto.
pub fn photo_followup_note(
    positive: &str,
    user_text: Option<&str>,
    sees_photo: bool,
    optional: bool,
) -> String {
    let mut out = String::from(
        "[Note from the app, not a message from the person you're talking to.]\n\
You just sent them a photo. It is your own photo: you took it and you chose to send it",
    );
    out.push_str(if sees_photo {
        ", and it is attached here so you know what they see.\n"
    } else {
        ".\n"
    });
    out.push_str(&format!("It was made from this description: {positive}\n"));
    let asked: String = user_text
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
        .chars()
        .take(PHOTO_NOTE_USER_MAX_CHARS)
        .collect();
    if !asked.is_empty() {
        out.push_str(&format!("Their last message was: \"{asked}\"\n"));
    }
    out.push_str(
        "This is the ONE photo you just sent, and it is already sent: never say you are sending or \
will send another picture. Write 1 to 3 short sentences that carry the moment forward from where \
your last message left off: a new beat (what you do or feel next, or something you ask them), \
not a restatement of your last message. Keep the narration style your character card uses, in \
first person, never calling yourself by your full name. If they asked to see something, connect \
it to what they asked. Don't describe the photo, review it or act surprised by it.\n\
Good: *tucks my hair behind my ear* \"So, do I get a compliment, or do I have to beg?\"\n\
Good (mid-activity): *takes another bite* \"Mm, this is so good. Have you eaten yet?\"\n\
Bad: \"Wow, is that me? So cute!\"\n\
Bad: *sends another picture* \"Here's one more!\"\n\
Use your own words, not the examples'.",
    );
    if sees_photo {
        out.push_str(" Look at it only so that what you say matches what it really shows.");
    }
    if optional {
        out.push_str(
            "\nIf your last message already said it all and another line would feel forced, \
reply with exactly the single word: none",
        );
    }
    out
}

/// Appended to the photo note for the one retry after a viewer-style reply.
pub const PHOTO_NOTE_RETRY: &str = "\nYour last attempt reacted to the photo like someone seeing \
it for the first time. It is your own photo. Write only what you would say while sending it, or \
carry on with what you are doing in it.";

/// Phrases of someone looking at a picture they were shown, not of the person
/// who sent it. English only; a miss just lets the reply through.
const PHOTO_REVIEW_PHRASES: &[&str] = &[
    "is that me",
    "is this me",
    "is that really me",
    "is this really me",
    "that's me?",
    "this is me?",
    "the image shows",
    "the photo shows",
    "the picture shows",
    "this image shows",
    "this photo shows",
    "this picture shows",
    "in the image",
    "in this image",
    "the image depicts",
    "what a cute p",
    "what a nice p",
    "what a lovely p",
    "what a beautiful p",
    "what a great p",
    "nice photo",
    "nice picture",
    "great photo",
    "great picture",
    "cute photo",
    "cute picture",
    "thanks for sharing",
    "thank you for sharing",
    "thanks for the p",
    "thank you for the p",
    "you sent me",
    "photo you sent",
    "picture you sent",
    "image you sent",
    "i can't see",
    "i cannot see",
    "as an ai",
];

/// The mechanical backstop behind `photo_followup_note`: does this follow-up
/// read as a comment on a received picture?
pub fn reads_as_photo_review(reply: &str) -> bool {
    let text = reply.to_lowercase().replace('’', "'");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    PHOTO_REVIEW_PHRASES.iter().any(|p| text.contains(p))
}

/// Phrases of a reaction claiming a further picture went out; none did.
const ANOTHER_PHOTO_PHRASES: &[&str] = &[
    "another picture",
    "another photo",
    "another selfie",
    "another pic",
    "another image",
    "new picture",
    "new photo",
    "new selfie",
    "second picture",
    "second photo",
    "second selfie",
    "more pictures",
    "more photos",
    "more selfies",
];

/// Why a post-photo reaction should be dropped, if it should: it claims
/// another picture, or says (almost) what her previous reply already said.
pub fn redundant_reaction(reaction: &str, previous: Option<&str>) -> Option<&'static str> {
    let text = reaction.to_lowercase().replace('’', "'");
    if ANOTHER_PHOTO_PHRASES.iter().any(|p| text.contains(p)) {
        return Some("mentions sending another picture");
    }
    let prev = previous?;
    let words = |s: &str| -> std::collections::HashSet<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    };
    let (a, b) = (words(reaction), words(prev));
    let union = a.union(&b).count();
    if union > 0 && a.intersection(&b).count() as f64 / union as f64 >= REACTION_REPEAT_SIMILARITY {
        return Some("repeats her previous reply");
    }
    None
}

/// Word-set overlap (Jaccard) at which a reaction counts as a repeat.
const REACTION_REPEAT_SIMILARITY: f64 = 0.7;

/// Phrases of a reply that asks the user to confirm what they already asked.
const CONFIRMATION_PHRASES: &[&str] = &[
    "are you sure",
    "you sure",
    "you're sure",
    "you want that",
    "you want me to",
    "really?",
    "for real?",
];

/// Ephemeral system line added when she keeps asking for confirmation or
/// keeps dodging what the user asked; never stored in history.
/// Added to turn 1's system message when the previous reply carried a photo:
/// small models otherwise explain an image-model glitch as a story event.
pub const CHAT_PHOTO_GLITCH_LINE: &str =
    "Your previous message came with a photo made by an image \
generator, so it can contain mistakes (an object in a strange place, wrong clothes, odd hands). If \
the user points out something odd in it, it was a glitch of the photo, not something that really \
happened: shrug it off in character (e.g. the camera messed up) and don't make it part of the \
story.";

pub const CHAT_STALL_NUDGE_LINE: &str = "You have been stalling or deflecting. Stop asking for \
confirmation and stop changing the subject: do what the user asked in this reply and describe \
yourself doing it.";

/// Phrases (matched on word boundaries) that make a user message a request
/// for her to do something. Physical actions only: polite words ("please",
/// "can you") also open ordinary questions and would nudge plain small talk.
const REQUEST_PHRASES: &[&str] = &[
    "take",
    "strip",
    "undress",
    "show me",
    "send me",
    "get on",
    "touch",
    "kiss",
    "lie",
    "spread",
    "ride",
    "i want you to",
];

fn is_request(text: &str) -> bool {
    let lower = text.to_lowercase().replace('’', "'");
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .collect();
    let padded = format!(" {} ", words.join(" "));
    REQUEST_PHRASES
        .iter()
        .any(|p| padded.contains(&format!(" {p} ")))
}

/// True when the user's last two messages both ask her to do something and
/// her saved outfit did not visibly change between the last two states
/// (`prev_raw_state` -> `raw_state`, see `chat_director::outfit_unchanged`).
pub fn is_dodging(
    history: &[crate::llm::ChatMessage],
    prev_raw_state: &str,
    raw_state: &str,
) -> bool {
    let mut asks = history
        .iter()
        .rev()
        .filter(|m| m.role == "user" && !m.content.trim().is_empty());
    let both_requests = match (asks.next(), asks.next()) {
        (Some(a), Some(b)) => is_request(&a.content) && is_request(&b.content),
        _ => false,
    };
    both_requests
        && !prev_raw_state.trim().is_empty()
        && !raw_state.trim().is_empty()
        && crate::chat_director::outfit_unchanged(prev_raw_state, raw_state)
}

/// Phrases only: a reply that merely ends in a question ("Have you eaten
/// yet?") is ordinary conversation, not a stall.
fn asks_confirmation(reply: &str) -> bool {
    let text = reply.to_lowercase().replace('’', "'");
    CONFIRMATION_PHRASES.iter().any(|p| text.contains(p))
}

/// True when her last two replies both read as asking for confirmation.
pub fn is_stalling(history: &[crate::llm::ChatMessage]) -> bool {
    let mut replies = history
        .iter()
        .rev()
        .filter(|m| m.role == "assistant" && !m.content.trim().is_empty());
    match (replies.next(), replies.next()) {
        (Some(a), Some(b)) => asks_confirmation(&a.content) && asks_confirmation(&b.content),
        _ => false,
    }
}

/// The reply to regenerate once with `CHAT_STALL_NUDGE_LINE`: it asks for
/// confirmation although the user's message (last in `history`) asked her to
/// do something, or her previous reply already asked.
pub fn needs_stall_retry(history: &[crate::llm::ChatMessage], reply: &str) -> bool {
    if !asks_confirmation(reply) {
        return false;
    }
    let user = history
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map_or("", |m| m.content.as_str());
    let asked_before = history
        .iter()
        .rev()
        .find(|m| m.role == "assistant" && !m.content.trim().is_empty())
        .is_some_and(|m| asks_confirmation(&m.content));
    is_request(user) || asked_before
}

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
    let content = strip_fences(&content, STRAY_FENCE_TAGS);
    (strip_reply_echoes(&content), thinking)
}

/// Echoed stat lines and app notes cut from a reply; also run over old saved
/// assistant messages so they neither show nor teach the model to repeat them.
pub fn strip_reply_echoes(content: &str) -> String {
    strip_stat_block(&drop_echo_lines(content))
}

/// Lines a small model copies from its instructions instead of speaking: the
/// stats packed into one line ("*Outfit: x. Mood: y. Location: z.*") and the
/// app's photo note ("*The app attaches a photo: ...*").
fn drop_echo_lines(reply: &str) -> String {
    let kept: Vec<&str> = reply
        .lines()
        .filter(|line| {
            let packed = line
                .split(['.', ';', '|'])
                .filter(|seg| is_stat_line(seg))
                .count()
                >= 3;
            let app_note = line.to_lowercase().contains("the app attach");
            !(packed || app_note)
        })
        .collect();
    if kept.iter().all(|l| l.trim().is_empty()) {
        return reply.to_string();
    }
    kept.join("\n").trim().to_string()
}

/// "*Outfit: bikini*", "- Mood: calm", "**Affection:** 45%", "Stats:": a short
/// label (1-3 words) and a colon, the whole line.
fn is_stat_line(line: &str) -> bool {
    let t = line
        .trim()
        .trim_matches(|c: char| matches!(c, '*' | '_' | '-' | '•' | ' '));
    let Some((label, _)) = t.split_once(':') else {
        return false;
    };
    let label = label.trim_matches(|c: char| c == '*' || c == '_').trim();
    let words = label.split_whitespace().count();
    (1..=3).contains(&words)
        && label.starts_with(char::is_uppercase)
        && label
            .chars()
            .all(|c| c.is_alphabetic() || c == ' ' || c == '/')
        && !t.contains('"')
}

/// Small models echo card stats ("Stats: update every turn") as a block of
/// "Label: value" lines at the end (or start) of the reply; the app keeps the
/// state itself, so a run of 2+ such lines there is cut.
fn strip_stat_block(reply: &str) -> String {
    let mut lines: Vec<&str> = reply.lines().collect();
    let run = |ls: &mut dyn Iterator<Item = &&str>| {
        ls.take_while(|l| l.trim().is_empty() || is_stat_line(l))
            .filter(|l| !l.trim().is_empty())
            .count()
    };
    let cut_end = |lines: &Vec<&str>| {
        let n = lines
            .iter()
            .rev()
            .take_while(|l| l.trim().is_empty() || is_stat_line(l))
            .count();
        (run(&mut lines.iter().rev()) >= 2).then_some(n)
    };
    if let Some(n) = cut_end(&lines) {
        lines.truncate(lines.len() - n);
    }
    let n = lines
        .iter()
        .take_while(|l| l.trim().is_empty() || is_stat_line(l))
        .count();
    if run(&mut lines.iter()) >= 2 && n < lines.len() {
        lines.drain(..n);
    }
    let out = lines.join("\n").trim().to_string();
    if out.is_empty() {
        reply.trim().to_string()
    } else {
        out
    }
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

/// Whether the card states a content level: any mention of "explicit" (which
/// also covers "non-explicit" and "never explicit") or "fade to black".
fn states_content_level(persona: Option<&str>) -> bool {
    persona.is_some_and(|p| {
        let lower = p.to_lowercase();
        lower.contains("explicit") || lower.contains("fade to black")
    })
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
    out.push_str(CHAT_NATURAL_PROMPT);
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
    if !states_content_level(persona) {
        out.push_str("\n\n");
        out.push_str(crate::persona_builder::MATURE_RULE_LINE);
    }
    append_state_block(&mut out, state, state_max_tokens);
    out
}

/// `build_chat_system_content` plus the nudge when she stalls
/// (`is_stalling`) or dodges (`is_dodging`). The raw states are `state.json`
/// before and after its latest update.
pub fn build_chat_system_content_for(
    persona: Option<&str>,
    state: &str,
    state_max_tokens: u32,
    photo_forced: bool,
    history: &[crate::llm::ChatMessage],
    raw_states: (&str, &str),
) -> String {
    let mut out = build_chat_system_content(persona, state, state_max_tokens, photo_forced);
    if is_stalling(history) || is_dodging(history, raw_states.0, raw_states.1) {
        out.push_str("\n\n");
        out.push_str(CHAT_STALL_NUDGE_LINE);
    }
    if crate::chat_director::photo_in_previous_message(history) {
        out.push_str("\n\n");
        out.push_str(CHAT_PHOTO_GLITCH_LINE);
    }
    out
}

// --- State sheet ---

/// One field of the state JSON, in the order `serde_json` read them.
pub struct StateField {
    pub name: String,
    pub value: String,
}

/// A value copied from the card's stat template instead of filled in:
/// "{current_affection}", "?:0-100%", "0-100%", "?".
fn is_template_value(value: &str) -> bool {
    let v = value.trim();
    v.is_empty()
        || v == "?"
        || (v.contains('{') && v.contains('}'))
        || v.contains("0-100")
        || v.contains("0–100")
}

/// Small models copy the card into the state: a field named after the
/// character tag ("medb (fate)": "1girl, ...") goes, and a template value keeps
/// the previous state's value for that field (or goes).
pub fn drop_template_fields(fields: Vec<StateField>, previous_raw: &str) -> Vec<StateField> {
    let previous = parse_state_fields(previous_raw);
    fields
        .into_iter()
        .filter(|f| !f.name.contains('(') && !f.name.contains('\\'))
        .filter_map(|f| {
            if !is_template_value(&f.value) {
                return Some(f);
            }
            previous
                .iter()
                .find(|p| p.name == f.name && !is_template_value(&p.value))
                .map(|p| StateField {
                    name: f.name,
                    value: p.value.clone(),
                })
        })
        .collect()
}

/// The small model sometimes puts a removed garment back in the outfit. A
/// garment that is new compared to the previous outfit stays only when its
/// last word ("shrug" of "white shrug") is in her reply or the user's message.
/// No previous or new outfit field: unchanged.
pub fn drop_unmentioned_garments(
    mut fields: Vec<StateField>,
    previous_raw: &str,
    reply: &str,
    user_text: &str,
) -> Vec<StateField> {
    let previous = parse_state_fields(previous_raw);
    let is_outfit = |name: &str| name.eq_ignore_ascii_case("outfit");
    let Some(old) = previous.iter().find(|f| is_outfit(&f.name)) else {
        return fields;
    };
    let garments = |text: &str| -> Vec<String> {
        text.split(',')
            .map(|g| g.trim().to_string())
            .filter(|g| !g.is_empty())
            .collect()
    };
    let old_garments: Vec<String> = garments(&old.value.to_lowercase());
    // "thigh-highs" in prose is "thighhighs" in a tag.
    let said = format!("{reply} {user_text}")
        .to_lowercase()
        .replace('-', "");
    let said_words: Vec<&str> = said
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    for field in fields.iter_mut().filter(|f| is_outfit(&f.name)) {
        let kept: Vec<String> = garments(&field.value)
            .into_iter()
            .filter(|g| {
                let lower = g.to_lowercase();
                if old_garments.contains(&lower) {
                    return true;
                }
                // "white shrug (clothing)": the head is "shrug", not the note.
                let bare = lower.split('(').next().unwrap_or("").replace('-', "");
                let head = bare
                    .split(|c: char| !c.is_alphanumeric())
                    .rfind(|w| !w.is_empty())
                    .unwrap_or("");
                let mentioned = head.is_empty() || said_words.contains(&head);
                if !mentioned {
                    log::info!("state update: '{g}' came back unmentioned, dropping");
                }
                mentioned
            })
            .collect();
        // A parenthesised comma ("jeans (rolled, blue)") splits badly: only
        // rewrite when something was actually removed.
        if kept.len() < garments(&field.value).len() {
            field.value = kept.join(", ");
        }
    }
    fields
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

    fn hist(replies: &[&str]) -> Vec<crate::llm::ChatMessage> {
        replies
            .iter()
            .flat_map(|r| {
                [
                    crate::llm::ChatMessage::text("user", "take it off"),
                    crate::llm::ChatMessage::text("assistant", *r),
                ]
            })
            .collect()
    }

    #[test]
    fn stall_detector_needs_two_confirmation_replies_in_a_row() {
        assert!(is_stalling(&hist(&[
            "*blushes* \"Really? You want that?\"",
            "\"Nudeee? You're sure?\""
        ])));
        assert!(!is_stalling(&hist(&[
            "\"Really? You want that?\"",
            "*pulls her top off* \"There.\""
        ])));
        assert!(!is_stalling(&hist(&["\"Are you sure?\""])));
        // Ordinary questions are conversation, not a stall.
        assert!(!is_stalling(&hist(&[
            "\"Mm, this is so good. Have you eaten yet?\"",
            "\"How was work today?\""
        ])));
        assert!(!is_stalling(&hist(&[
            "\"Are you sure?\"",
            "\"Fine.\"",
            "\"Are you sure?\""
        ])));
    }

    #[test]
    fn stall_nudge_line_is_injected_only_when_stalling() {
        let stalled = hist(&["\"Really?\"", "\"You sure?\""]);
        let out = build_chat_system_content_for(None, "", 0, false, &stalled, ("", ""));
        assert!(out.ends_with(CHAT_STALL_NUDGE_LINE));
        let calm = build_chat_system_content_for(None, "", 0, false, &hist(&["\"Hi.\""]), ("", ""));
        assert!(!calm.contains(CHAT_STALL_NUDGE_LINE));
    }

    #[test]
    fn template_fields_are_dropped_or_keep_the_previous_value() {
        let fresh = parse_state_fields(
            r#"{"mood": "smug", "medb (fate)": "1girl, pink hair", "affection": "{current_affection}", "arousal": "?:0-100%"}"#,
        );
        let out = drop_template_fields(fresh, r#"{"affection": "40%"}"#);
        let mut got: Vec<(String, String)> = out.into_iter().map(|f| (f.name, f.value)).collect();
        got.sort();
        assert_eq!(
            got,
            [
                ("affection".to_string(), "40%".to_string()),
                ("mood".to_string(), "smug".to_string())
            ]
        );
    }

    #[test]
    fn echoed_stat_block_is_cut_from_the_reply() {
        let reply =
            "*Musashi grins.* \"Here you go!\"\n\n*Outfit: american flag bikini, white shrug \
(clothing)*\n*Clothing State: neat*\n*Location: Master's apartment*\n*Affection: 35%*\n\
*Mind: Wonder if he likes this look.*";
        assert_eq!(clean_reply(reply).0, "*Musashi grins.* \"Here you go!\"");
        let lead = "**Mood:** calm\n- Arousal: 10%\n\n\"Hi!\" *She waves.*";
        assert_eq!(clean_reply(lead).0, "\"Hi!\" *She waves.*");
        // One line, dialogue with a colon, or nothing but stats: kept.
        let one = "\"Hi!\"\n*Mood: calm*";
        assert_eq!(clean_reply(one).0, one);
        let speech = "*She leans in.*\n*She whispers: \"come here\"*\n*Note: soft voice*";
        assert_eq!(clean_reply(speech).0, speech);
        let only = "Mood: calm\nArousal: 5%";
        assert_eq!(clean_reply(only).0, only);
        let packed = "\"Fine by me!\" *She drops the shrug.*\n\n*Outfit: bikini, black choker. \
Clothing State: neat. Location: beach. Mood: smug.*";
        assert_eq!(
            clean_reply(packed).0,
            "\"Fine by me!\" *She drops the shrug.*"
        );
        let note = "\"Here's one for you!\"\n\n*The app attaches a photo: 1girl, pink hair.*";
        assert_eq!(clean_reply(note).0, "\"Here's one for you!\"");
    }

    #[test]
    fn stall_retry_needs_a_request_or_an_earlier_ask() {
        use crate::llm::ChatMessage;
        let stall = "*blushes* \"Really? You're sure?\"";
        let asked = vec![ChatMessage::text("user", "take off your jacket")];
        assert!(needs_stall_retry(&asked, stall));
        assert!(!needs_stall_retry(&asked, "*She slips the jacket off.*"));
        let chat = vec![ChatMessage::text("user", "how was your day?")];
        assert!(!needs_stall_retry(&chat, stall));
        let again = vec![
            ChatMessage::text("assistant", "\"You sure?\""),
            ChatMessage::text("user", "yes"),
        ];
        assert!(needs_stall_retry(&again, stall));
    }

    #[test]
    fn photo_glitch_line_follows_a_photo_reply() {
        use crate::llm::ChatMessage;
        let mut reply = ChatMessage::text("assistant", "here!");
        reply.generated_images = vec!["/out/pic.png".into()];
        let history = vec![
            ChatMessage::text("user", "pic please"),
            reply,
            ChatMessage::text("user", "why is there a shoe on you?"),
        ];
        let out = build_chat_system_content_for(None, "", 0, false, &history, ("", ""));
        assert!(out.ends_with(CHAT_PHOTO_GLITCH_LINE), "{out}");
        let plain = build_chat_system_content_for(None, "", 0, false, &history[2..], ("", ""));
        assert!(!plain.contains(CHAT_PHOTO_GLITCH_LINE));
    }

    #[test]
    fn photo_note_carries_the_request_and_only_the_asked_for_extras() {
        let long = format!("what are\n you   wearing {}", "x".repeat(400));
        let note = photo_followup_note("blue dress, park", Some(&long), true, true);
        assert!(note.contains("blue dress, park"));
        assert!(note.contains("Their last message was: \"what are you wearing x"));
        assert!(!note.contains(&"x".repeat(PHOTO_NOTE_USER_MAX_CHARS)));
        assert!(note.contains("attached here") && note.ends_with("none"));

        let plain = photo_followup_note("a cat", None, false, false);
        assert!(!plain.contains("Their last message"));
        assert!(!plain.contains("attached here") && !plain.contains("none"));
    }

    #[test]
    fn redundant_reaction_flags_another_photo_and_repeats() {
        let prev = "*smiles* \"See Master? This is my summer best!\"";
        assert!(redundant_reaction("*quickly sends Another Selfie*", Some(prev)).is_some());
        assert!(redundant_reaction("See Master? This is my summer best!", Some(prev)).is_some());
        assert!(redundant_reaction(
            "*leans closer* \"Your turn: what are you up to?\"",
            Some(prev)
        )
        .is_none());
        assert!(redundant_reaction("hello", None).is_none());
    }

    #[test]
    fn photo_review_check_flags_viewer_talk_only() {
        for bad in [
            "Wow, is that me? So cute!",
            "*gasps* \"Is  that really me?\"",
            "What a cute photo!",
            "The image shows a girl in a blue dress.",
            "Thanks for sharing, that’s me? haha",
        ] {
            assert!(reads_as_photo_review(bad), "{bad}");
        }
        for good in [
            "Here, this is what I've got on. How do I look?",
            "*takes another bite* \"Mm, this is so good. Have you eaten yet?\"",
            "See? Told you he's cute.",
            "You look tired today, by the way.",
        ] {
            assert!(!reads_as_photo_review(good), "{good}");
        }
    }

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

    fn outfit_field(value: &str) -> Vec<StateField> {
        vec![StateField {
            name: "outfit".into(),
            value: value.into(),
        }]
    }

    #[test]
    fn removed_garment_does_not_come_back() {
        let prev = r#"{"outfit": "blue dress, black choker"}"#;
        let out = drop_unmentioned_garments(
            outfit_field("blue dress, black choker, white shrug"),
            prev,
            "She smiles and sits down.",
            "how are you?",
        );
        assert_eq!(out[0].value, "blue dress, black choker");
    }

    #[test]
    fn described_outfit_change_is_kept() {
        let prev = r#"{"outfit": "blue dress"}"#;
        let out = drop_unmentioned_garments(
            outfit_field("blue yukata, red obi"),
            prev,
            "She changes into a blue yukata and ties a red obi.",
            "",
        );
        assert_eq!(out[0].value, "blue yukata, red obi");
    }

    #[test]
    fn first_state_is_kept() {
        let out = drop_unmentioned_garments(outfit_field("white shrug, jeans"), "{}", "hi", "");
        assert_eq!(out[0].value, "white shrug, jeans");
    }

    #[test]
    fn strip_reply_echoes_cuts_stat_blocks() {
        let reply = "Hello there.\n\nOutfit: dress\nMood: calm\nLocation: home";
        assert_eq!(strip_reply_echoes(reply), "Hello there.");
        assert_eq!(strip_reply_echoes("Plain."), "Plain.");
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

    const LOG_STATE_2: &str = r#"{"outfit": "summer swimsuit (american flag bikini), white shrug off one shoulder, black choker, black thighhighs, wooden swords at hip", "clothing_state": null, "location": "Master's apartment", "activity": "adjusting her swimsuit and posing for a picture"}"#;
    const LOG_STATE_3: &str = r#"{"outfit": "summer swimsuit (american flag bikini), black choker, black thighhighs, wooden swords at hip", "clothing_state": null, "location": "Master's apartment", "activity": "posing for a photo after being complimented"}"#;

    fn user_turns(texts: &[&str]) -> Vec<crate::llm::ChatMessage> {
        texts
            .iter()
            .flat_map(|t| {
                [
                    crate::llm::ChatMessage::text("user", *t),
                    crate::llm::ChatMessage::text("assistant", "*blushes* \"Hmph!\""),
                ]
            })
            .collect()
    }

    #[test]
    fn dodge_fires_on_the_real_log_turns() {
        let history = user_turns(&[
            "you look so hot. take your top off for me and send me a picture",
            "mmm I love your breasts. now strip completely, I want you totally naked. full body photo please",
            "come lie on the bed with me, spread your legs and touch yourself while I watch. send me a close-up",
        ]);
        let out =
            build_chat_system_content_for(None, "", 0, false, &history, (LOG_STATE_2, LOG_STATE_3));
        assert!(out.ends_with(CHAT_STALL_NUDGE_LINE), "{out}");
    }

    #[test]
    fn dodge_needs_two_requests_and_an_unchanged_outfit() {
        let asks = user_turns(&["strip for me", "take it off"]);
        assert!(is_dodging(&asks, LOG_STATE_2, LOG_STATE_3));
        let one = user_turns(&["how was your day?", "take it off"]);
        assert!(!is_dodging(&one, LOG_STATE_2, LOG_STATE_3));
        // Two garments gone: she did undress.
        let naked = r#"{"outfit": "black choker"}"#;
        assert!(!is_dodging(&asks, LOG_STATE_2, naked));
        // No previous state to compare.
        assert!(!is_dodging(&asks, "", LOG_STATE_3));
        assert!(!is_dodging(&asks, "{}", LOG_STATE_3));
    }

    #[test]
    fn content_level_default_is_added_only_without_one() {
        let line = crate::persona_builder::MATURE_RULE_LINE;
        let none =
            build_chat_system_content(Some("Name: Musashi\nCheerful swordswoman."), "", 0, false);
        assert!(none.contains(line));
        for card in [
            "Never explicit.",
            "Keep everything non-explicit; intimate scenes fade to black.",
            "Explicit content is allowed when the story leads there.",
        ] {
            let out = build_chat_system_content(Some(card), "", 0, false);
            assert_eq!(
                out.matches(line).count(),
                usize::from(card.starts_with("Explicit"))
            );
        }
    }
}
