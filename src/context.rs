//! Fits a request into a token budget by, in order: condensing finished tool
//! steps, optionally summarizing, then dropping oldest first. Every loss is
//! reported, never silent.
//!
//! Summarizing sits second because it's the risky rung: a model-written
//! summary becomes the record with no transcript left to check it against,
//! whereas a dropped turn leaves a marker saying to go look. Hence opt-in,
//! temperature 0, and shown to the user.
//!
//! The message that opened the current task (`task_start`) and the last
//! message are pinned. History is in the agent's canonical form: an assistant
//! message with one tool call is always followed by its `tool` result, and
//! those two are never separated here, or the native wire format breaks.

use crate::llm::{ChatMessage, ToolCall};
use crate::{tool_protocol, tools};

const TRIM_MARKER: &str = "[...older turns of this conversation were dropped to stay within the \
context window. Don't assume anything about what was removed -- if an earlier detail matters, ask \
the user or re-check it with a tool.]";

/// Distinct from `TRIM_MARKER`: these turns aren't a budget-forced loss, their
/// facts live in the app-written session record.
const ARCHIVED_TASK_MARKER: &str =
    "[...turns from earlier, already-completed tasks were dropped here -- what they did is in the \
session record, this was only their chat transcript.]";

const STEP_RESULT_CHARS: usize = 600;

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

/// Cheap chars/4 estimate, not a real tokenizer; budgets need headroom.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// A base64 image's real token cost is a small roughly-fixed amount, not
/// proportional to its encoded length, so it gets a flat placeholder instead
/// of chars/4 (which would make one picture look like 100k tokens).
const ESTIMATED_TOKENS_PER_IMAGE: usize = 1000;

fn estimate_message_tokens(msg: &ChatMessage) -> usize {
    let calls: usize = msg
        .tool_calls
        .iter()
        .map(|c| estimate_tokens(&c.name) + estimate_tokens(&c.arguments))
        .sum();
    estimate_tokens(&msg.content) + calls + msg.images.len() * ESTIMATED_TOKENS_PER_IMAGE
}

pub struct TrimOutcome {
    pub messages: Vec<ChatMessage>,
    pub dropped: usize,
    pub condensed: usize,
    /// Including the system block.
    pub estimated_tokens: usize,
}

// --- Condensing ---

/// An assistant message with exactly one tool call and its matching result.
fn is_finished_step(messages: &[ChatMessage], i: usize) -> bool {
    let (Some(call), Some(result)) = (messages.get(i), messages.get(i + 1)) else {
        return false;
    };
    call.role == "assistant"
        && call.tool_calls.len() == 1
        && result.role == "tool"
        && result.tool_call_id.as_deref() == Some(call.tool_calls[0].id.as_str())
}

/// Oldest condensable step. The last message is off limits (its result is what
/// the turn is answering), so a pair ending there is skipped.
fn next_condensable(messages: &[ChatMessage]) -> Option<usize> {
    (0..messages.len().saturating_sub(2)).find(|&i| is_finished_step(messages, i))
}

fn call_title(call: &ToolCall) -> String {
    tool_protocol::from_native(call)
        .map(|t: tools::Tool| t.title())
        .unwrap_or_else(|_| call.name.clone())
}

/// `[tool result: name — ok]\nbody` -> (ok, body). Anything else counts as ok.
fn split_result(content: &str) -> (bool, &str) {
    match content.split_once('\n') {
        Some((head, body)) if head.starts_with("[tool result:") => {
            (!head.contains("— error"), body)
        }
        _ => (true, content),
    }
}

/// One assistant message with no tool calls, so the wire format stays valid.
fn condense_step(call: &ChatMessage, result: &ChatMessage) -> ChatMessage {
    let (ok, body) = split_result(&result.content);
    let body = truncate_with_note(body.trim(), STEP_RESULT_CHARS, "output");
    ChatMessage::text(
        "assistant",
        format!(
            "[earlier step] {} → {}: {body}",
            call_title(&call.tool_calls[0]),
            if ok { "ok" } else { "error" }
        ),
    )
}

// --- Trimming ---

/// A message and, when it is a tool result, the assistant call before it.
fn unit_start(history: &[ChatMessage], i: usize) -> usize {
    if history[i].role == "tool" && i > 0 && history[i - 1].role == "assistant" {
        i - 1
    } else {
        i
    }
}

