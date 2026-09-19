//! Fits a turn into a token budget by, in order: condensing finished chain
//! steps to command + output, optionally summarizing, then dropping oldest
//! first. Every loss is reported, never silent.
//!
//! Summarizing sits second because it's the *risky* rung, not the safe one:
//! a model-written summary becomes the record with no transcript left to
//! check it against, whereas a dropped turn leaves a marker saying to go
//! look. Hence opt-in, temperature 0, and shown to the user.

use crate::llm::ChatMessage;
use crate::rules;

const TRIM_MARKER: &str = "[...older turns of this conversation were dropped to stay within the \
context window. Don't assume anything about what was removed -- if an earlier detail matters, ask \
the user or re-check it with a command.]";

/// Distinct from `TRIM_MARKER` -- this drop isn't a budget-forced loss, it's
/// turns whose facts already live safely elsewhere (see `fit_to_budget`'s
/// `archived_before` parameter), so the wording says where to actually look
/// instead of just "don't assume anything".
const ARCHIVED_TASK_MARKER: &str =
    "[...turns from an earlier, already-completed task were dropped \
here -- that task's commands and their real exit codes are still in the session record above, this \
was only its chat transcript.]";

/// Must match `formatCommandFeedback` in `ui/main.js` and `headless.rs`
/// exactly -- condensing recognizes a finished step by this prefix.
pub const COMMAND_OUTPUT_PREFIX: &str = "[command output, exit ";

const CONDENSED_MARKER: &str = "[earlier step, condensed to its command and result]";

const CONDENSED_COMMAND_CHARS: usize = 300;
const CONDENSED_OUTPUT_CHARS: usize = 400;
/// Floor on each step's share in a multi-step batch, so a long sequence
/// doesn't shrink every step to nothing. Can push a batch over the cap;
/// `trim_to_budget` re-measures after each step and keeps going.
const CONDENSED_MIN_BLOCK_CHARS: usize = 60;

pub(crate) fn truncate_with_note(text: &str, max: usize, what: &str) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!(
        "{kept}\n[... {} more characters of {what} condensed away]",
        total - max
    )
}

/// Cheap chars/4 estimate, not a real tokenizer -- every endpoint tokenizes
/// differently. Runs optimistic on shell output, so budgets need headroom.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// A base64 image easily runs to hundreds of thousands of characters, and a
/// real vision model's actual per-image token cost is nowhere near
/// proportional to that -- it's a much smaller, roughly fixed amount that
/// depends on the model and the image's resolution, not its encoded length.
/// Running the encoded blob through `chars/4` like ordinary text would make
/// trimming think one attached picture costs 100k+ tokens and start
/// aggressively condensing or dropping history the moment someone attaches
/// one. This flat per-image estimate is a deliberately rough placeholder
/// instead -- cheap and wrong in a bounded way, matching the spirit of
/// `estimate_tokens` itself, rather than wrong in an unbounded one.
const ESTIMATED_TOKENS_PER_IMAGE: usize = 1000;

/// Every message-token estimate in this module goes through here rather
/// than `estimate_tokens(&msg.content)` directly, so an image attachment
/// (chat mode only -- operation mode never has one) is never invisible to
/// the budget it's actually part of.
fn estimate_message_tokens(msg: &ChatMessage) -> usize {
    estimate_tokens(&msg.content) + msg.images.len() * ESTIMATED_TOKENS_PER_IMAGE
}

pub struct TrimOutcome {
    pub messages: Vec<ChatMessage>,
    pub dropped: usize,
    pub condensed: usize,
    /// Including the system block.
    pub estimated_tokens: usize,
}

/// An assistant message proposing a command, followed by that command's result.
fn is_finished_step(messages: &[ChatMessage], i: usize) -> bool {
    let Some(proposal) = messages.get(i) else {
        return false;
    };
    let Some(result) = messages.get(i + 1) else {
        return false;
    };
    proposal.role == "assistant"
        && result.role == "user"
        && result.content.starts_with(COMMAND_OUTPUT_PREFIX)
        && rules::extract_command(&proposal.content).is_some()
}

