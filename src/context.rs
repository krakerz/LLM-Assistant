//! Keeps what gets sent to the model inside a token budget.
//!
//! Every turn sends the system block plus the whole conversation so far, and
//! nothing trimmed it before this -- a long enough session just eventually
//! blew past the model's context limit, showing up as an endpoint error or
//! (worse) the model quietly losing the earliest turns with no indication.
//! This trims from the oldest end instead, and says so out loud rather than
//! dropping things silently.
//!
//! Two mechanisms, tried in that order because the first loses less: an
//! auto-continue chain's finished steps are *condensed* to the command that
//! ran and what it printed (the model's running commentary is what bulks
//! those up, and it's the part with no facts in it), and only if that still
//! doesn't fit are whole messages dropped from the oldest end.
//!
//! Deliberately *only* mechanical, no summarization: the local models this
//! app talks to have already been caught fabricating a whole completed
//! directory reorganization that never happened, and a bad summary is worse
//! than a dropped turn -- it becomes the authoritative record with no
//! transcript left to check it against. Condensing keeps the command text
//! and the real output verbatim, so nothing here ever invents a fact.

use crate::llm::ChatMessage;
use crate::rules;

/// Left in place of what was dropped, so the model sees an explicit gap
/// rather than an unexplained jump in the conversation.
const TRIM_MARKER: &str = "[...older turns of this conversation were dropped to stay within the \
context window. Don't assume anything about what was removed -- if an earlier detail matters, ask \
the user or re-check it with a command.]";

/// Prefix on every command result fed back to the model. Written in exactly
/// this shape by `formatCommandFeedback` in `ui/main.js` and by
/// `headless.rs`; condensing keys off it to recognize a step that already
/// ran, so all three have to agree -- change one, change all three.
pub const COMMAND_OUTPUT_PREFIX: &str = "[command output, exit ";

/// Stands in front of a condensed step so the model can see that what it's
/// reading is an abridged record rather than the full exchange -- same
/// honesty rule as `TRIM_MARKER`, kept short because there's one per step.
const CONDENSED_MARKER: &str = "[earlier step, condensed to its command and result]";

/// How much of a condensed step's command and output survive. The point is
/// to shed the model's prose, not the facts, so short output is kept whole
/// -- but a huge `find` dump is usually the very thing that blew the budget,
/// and cutting it with an explicit note beats dropping the step entirely.
const CONDENSED_COMMAND_CHARS: usize = 300;
const CONDENSED_OUTPUT_CHARS: usize = 400;

fn truncate_with_note(text: &str, max: usize, what: &str) -> String {
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

/// Rough token estimate. Deliberately not a real tokenizer: every endpoint
/// this app talks to uses a different one, and for a safety margin being
/// *cheap* matters far more than being exact. ~4 chars/token is the usual
/// English approximation; symbol-heavy shell output tokenizes worse than
/// prose, so this runs a little optimistic, which is why the default budget
/// leaves headroom.
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

pub struct TrimOutcome {
    pub messages: Vec<ChatMessage>,
    /// How many messages were dropped (0 when everything fit).
    pub dropped: usize,
    /// How many finished chain steps were condensed to command + output
    /// before any dropping was needed (0 when everything fit).
    pub condensed: usize,
    /// Estimated tokens actually being sent, system block included.
    pub estimated_tokens: usize,
}

/// A finished step of an auto-continue chain: an assistant message that
/// proposed a command, immediately followed by the result of running it.
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

/// Oldest step that may still be condensed. The *last* pair is deliberately
/// off limits: its output is the thing the model is being asked to react to
/// right now, and abridging that is the one place this would actually change
/// the answer.
fn next_condensable(messages: &[ChatMessage]) -> Option<usize> {
    (0..messages.len().saturating_sub(2)).find(|&i| is_finished_step(messages, i))
}

/// Folds one finished step into a single message: the command that ran plus
/// the result line and output it produced, with the assistant's narration in
/// between thrown away. That narration is the bulk of a long chain and
/// contains nothing the following messages don't already establish -- the UI
/// draws exactly this line too, collapsing intermediate steps into
/// "Thinking…" while final answers stay inline.
///
/// The result message's own first line is reused verbatim rather than
/// re-derived, so a multi-step sequence that failed partway through keeps its
/// real per-step exit codes instead of being flattened into one (possibly
/// wrong) status.
fn condense_step(proposal: &ChatMessage, result: &ChatMessage) -> ChatMessage {
    let cmd = rules::extract_command(&proposal.content)
        .unwrap_or_else(|| "(command could not be re-read)".to_string());
    let (header, body) = result
        .content
        .split_once('\n')
        .unwrap_or((result.content.as_str(), ""));

    let mut content = format!(
        "{CONDENSED_MARKER} $ {}\n{header}",
        truncate_with_note(&cmd, CONDENSED_COMMAND_CHARS, "command")
    );
    let body = truncate_with_note(body.trim_end(), CONDENSED_OUTPUT_CHARS, "output");
    if !body.is_empty() {
        content.push('\n');
        content.push_str(&body);
    }
    ChatMessage {
        role: "user".into(),
        content,
    }
}

/// Fits the conversation into `budget` (0 disables this entirely), doing the
/// least destructive thing that works.
///
/// First pass condenses finished chain steps from the oldest end, which
/// keeps every command and every byte of (short) output and only sheds the
/// model's commentary about them. Only if that isn't enough does the second
/// pass start dropping messages outright, and there two are privileged: the
/// very first one -- normally the user's original request, and losing *that*
/// is worse than losing any single later turn -- and the most recent one,
/// which is the thing we're actually answering and so is kept even if it
/// alone busts the budget, since dropping it accomplishes nothing.
pub fn trim_to_budget(
    system_tokens: usize,
    history: Vec<ChatMessage>,
    budget: usize,
) -> TrimOutcome {
    let mut history = history;
    let history_tokens: usize = history.iter().map(|m| estimate_tokens(&m.content)).sum();
    let mut total = system_tokens + history_tokens;

    // Condense before dropping: a step reduced to `$ cmd` + its output still
    // answers "what has already been done here", which a dropped one can't.
    let mut condensed = 0;
    if budget != 0 {
        while total > budget {
            let Some(i) = next_condensable(&history) else {
                break;
            };
            let before =
                estimate_tokens(&history[i].content) + estimate_tokens(&history[i + 1].content);
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
    let mut used = system_tokens + estimate_tokens(&first.content) + estimate_tokens(TRIM_MARKER);

    let mut kept_tail: Vec<ChatMessage> = Vec::new();
    for (i, msg) in history.iter().enumerate().skip(1).rev() {
        let cost = estimate_tokens(&msg.content);
        // `i == history.len() - 1` is the turn being answered right now:
        // always keep it, budget or not.
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
        messages.push(ChatMessage {
            role: "user".into(),
            content: TRIM_MARKER.into(),
        });
    }
    messages.extend(kept_tail);

    TrimOutcome {
        messages,
        dropped,
        condensed,
        estimated_tokens: used,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: content.into(),
        }
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