/// Condense, then drop, until it fits `budget` (0 disables). `history[task_start]`
/// and the last message are always kept, even if the last alone busts the budget.
fn trim_core(
    system_tokens: usize,
    history: Vec<ChatMessage>,
    budget: usize,
    task_start: usize,
) -> TrimOutcome {
    let mut history = history;
    let mut task_start = task_start.min(history.len().saturating_sub(1));
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
            total = total + estimate_message_tokens(&merged) - before;
            history.splice(i..=i + 1, [merged]);
            if i < task_start {
                task_start -= 1;
            }
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

    let pinned = history[task_start].clone();
    let mut used = system_tokens + estimate_message_tokens(&pinned) + estimate_tokens(TRIM_MARKER);

    // Current task first (newest to oldest, by whole units), then whatever
    // earlier tasks still fit.
    let take_units = |range_start: usize, range_end: usize, used: &mut usize| -> usize {
        let mut keep_from = range_end;
        let mut end = range_end;
        while end > range_start {
            let start = unit_start(&history, end - 1).max(range_start);
            let cost: usize = history[start..end]
                .iter()
                .map(estimate_message_tokens)
                .sum();
            // The last message is the turn being answered: keep it regardless.
            if *used + cost > budget && end != history.len() {
                break;
            }
            *used += cost;
            keep_from = start;
            end = start;
        }
        keep_from
    };
    let tail_from = take_units(task_start + 1, history.len(), &mut used);
    let prefix_from = if tail_from == task_start + 1 && task_start > 0 {
        take_units(0, task_start, &mut used)
    } else {
        task_start
    };

    let mut messages = Vec::new();
    let prefix_dropped = prefix_from;
    if prefix_dropped > 0 {
        messages.push(ChatMessage::text("user", TRIM_MARKER));
    }
    messages.extend_from_slice(&history[prefix_from..task_start]);
    messages.push(pinned);
    let tail_dropped = tail_from - (task_start + 1);
    if tail_dropped > 0 {
        messages.push(ChatMessage::text("user", TRIM_MARKER));
    }
    messages.extend_from_slice(&history[tail_from..]);

    TrimOutcome {
        messages,
        dropped: prefix_dropped + tail_dropped,
        condensed,
        estimated_tokens: used,
    }
}

/// For callers whose history has no task boundary (chat): the first message
/// is pinned.
pub fn trim_to_budget(
    system_tokens: usize,
    history: Vec<ChatMessage>,
    budget: usize,
) -> TrimOutcome {
    trim_core(system_tokens, history, budget, 0)
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

/// Written against observed fabrication: keep pointing at tool results as the
/// only evidence, and ask for omission over guessing.
const SUMMARY_PROMPT: &str = "You are compressing the oldest part of a transcript between a user \
and a file assistant, so it can be kept in a limited context window. Write the summary that will \
replace it.\n\n\
- Report only what the text actually shows. Never say a file was created, moved, renamed, or \
deleted unless a tool result in the text shows that it happened.\n\
- Tool results are the record. Where the assistant's prose disagrees with the result it got, go \
with the result.\n\
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
         detail matters, re-check it with a tool.]"
    )
}

/// How many messages after `history[task_start]` (never the last) to summarize.
/// Measured against the un-condensed history so the span maps onto the stored
/// record; that over-takes slightly, which is the safe direction. Extended so
/// it never ends between a tool call and its result.
fn pick_summary_span(
    history: &[ChatMessage],
    system_tokens: usize,
    budget: usize,
    task_start: usize,
) -> usize {
    let total: usize = system_tokens + history.iter().map(estimate_message_tokens).sum::<usize>();
    let reserve = SUMMARY_MAX_CHARS.div_ceil(4) + estimate_tokens(&summary_marker(0));
    let mut need = (total + reserve).saturating_sub(budget);
    let room = history.len().saturating_sub(task_start + 2);

    let mut taken = 0;
    for msg in history.iter().skip(task_start + 1).take(room) {
        if need == 0 {
            break;
        }
        need = need.saturating_sub(estimate_message_tokens(msg));
        taken += 1;
    }
    while taken > 0
        && history
            .get(task_start + 1 + taken)
            .is_some_and(|m| m.role == "tool")
    {
        taken += 1;
    }
    if task_start + 1 + taken >= history.len() {
        return 0;
    }
    taken
}

fn describe(m: &ChatMessage) -> String {
    let mut out = format!("{}: {}", m.role, m.content);
    for c in &m.tool_calls {
        out.push_str(&format!("\n[tool call] {}", call_title(c)));
    }
    out
}

async fn summarize(s: &Summarizer<'_>, victims: &[ChatMessage]) -> anyhow::Result<String> {
    let transcript = victims
        .iter()
        .map(describe)
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
    /// Where the current task's opening message sits in `rewritten_history`.
    pub rewritten_task_start: usize,
    /// For showing in the UI.
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
            rewritten_task_start: 0,
            summary: None,
            summarized: 0,
            condensed: trim.condensed,
            dropped: trim.dropped,
            estimated_tokens: trim.estimated_tokens,
        }
    }
}