/// Oldest condensable step. The last pair is off limits -- its output is what
/// the turn is answering.
fn next_condensable(messages: &[ChatMessage]) -> Option<usize> {
    (0..messages.len().saturating_sub(2)).find(|&i| is_finished_step(messages, i))
}

/// Keeps every `[command output, exit N]` line and truncates only the output
/// between them. `executeSequence` reports a whole batch in one message, so
/// treating it as a single string let a long first output push a later step's
/// failure past the cap -- exactly the "it must have worked" gap this module
/// exists to close.
fn condense_result(content: &str, budget: usize) -> String {
    let mut blocks: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in content.lines() {
        if line.starts_with(COMMAND_OUTPUT_PREFIX) {
            blocks.push((line, Vec::new()));
        } else if let Some((_, body)) = blocks.last_mut() {
            body.push(line);
        }
    }
    if blocks.is_empty() {
        return truncate_with_note(content.trim_end(), budget, "output");
    }

    let share = (budget / blocks.len()).max(CONDENSED_MIN_BLOCK_CHARS);
    let mut out = String::new();
    for (header, body) in blocks {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(header);
        let body = truncate_with_note(body.join("\n").trim_end(), share, "output");
        if !body.is_empty() {
            out.push('\n');
            out.push_str(&body);
        }
    }
    out
}

/// Command + result, narration discarded.
fn condense_step(proposal: &ChatMessage, result: &ChatMessage) -> ChatMessage {
    let cmd = rules::extract_command(&proposal.content)
        .unwrap_or_else(|| "(command could not be re-read)".to_string());
    ChatMessage::text(
        "user",
        format!(
            "{CONDENSED_MARKER} $ {}\n{}",
            truncate_with_note(&cmd, CONDENSED_COMMAND_CHARS, "command"),
            condense_result(&result.content, CONDENSED_OUTPUT_CHARS)
        ),
    )
}

/// Condense, then drop, until it fits `budget` (0 disables). The first
/// message (the original request) and the last (the turn being answered) are
/// always kept, even if the last alone busts the budget.
pub fn trim_to_budget(
    system_tokens: usize,
    history: Vec<ChatMessage>,
    budget: usize,
) -> TrimOutcome {
    let mut history = history;
    let history_tokens: usize = history.iter().map(estimate_message_tokens).sum();
    let mut total = system_tokens + history_tokens;

    let mut condensed = 0;
    if budget != 0 {
        while total > budget {
            let Some(i) = next_condensable(&history) else {
                break;
            };
            let before =
                estimate_message_tokens(&history[i]) + estimate_message_tokens(&history[i + 1]);
            let merged = condense_step(&history[i], &history[i + 1]);
            total = total + estimate_tokens(&merged.content) - before;
            history.splice(i..=i + 1, [merged]);
            condensed += 1;
        }
    }

    if budget == 0 || total <= budget || history.len() <= 2 {
        return TrimOutcome {
            messages: history,
            dropped: 0,
            condensed,
            estimated_tokens: total,
        };
    }

    let first = history[0].clone();
    let mut used = system_tokens + estimate_message_tokens(&first) + estimate_tokens(TRIM_MARKER);

    let mut kept_tail: Vec<ChatMessage> = Vec::new();
    for (i, msg) in history.iter().enumerate().skip(1).rev() {
        let cost = estimate_message_tokens(msg);
        // The last message is the turn being answered: keep it regardless.
        if used + cost > budget && i != history.len() - 1 {
            break;
        }
        used += cost;
        kept_tail.push(msg.clone());
    }
    kept_tail.reverse();

    let dropped = history.len() - kept_tail.len() - 1;
    let mut messages = Vec::with_capacity(kept_tail.len() + 2);
    messages.push(first);
    if dropped > 0 {
        messages.push(ChatMessage::text("user", TRIM_MARKER));
    }
    messages.extend(kept_tail);

    TrimOutcome {
        messages,
        dropped,
        condensed,
        estimated_tokens: used,
    }
}

// --- Summarization (opt-in, `AppConfig.summarize_before_dropping`) ---