/// Everything before `task_start` becomes one marker message. Only valid when
/// that prefix is recorded elsewhere (the session record).
fn drop_archived_prefix(history: Vec<ChatMessage>, task_start: usize) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(history.len() - task_start + 1);
    out.push(ChatMessage::text("user", ARCHIVED_TASK_MARKER));
    out.extend_from_slice(&history[task_start..]);
    out
}

/// Chat-mode entry point; unchanged contract. `archived_before` (agent-style
/// task boundary with an archived prefix) is `None` for chat, which then pins
/// its first message.
pub async fn fit_to_budget(
    system_tokens: usize,
    history: Vec<ChatMessage>,
    budget: usize,
    summarizer: Option<Summarizer<'_>>,
    archived_before: Option<usize>,
) -> FitOutcome {
    fit_task_to_budget(
        system_tokens,
        history,
        budget,
        summarizer,
        archived_before.unwrap_or(0),
        archived_before.is_some(),
    )
    .await
}

/// Agent entry point. `task_start` indexes the message that opened the
/// current task. `prefix_archived` says everything before it is already in the
/// session record, so it is dropped outright as the first pass once over
/// budget (rather than condensed/summarized like turns with no backstop).
pub async fn fit_task_to_budget(
    system_tokens: usize,
    history: Vec<ChatMessage>,
    budget: usize,
    summarizer: Option<Summarizer<'_>>,
    task_start: usize,
    prefix_archived: bool,
) -> FitOutcome {
    let mut task_start = task_start.min(history.len().saturating_sub(1));
    let mut archived_dropped = 0;
    let mut history = history;
    if prefix_archived && budget != 0 && task_start >= 1 {
        let total: usize =
            system_tokens + history.iter().map(estimate_message_tokens).sum::<usize>();
        if total > budget {
            archived_dropped = task_start;
            history = drop_archived_prefix(history, task_start);
            task_start = 1;
        }
    }
    let mut outcome = fit_inner(system_tokens, history, budget, summarizer, task_start).await;
    outcome.dropped += archived_dropped;
    outcome
}

async fn fit_inner(
    system_tokens: usize,
    history: Vec<ChatMessage>,
    budget: usize,
    summarizer: Option<Summarizer<'_>>,
    task_start: usize,
) -> FitOutcome {
    let mechanical = trim_core(system_tokens, history.clone(), budget, task_start);
    let Some(s) = summarizer else {
        return FitOutcome::mechanical(mechanical);
    };
    if budget == 0 || mechanical.dropped == 0 {
        return FitOutcome::mechanical(mechanical);
    }

    let span = pick_summary_span(&history, system_tokens, budget, task_start);
    if span < MIN_MESSAGES_TO_SUMMARIZE {
        return FitOutcome::mechanical(mechanical);
    }

    let victims = task_start + 1..=task_start + span;
    let summary = match summarize(&s, &history[victims.clone()]).await {
        Ok(text) => text,
        Err(e) => {
            log::warn!("summarizing older turns failed, falling back to dropping them: {e}");
            return FitOutcome::mechanical(mechanical);
        }
    };

    let mut rewritten = history;
    rewritten.splice(
        victims,
        // A user message, so nothing in the summary can read as a tool call.
        [ChatMessage::text(
            "user",
            format!("{}\n{summary}", summary_marker(span)),
        )],
    );

    let trim = trim_core(system_tokens, rewritten.clone(), budget, task_start);
    FitOutcome {
        messages: trim.messages,
        rewritten_history: Some(rewritten),
        rewritten_task_start: task_start,
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

    // Flat per-image estimate: chars/4 of a base64 blob would get a short
    // conversation dropped to "fit" around one attached picture.
    #[test]
    fn an_attached_image_does_not_blow_the_token_estimate() {
        let mut with_image = msg("user", "what's in this picture?");
        with_image.images = vec![format!("data:image/png;base64,{}", "A".repeat(500_000))];

        let h = vec![msg("user", "hello"), msg("assistant", "hi"), with_image];
        let out = trim_to_budget(20, h, 5000);
        assert_eq!(out.dropped, 0);
        assert!(out.estimated_tokens < 50_000, "{}", out.estimated_tokens);
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
        let out = trim_to_budget(50, history(40), 600);
        assert!(out.dropped > 0);
        assert!(out.estimated_tokens <= 600, "{}", out.estimated_tokens);
        assert_eq!(
            out.messages
                .iter()
                .filter(|m| m.content == TRIM_MARKER)
                .count(),
            1
        );
    }

    #[test]
    fn keeps_the_original_request_and_the_turn_being_answered() {
        let mut h = history(40);
        h[0] = msg("user", "ORIGINAL REQUEST");
        let last = h.len() - 1;
        h[last] = msg("user", "CURRENT TURN");

        let out = trim_to_budget(50, h, 600);
        assert_eq!(out.messages.first().unwrap().content, "ORIGINAL REQUEST");
        assert_eq!(out.messages.last().unwrap().content, "CURRENT TURN");
    }

    #[test]
    fn oversized_final_turn_is_kept_even_though_it_busts_the_budget() {
        let mut h = history(6);
        let last = h.len() - 1;
        h[last] = msg("user", &"y".repeat(40_000));
        let out = trim_to_budget(10, h, 500);
        assert_eq!(out.messages.last().unwrap().content.len(), 40_000);
    }

    fn call(id: &str, path: &str) -> ChatMessage {
        let mut m = msg("assistant", &"narration ".repeat(40));
        m.tool_calls = vec![ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: format!("{{\"path\":\"{path}\"}}"),
        }];
        m
    }

    fn result(id: &str, ok: bool, body: &str) -> ChatMessage {
        let mut m = msg(
            "tool",
            &format!(
                "[tool result: read_file — {}]\n{body}",
                if ok { "ok" } else { "error" }
            ),
        );
        m.tool_call_id = Some(id.into());
        m
    }

    /// `n` finished steps after the opening user message.
    fn chain(n: usize) -> Vec<ChatMessage> {
        let mut h = vec![msg("user", "organize this folder")];
        for i in 0..n {
            h.push(call(&format!("c{i}"), &format!("f{i}.txt")));
            h.push(result(&format!("c{i}"), true, "a\nb"));
        }
        h
    }

    /// Native wire validity: every `tool` message directly follows its call.
    fn assert_wire_valid(h: &[ChatMessage]) {
        for (i, m) in h.iter().enumerate() {
            if m.role == "tool" {
                let prev = &h[i - 1];
                assert!(
                    prev.role == "assistant"
                        && prev.tool_calls.first().map(|c| c.id.as_str())
                            == m.tool_call_id.as_deref(),
                    "orphan tool result at {i}"
                );
            }
            if !m.tool_calls.is_empty() {
                assert_eq!(h[i + 1].role, "tool", "call at {i} lost its result");
            }
        }
    }

    #[test]
    fn condensing_is_preferred_over_dropping() {
        let out = trim_to_budget(20, chain(6), 450);
        assert!(out.condensed > 0);
        assert_eq!(out.dropped, 0);
        assert!(out.estimated_tokens <= 450, "{}", out.estimated_tokens);
        assert_wire_valid(&out.messages);
    }

    #[test]
    fn a_condensed_step_is_one_plain_assistant_message() {
        let mut h = chain(6);
        h[2] = result("c0", false, "no such file");
        let out = trim_to_budget(20, h, 450);
        let c = out
            .messages
            .iter()
            .find(|m| m.content.starts_with("[earlier step]"))
            .expect("expected a condensed step");
        assert_eq!(c.role, "assistant");
        assert!(c.tool_calls.is_empty());
        assert!(c.content.contains("Read f0.txt"), "{c:?}");
        assert!(c.content.contains("error: no such file"), "{c:?}");
        assert!(!c.content.contains("narration"), "{c:?}");
    }

    #[test]
    fn the_step_being_answered_is_never_condensed() {
        let out = trim_to_budget(20, chain(8), 200);
        let last = out.messages.last().unwrap();
        assert_eq!(last.role, "tool", "{last:?}");
        assert_wire_valid(&out.messages);
    }

    #[test]
    fn long_output_is_cut_with_an_explicit_note() {
        let mut h = chain(2);
        h[2] = result("c0", true, &"./some/path\n".repeat(500));
        let out = trim_to_budget(20, h, 400);
        let c = out
            .messages
            .iter()
            .find(|m| m.content.starts_with("[earlier step]"))
            .expect("expected a condensed step");
        assert!(c.content.contains("condensed away"), "{c:?}");
    }

    #[test]
    fn dropping_keeps_call_and_result_together() {
        let mut h = chain(4);
        h.splice(1..1, history(30));
        for budget in [300, 400, 600, 900] {
            let out = trim_to_budget(20, h.clone(), budget);
            assert!(out.dropped > 0, "budget {budget}");
            assert_wire_valid(&out.messages);
        }
    }

    // Audit H1: a mid-chain send over budget used to pin the *session's* first
    // message and could lose the current task's own request.
    #[test]
    fn a_mid_chain_send_over_budget_keeps_the_current_tasks_request() {
        let mut h = history(10);
        let task_start = h.len();
        h.push(msg("user", "CURRENT TASK REQUEST"));
        for i in 0..10 {
            h.push(call(&format!("c{i}"), "big.txt"));
            h.push(result(&format!("c{i}"), true, &"line\n".repeat(300)));
        }
        let out = trim_core(50, h, 900, task_start);
        assert!(
            out.messages
                .iter()
                .any(|m| m.content == "CURRENT TASK REQUEST"),
            "the request being worked on must survive"
        );
        assert!(out.dropped > 0);
        assert_eq!(out.messages.last().unwrap().role, "tool");
        assert_wire_valid(&out.messages);
    }

    #[test]
    fn earlier_tasks_go_before_the_current_one() {
        let mut h = history(10);
        let task_start = h.len();
        h.push(msg("user", "CURRENT"));
        h.push(msg("assistant", &"z".repeat(400)));
        h.push(msg("user", "NEXT"));
        let out = trim_core(50, h, 330, task_start);
        let texts: Vec<&str> = out.messages.iter().map(|m| m.content.as_str()).collect();
        assert!(texts.contains(&"CURRENT"), "{texts:?}");
        assert!(texts.contains(&"NEXT"), "{texts:?}");
        assert!(out.dropped >= 8, "{}", out.dropped);
    }

    #[test]
    fn summary_span_never_takes_the_pinned_or_last_message() {
        let h = history(40);
        let span = pick_summary_span(&h, 50, 600, 0);
        assert!(span > 0);
        assert!(
            span <= h.len() - 2,
            "span {span} would swallow the last turn"
        );
    }

    #[test]
    fn summary_span_does_not_split_a_step() {
        let h = chain(12);
        for budget in [300, 500, 800] {
            let span = pick_summary_span(&h, 20, budget, 0);
            if span > 0 {
                assert_ne!(h[1 + span].role, "tool", "budget {budget} span {span}");
            }
        }
    }

    #[test]
    fn summary_span_is_zero_when_everything_already_fits() {
        assert_eq!(pick_summary_span(&history(4), 50, 100_000, 0), 0);
    }

    // The summarizer is an LLM call, so the paths worth pinning down are the
    // ones that decide whether to make it. Everything here must resolve
    // without one.
    #[tokio::test]
    async fn no_summarizer_configured_falls_back_to_mechanical_trimming() {
        let out = fit_to_budget(50, history(40), 600, None, None).await;
        assert!(out.dropped > 0);
        assert!(out.summary.is_none());
        assert!(out.rewritten_history.is_none());
    }

    #[tokio::test]
    async fn summarizing_is_skipped_when_condensing_alone_was_enough() {
        let s = Summarizer {
            endpoint: "http://127.0.0.1:1/never",
            model: "none",
            api_key: "",
        };
        let out = fit_to_budget(20, chain(6), 450, Some(s), None).await;
        assert!(out.condensed > 0);
        assert_eq!(out.dropped, 0);
        assert!(out.summary.is_none());
    }

    #[tokio::test]
    async fn archived_task_prefix_is_dropped_outright_not_condensed() {
        let mut h = history(20);
        let boundary = h.len();
        h.push(msg("user", "please check something in the new task"));

        let out = fit_to_budget(50, h, 300, None, Some(boundary)).await;
        assert!(out
            .messages
            .iter()
            .any(|m| m.content == ARCHIVED_TASK_MARKER));
        assert_eq!(out.condensed, 0);
        assert_eq!(out.dropped, 20);
        assert!(out
            .messages
            .iter()
            .any(|m| m.content.contains("please check something in the new task")));
    }

    #[tokio::test]
    async fn archived_before_is_a_no_op_when_everything_already_fits() {
        let out = fit_to_budget(20, history(4), 100_000, None, Some(2)).await;
        assert_eq!(out.dropped, 0);
        assert!(!out
            .messages
            .iter()
            .any(|m| m.content == ARCHIVED_TASK_MARKER));
    }

    #[tokio::test]
    async fn a_failed_summarizer_falls_back_to_dropping() {
        let s = Summarizer {
            endpoint: "http://127.0.0.1:1/refused",
            model: "none",
            api_key: "",
        };
        let out = fit_to_budget(50, history(40), 600, Some(s), None).await;
        assert!(out.summary.is_none());
        assert!(out.dropped > 0);
        assert!(out.messages.iter().any(|m| m.content == TRIM_MARKER));
    }
}