/// Reuses the conversation's own endpoint/model rather than adding a second
/// one to get wrong.
pub struct Summarizer<'a> {
    pub endpoint: &'a str,
    pub model: &'a str,
    pub api_key: &'a str,
}

/// Below this a round-trip isn't worth it -- a gap marker says as much.
const MIN_MESSAGES_TO_SUMMARIZE: usize = 4;

/// Also the space `pick_summary_span` reserves, so the two must agree.
const SUMMARY_MAX_CHARS: usize = 1200;

/// Written against observed fabrication, not as general advice: keep pointing
/// at command output as the only evidence, and ask for omission over guessing.
const SUMMARY_PROMPT: &str = "You are compressing the oldest part of a transcript between a user \
and a file assistant, so it can be kept in a limited context window. Write the summary that will \
replace it.\n\n\
- Report only what the text actually shows. Never say a file was created, moved, renamed, or \
deleted unless a command's output in the text shows that it happened.\n\
- Command output is the record. Where the assistant's prose disagrees with the output it got, go \
with the output.\n\
- Say plainly when something failed, was denied, or never ran. Those matter more than successes.\n\
- Keep exact names verbatim: files, folders, paths, commands.\n\
- Keep what the user asked for and any constraints or preferences they stated.\n\
- If the text doesn't establish something, leave it out. Never fill a gap with a guess.\n\
- Answer with short factual bullet points and nothing else: no preamble, no closing remarks, no \
code fences, no offers to help.";

fn summary_marker(count: usize) -> String {
    format!(
        "[summary of {count} earlier messages, written by the model to save context -- a lossy \
         record, not evidence. Don't treat anything here as proof that something happened; if a \
         detail matters, re-check it with a command.]"
    )
}

/// How many of the oldest messages (never the first or last) to summarize.
/// Measured against the un-condensed history so the span maps onto the stored
/// record; that over-takes slightly, which is the safe direction.
fn pick_summary_span(history: &[ChatMessage], system_tokens: usize, budget: usize) -> usize {
    let total: usize = system_tokens + history.iter().map(estimate_message_tokens).sum::<usize>();
    let reserve = SUMMARY_MAX_CHARS.div_ceil(4) + estimate_tokens(&summary_marker(0));
    let mut need = (total + reserve).saturating_sub(budget);

    let mut taken = 0;
    for msg in history.iter().skip(1).take(history.len().saturating_sub(2)) {
        if need == 0 {
            break;
        }
        need = need.saturating_sub(estimate_message_tokens(msg));
        taken += 1;
    }
    taken
}

async fn summarize(s: &Summarizer<'_>, victims: &[ChatMessage]) -> anyhow::Result<String> {
    let transcript = victims
        .iter()
        .map(|m| format!("{}: {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n\n");
    let messages = vec![
        ChatMessage::text("system", SUMMARY_PROMPT),
        ChatMessage::text("user", transcript),
    ];
    // Temperature 0: a creative record is the failure mode.
    let text = crate::llm::send_chat(s.endpoint, s.model, s.api_key, 0.0, &messages).await?;
    let text = text.trim();
    if text.is_empty() {
        anyhow::bail!("summarizer returned nothing");
    }
    Ok(truncate_with_note(text, SUMMARY_MAX_CHARS, "summary"))
}

pub struct FitOutcome {
    /// What to send this turn.
    pub messages: Vec<ChatMessage>,
    /// `Some` only when summarizing ran; the caller must adopt it, so the
    /// summary is paid for once instead of regenerated differently each turn.
    /// Excludes condensing, which is free to redo and keeps the transcript.
    pub rewritten_history: Option<Vec<ChatMessage>>,
    /// For showing and editing in the UI.
    pub summary: Option<String>,
    pub summarized: usize,
    pub condensed: usize,
    pub dropped: usize,
    pub estimated_tokens: usize,
}

impl FitOutcome {
    fn mechanical(trim: TrimOutcome) -> Self {
        Self {
            messages: trim.messages,
            rewritten_history: None,
            summary: None,
            summarized: 0,
            condensed: trim.condensed,
            dropped: trim.dropped,
            estimated_tokens: trim.estimated_tokens,
        }
    }
}

/// Keeps the sacred first message (`trim_to_budget` never drops it either),
/// replaces everything up to (not including) `boundary` with one
/// `ARCHIVED_TASK_MARKER` message, and keeps `boundary..` untouched. Called
/// only when `boundary` is known to be the start of the *current*,
/// not-yet-archived task -- see `fit_to_budget`'s `archived_before`
/// parameter. A no-op if there's nothing real to drop (`boundary < 2`) or
/// `boundary` is out of range.
fn drop_archived_prefix(history: Vec<ChatMessage>, boundary: usize) -> Vec<ChatMessage> {
    if boundary < 2 || boundary >= history.len() {
        return history;
    }
    let mut out = Vec::with_capacity(history.len() - boundary + 2);
    out.push(history[0].clone());
    out.push(ChatMessage::text("user", ARCHIVED_TASK_MARKER));
    out.extend_from_slice(&history[boundary..]);
    out
}

/// Condense, then summarize (only if opted in *and* the mechanical passes
/// already gave up -- `dropped > 0` is that signal), then drop. A failed
/// summarizer falls back to the mechanical result.
///
/// `archived_before`, when `Some(idx)`, is the index of the current
/// (not-yet-archived) task's own opening message -- operation mode's
/// `memory.rs` already has an independent, app-written record of every
/// command that ran (with its real exit code) for everything before it, so
/// that prefix is dropped outright as a first pass whenever the turn is
/// over budget at all, rather than being condensed/summarized/dropped
/// piecemeal like turns with no such backstop. `None` for chat mode, which
/// has no `memory.rs` equivalent (its own `state.json`/`state.md` is a
/// different mechanism entirely). Doesn't change condensing/summarizing/
/// dropping's own logic at all -- it only shrinks what those passes ever
/// see, and reports the extra drop honestly rather than letting it vanish
/// from the turn's `dropped` count.
pub async fn fit_to_budget(
    system_tokens: usize,
    history: Vec<ChatMessage>,
    budget: usize,
    summarizer: Option<Summarizer<'_>>,
    archived_before: Option<usize>,
) -> FitOutcome {
    let mut archived_dropped = 0;
    let history = match archived_before {
        Some(idx) if budget != 0 && idx >= 2 && idx < history.len() => {
            let total: usize =
                system_tokens + history.iter().map(estimate_message_tokens).sum::<usize>();
            if total > budget {
                archived_dropped = idx - 1;
                drop_archived_prefix(history, idx)
            } else {
                history
            }
        }
        _ => history,
    };
    let mut outcome = fit_to_budget_inner(system_tokens, history, budget, summarizer).await;
    outcome.dropped += archived_dropped;
    outcome
}

async fn fit_to_budget_inner(
    system_tokens: usize,
    history: Vec<ChatMessage>,
    budget: usize,
    summarizer: Option<Summarizer<'_>>,
) -> FitOutcome {
    let mechanical = trim_to_budget(system_tokens, history.clone(), budget);
    let Some(s) = summarizer else {
        return FitOutcome::mechanical(mechanical);
    };
    if budget == 0 || mechanical.dropped == 0 {
        return FitOutcome::mechanical(mechanical);
    }

    let span = pick_summary_span(&history, system_tokens, budget);
    if span < MIN_MESSAGES_TO_SUMMARIZE {
        return FitOutcome::mechanical(mechanical);
    }

    let summary = match summarize(&s, &history[1..=span]).await {
        Ok(text) => text,
        Err(e) => {
            log::warn!("summarizing older turns failed, falling back to dropping them: {e}");
            return FitOutcome::mechanical(mechanical);
        }
    };

    let mut rewritten = history;
    rewritten.splice(
        1..=span,
        // A user message, so a ```sh fence in the summary can never be
        // parsed as a command -- only assistant replies are scanned.
        [ChatMessage::text(
            "user",
            format!("{}\n{summary}", summary_marker(span)),
        )],
    );

    let trim = trim_to_budget(system_tokens, rewritten.clone(), budget);
    FitOutcome {
        messages: trim.messages,
        rewritten_history: Some(rewritten),
        summary: Some(summary),
        summarized: span,
        condensed: trim.condensed,
        dropped: trim.dropped,
        estimated_tokens: trim.estimated_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage::text(role, content)
    }

    // An attached image is chat-mode-only in practice, but `trim_to_budget`
    // is shared code -- this pins the exact bug the flat-per-image estimate
    // exists to prevent: naive chars/4 on a base64 blob would make one
    // attached picture look like ~100k tokens and get the rest of a short,
    // legitimate conversation dropped to "fit" around it.
    #[test]
    fn an_attached_image_does_not_blow_the_token_estimate() {
        let mut with_image = msg("user", "what's in this picture?");
        with_image.images = vec![format!("data:image/png;base64,{}", "A".repeat(500_000))];

        let h = vec![msg("user", "hello"), msg("assistant", "hi"), with_image];
        let out = trim_to_budget(20, h, 5000);
        assert_eq!(
            out.dropped, 0,
            "a short conversation plus one image should still fit 5000"
        );
        assert!(
            out.estimated_tokens < 50_000,
            "flat-per-image estimate should dominate, not chars/4 of the base64 blob: {}",
            out.estimated_tokens
        );
    }

    fn history(n: usize) -> Vec<ChatMessage> {
        (0..n)
            .map(|i| {
                msg(
                    if i % 2 == 0 { "user" } else { "assistant" },
                    &"x".repeat(400),
                )
            })
            .collect()
    }

    #[test]
    fn under_budget_is_left_completely_alone() {
        let h = history(4);
        let out = trim_to_budget(100, h.clone(), 100_000);
        assert_eq!(out.dropped, 0);
        assert_eq!(out.messages.len(), h.len());
    }

    #[test]
    fn budget_of_zero_disables_trimming() {
        let h = history(50);
        let out = trim_to_budget(100, h.clone(), 0);
        assert_eq!(out.dropped, 0);
        assert_eq!(out.messages.len(), h.len());
    }

    #[test]
    fn over_budget_drops_oldest_and_marks_the_gap() {
        let h = history(40); // ~100 tokens each
        let out = trim_to_budget(50, h, 600);
        assert!(out.dropped > 0, "expected something to be dropped");
        assert!(
            out.estimated_tokens <= 600,
            "still over budget: {}",
            out.estimated_tokens
        );
        assert_eq!(
            out.messages
                .iter()
                .filter(|m| m.content == TRIM_MARKER)
                .count(),
            1,
            "exactly one gap marker expected"
        );
    }

    #[test]
    fn keeps_the_original_request_and_the_turn_being_answered() {
        let mut h = history(40);
        h[0] = msg("user", "ORIGINAL REQUEST");
        let last = h.len() - 1;
        h[last] = msg("user", "CURRENT TURN");

        let out = trim_to_budget(50, h, 600);
        assert_eq!(
            out.messages.first().unwrap().content,
            "ORIGINAL REQUEST",
            "the original request must survive trimming"
        );
        assert_eq!(
            out.messages.last().unwrap().content,
            "CURRENT TURN",
            "the turn being answered must survive trimming"
        );
    }

    #[test]
    fn oversized_final_turn_is_kept_even_though_it_busts_the_budget() {
        let mut h = history(6);
        let last = h.len() - 1;
        h[last] = msg("user", &"y".repeat(40_000));
        let out = trim_to_budget(10, h, 500);
        assert_eq!(out.messages.last().unwrap().content.len(), 40_000);
    }

    /// One propose -> run step, the way both `main.js` and `headless.rs`
    /// actually write it into the history.
    fn step(narration: &str, cmd: &str, output: &str) -> Vec<ChatMessage> {
        vec![
            msg("assistant", &format!("{narration}\n```sh\n{cmd}\n```")),
            msg("user", &format!("{COMMAND_OUTPUT_PREFIX}0]\n{output}")),
        ]
    }

    fn chain(steps: usize) -> Vec<ChatMessage> {
        let mut h = vec![msg("user", "organize this folder")];
        for i in 0..steps {
            h.extend(step(
                &"narration ".repeat(40),
                &format!("ls -F dir{i}"),
                "a\nb",
            ));
        }
        h.push(msg("user", "so what happened?"));
        h
    }

    #[test]
    fn condensing_is_preferred_over_dropping() {
        let h = chain(6);
        let out = trim_to_budget(20, h, 400);
        assert!(out.condensed > 0, "expected steps to be condensed");
        assert_eq!(
            out.dropped, 0,
            "condensing alone should have been enough here"
        );
        assert!(out.estimated_tokens <= 400);
    }

    #[test]
    fn condensing_keeps_the_command_and_its_output_verbatim() {
        let h = chain(6);
        let out = trim_to_budget(20, h, 400);
        let condensed = out
            .messages
            .iter()
            .find(|m| m.content.starts_with(CONDENSED_MARKER))
            .expect("expected a condensed step");
        assert!(condensed.content.contains("$ ls -F dir0"), "{condensed:?}");
        assert!(condensed.content.contains("exit 0"), "{condensed:?}");
        assert!(condensed.content.contains("a\nb"), "{condensed:?}");
        assert!(!condensed.content.contains("narration"), "{condensed:?}");
    }

    // `executeSequence` reports a whole batch in one message. Truncating that
    // as a single string let a long first output push a later step's failure
    // past the cap, leaving the model to conclude it had worked.
    #[test]
    fn condensing_keeps_every_step_exit_code() {
        let batch = format!(
            "{COMMAND_OUTPUT_PREFIX}0] $ mkdir -p A\n{}\n\n\
             {COMMAND_OUTPUT_PREFIX}1] $ mv x A/\nmv: cannot stat 'x'",
            "created a directory and said a great deal about it\n".repeat(40)
        );
        let mut h = vec![msg("user", "organize this")];
        h.push(msg(
            "assistant",
            "doing it\n```sh\nmkdir -p A\nmv x A/\n```",
        ));
        h.push(msg("user", &batch));
        h.push(msg("user", "did that work?"));

        let out = trim_to_budget(20, h, 300);
        let condensed = out
            .messages
            .iter()
            .find(|m| m.content.starts_with(CONDENSED_MARKER))
            .expect("expected a condensed step");
        assert!(
            condensed.content.contains("exit 1] $ mv x A/"),
            "the failing step must survive truncation: {condensed:?}"
        );
        assert!(
            condensed.content.contains("cannot stat"),
            "and enough of its output to say why: {condensed:?}"
        );
    }

    #[test]
    fn the_step_being_answered_is_never_condensed() {
        // No trailing user turn: the command result *is* the last message,
        // so it's what the model is about to react to.
        let mut h = vec![msg("user", "what's here?")];
        for i in 0..8 {
            h.extend(step(&"narration ".repeat(40), &format!("ls dir{i}"), "x"));
        }
        let out = trim_to_budget(20, h, 200);
        let last = out.messages.last().unwrap();
        assert!(
            !last.content.starts_with(CONDENSED_MARKER),
            "the turn being answered must stay verbatim: {last:?}"
        );
    }

    #[test]
    fn dropping_still_happens_when_condensing_is_not_enough() {
        let mut h = chain(4);
        // A pile of plain user/assistant turns has nothing condensable in
        // it, so the budget can only be met by dropping.
        h.splice(1..1, history(30));
        let out = trim_to_budget(20, h, 300);
        assert!(out.dropped > 0, "expected messages to be dropped too");
        assert!(out.estimated_tokens <= 300);
    }

    #[test]
    fn long_output_is_cut_with_an_explicit_note() {
        let mut h = vec![msg("user", "find everything")];
        h.extend(step("looking", "find .", &"./some/path\n".repeat(500)));
        h.push(msg("user", "summarize that"));
        let out = trim_to_budget(20, h, 300);
        let condensed = out
            .messages
            .iter()
            .find(|m| m.content.starts_with(CONDENSED_MARKER))
            .expect("expected a condensed step");
        assert!(
            condensed.content.contains("condensed away"),
            "truncation must be stated, not silent: {condensed:?}"
        );
    }

    #[test]
    fn summary_span_never_takes_the_first_or_last_message() {
        let h = history(40);
        let span = pick_summary_span(&h, 50, 600);
        assert!(span > 0);
        assert!(
            span <= h.len() - 2,
            "span {span} would swallow the turn being answered"
        );
    }

    #[test]
    fn summary_span_is_zero_when_everything_already_fits() {
        let h = history(4);
        assert_eq!(pick_summary_span(&h, 50, 100_000), 0);
    }

    /// The summarizer is an LLM call, so the paths worth pinning down in a
    /// unit test are the ones that decide *whether* to make it. Everything
    /// here must resolve without one.
    #[tokio::test]
    async fn no_summarizer_configured_falls_back_to_mechanical_trimming() {
        let out = fit_to_budget(50, history(40), 600, None, None).await;
        assert!(out.dropped > 0);
        assert!(out.summary.is_none());
        assert!(out.rewritten_history.is_none());
    }

    #[tokio::test]
    async fn summarizing_is_skipped_when_condensing_alone_was_enough() {
        // A pointless endpoint: reaching it would be the bug this asserts
        // against, since nothing was going to be dropped here.
        let s = Summarizer {
            endpoint: "http://127.0.0.1:1/never",
            model: "none",
            api_key: "",
        };
        let out = fit_to_budget(20, chain(6), 400, Some(s), None).await;
        assert!(out.condensed > 0);
        assert_eq!(out.dropped, 0);
        assert!(out.summary.is_none());
    }

    #[tokio::test]
    async fn archived_task_prefix_is_dropped_outright_not_condensed() {
        // 20 messages standing in for a previous, already-archived task,
        // plus one more for the current task's own opening message.
        let mut h = history(20);
        let boundary = h.len();
        h.push(msg("user", "please check something in the new task"));

        let out = fit_to_budget(50, h, 300, None, Some(boundary)).await;

        assert!(
            out.messages
                .iter()
                .any(|m| m.content == ARCHIVED_TASK_MARKER),
            "expected the archived-task marker, not a generic trim/condense: {:?}",
            out.messages.iter().map(|m| &m.content).collect::<Vec<_>>()
        );
        assert_eq!(
            out.condensed, 0,
            "the archived prefix should be dropped outright, never condensed"
        );
        assert!(
            out.dropped > 0,
            "the archived prefix's messages must still count toward `dropped`"
        );
        assert!(
            out.messages
                .iter()
                .any(|m| m.content.contains("please check something in the new task")),
            "the current task's own message must survive: {:?}",
            out.messages.iter().map(|m| &m.content).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn archived_before_is_a_no_op_when_everything_already_fits() {
        let h = history(4);
        let out = fit_to_budget(20, h, 100_000, None, Some(2)).await;
        assert_eq!(out.dropped, 0);
        assert!(
            !out.messages
                .iter()
                .any(|m| m.content == ARCHIVED_TASK_MARKER),
            "nothing needed dropping, so the archived-prefix pass shouldn't have run at all"
        );
    }

    #[tokio::test]
    async fn a_failed_summarizer_falls_back_to_dropping() {
        let s = Summarizer {
            endpoint: "http://127.0.0.1:1/refused",
            model: "none",
            api_key: "",
        };
        let out = fit_to_budget(50, history(40), 600, Some(s), None).await;
        assert!(
            out.summary.is_none(),
            "a failed call must not fabricate one"
        );
        assert!(out.dropped > 0, "the mechanical result must still apply");
        assert!(out.messages.iter().any(|m| m.content == TRIM_MARKER));
    }

    #[test]
    fn an_untagged_fence_is_not_treated_as_a_step() {
        let mut h = vec![msg("user", "show me the file")];
        // A plain ``` fence is the model showing text, not a command -- so
        // this pair isn't a finished step and must be left alone.
        h.push(msg("assistant", "here it is:\n```\nfile contents\n```"));
        h.push(msg("user", &"z".repeat(4000)));
        h.push(msg("user", "thanks"));
        let out = trim_to_budget(20, h, 200);
        assert_eq!(out.condensed, 0);
    }
}
