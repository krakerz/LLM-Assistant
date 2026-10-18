//! The file-ops agent loop, shared by the GUI (`fileops_commands.rs`) and the
//! terminal (`headless.rs`). One `AgentSession` per open folder; one
//! `run_task` per user message.
//!
//! History is kept in one canonical form: an assistant message holds the
//! model's raw reply plus exactly one tool call, and each result is a `tool`
//! message. Native mode sends that as-is; text mode derives the request copy
//! (see `build_request`). The app, not the model, decides what needs
//! confirmation, what counts as repeating, and what the end-of-task change
//! list says.

use crate::config::{self, AppConfig, GrantedPath};
use crate::llm::{self, ChatMessage, ChatOptions, ChatReply, ToolCall};
use crate::sandbox::{self, CancelToken, Classification};
use crate::tool_protocol::{self as proto, TextParse, Transport};
use crate::tools::{self, TaskState, Tool, Workspace};
use crate::{context, memory, rules};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

pub type Cancel = CancelToken;

/// Display cap for a tool result; the model gets `proto::format_tool_result`'s own cap.
const DISPLAY_OUTPUT_CAP: usize = 20_000;
/// Same exact call this many times in one task is a loop.
const STUCK_CALL_THRESHOLD: usize = 4;
const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 120;
/// How much of a cut-off reply is kept in history.
const CUT_OFF_KEEP_CHARS: usize = 1500;

const EXTRA_CALLS_NOTE: &str =
    "[only the first tool call in your reply was run; the others were ignored. Make one call per reply.]";
const EMPTY_REPLY_ERROR: &str = "the reply was empty";
const AFTER_ERROR_NUDGE: &str = "[Your last tool call failed and the task isn't done. If you \
meant to do something next, make that tool call now. If the task is finished or can't be done, \
say so in your final answer.]";

// --- Events and host interface ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DoneReason {
    Finished,
    Stopped,
    StepCap,
    Denied,
    Guard,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedFile {
    pub path: String,
    /// `created`, `modified`, `moved`, `deleted` or `folder created`.
    pub change: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AgentEvent {
    Thinking {
        text: String,
    },
    AssistantText {
        text: String,
        #[serde(rename = "final")]
        is_final: bool,
    },
    ToolCall {
        id: String,
        tool: String,
        title: String,
    },
    ToolResult {
        id: String,
        ok: bool,
        output: String,
    },
    Confirm {
        id: String,
        title: String,
        command: Option<String>,
        diff: Option<String>,
        can_always_allow: bool,
    },
    Notice {
        text: String,
    },
    ManualCommand {
        command: String,
    },
    ChangedFiles {
        files: Vec<ChangedFile>,
    },
    Done {
        reason: DoneReason,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct ConfirmRequest {
    pub id: String,
    pub title: String,
    pub command: Option<String>,
    pub diff: Option<String>,
    pub can_always_allow: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    AlwaysAllow,
    Deny,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait AgentHost: Send + Sync {
    fn emit(&self, ev: AgentEvent);
    /// Must resolve to `Deny` if the task is cancelled while waiting.
    fn confirm<'a>(&'a self, req: ConfirmRequest) -> BoxFuture<'a, Decision>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskOutcome {
    pub reason: DoneReason,
}

// --- Session ---

pub struct AgentSession {
    root: PathBuf,
    workspace: Arc<Workspace>,
    history: Vec<ChatMessage>,
    /// Index of the user message that opened the current task.
    task_start: usize,
    transport: Transport,
    /// Native was rejected by the model; stays text for the session.
    fell_back: bool,
    /// Approvals per fade key (`cmd:<program>` / `tool:<name>`) since the last deny.
    approvals: HashMap<String, u32>,
    faded: HashSet<String>,
}

fn resolve_transport(cfg: &AppConfig, fell_back: bool) -> Transport {
    match cfg.tool_calling.trim() {
        "text" => Transport::Text,
        "native" => Transport::Native,
        _ if fell_back => Transport::Text,
        _ => Transport::Native,
    }
}

/// One shim dir for the GUI and headless.
pub fn shim_dir() -> PathBuf {
    crate::paths::app_config_dir().join("shims")
}

impl AgentSession {
    pub fn new(root: &Path, cfg: &AppConfig) -> anyhow::Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            workspace: Arc::new(Workspace::new(root, cfg)?),
            history: Vec::new(),
            task_start: 0,
            transport: resolve_transport(cfg, false),
            fell_back: false,
            approvals: HashMap::new(),
            faded: HashSet::new(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    #[cfg(test)]
    fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    fn fade_threshold_note(&mut self, key: &str, threshold: u32, host: &dyn AgentHost) {
        let n = self.approvals.entry(key.to_string()).or_insert(0);
        *n += 1;
        if threshold > 0 && *n >= threshold && self.faded.insert(key.to_string()) {
            let what = match key.split_once(':') {
                Some(("cmd", prog)) => format!("`{prog}` commands"),
                Some((_, tool)) => format!("{tool} calls"),
                None => key.to_string(),
            };
            host.emit(AgentEvent::Notice {
                text: format!(
                    "approved {n} times in a row: {what} will now run without asking for the rest of this session"
                ),
            });
        }
    }
}

// --- Public entry point ---

/// Runs one task to completion. Rules text is loaded here (hot-reloaded per
/// task); see `run_task_with` for the loop.
pub async fn run_task(
    cfg: &AppConfig,
    session: &mut AgentSession,
    user_message: String,
    host: &dyn AgentHost,
    cancel: &Cancel,
) -> TaskOutcome {
    let rules_text = load_rules_text(cfg);
    run_task_with(cfg, session, &rules_text, user_message, host, cancel).await
}

fn load_rules_text(cfg: &AppConfig) -> String {
    if cfg.disable_builtin_rules {
        return String::new();
    }
    let general = rules::load_general_or_init().unwrap_or_else(|e| {
        log::warn!("agent: cannot load general rules: {e}");
        String::new()
    });
    let command = rules::load_command_or_init().unwrap_or_else(|e| {
        log::warn!("agent: cannot load command rules: {e}");
        String::new()
    });
    format!("{general}\n\n{command}").trim().to_string()
}

pub(crate) async fn run_task_with(
    cfg: &AppConfig,
    session: &mut AgentSession,
    rules_text: &str,
    user_message: String,
    host: &dyn AgentHost,
    cancel: &Cancel,
) -> TaskOutcome {
    let cx = Cx {
        cfg,
        host,
        cancel,
        rules: rules_text,
    };
    // Grants and settings may have changed since the last task.
    match Workspace::new(&session.root, cfg) {
        Ok(ws) => session.workspace = Arc::new(ws),
        Err(e) => {
            host.emit(AgentEvent::Error {
                message: e.to_string(),
            });
            host.emit(AgentEvent::Done {
                reason: DoneReason::Error,
            });
            return TaskOutcome {
                reason: DoneReason::Error,
            };
        }
    }
    session.transport = resolve_transport(cfg, session.fell_back);

    memory::start_task(cfg, Some(&session.root), &user_message);
    session.task_start = session.history.len();
    session
        .history
        .push(ChatMessage::text("user", user_message));

    let mut t = TaskLocal {
        state: TaskState::new(),
        ..Default::default()
    };
    let reason = session.drive(&cx, &mut t).await;

    let files: Vec<ChangedFile> = t
        .state
        .changed_files_summary()
        .into_iter()
        .map(|c| ChangedFile {
            path: c.path,
            change: c.kind.label().to_string(),
            from: match c.kind {
                tools::ChangeKind::Moved { from } => Some(from),
                _ => None,
            },
        })
        .collect();
    if !files.is_empty() {
        host.emit(AgentEvent::ChangedFiles { files });
    }
    host.emit(AgentEvent::Done { reason });
    TaskOutcome { reason }
}

// --- The loop ---

struct Cx<'a> {
    cfg: &'a AppConfig,
    host: &'a dyn AgentHost,
    cancel: &'a Cancel,
    rules: &'a str,
}

#[derive(Default)]
struct TaskLocal {
    state: TaskState,
    /// Every call attempted this task, for the repeat guards.
    calls: Vec<Tool>,
    steps: u32,
    malformed_streak: u32,
    cut_off_used: bool,
    error_nudge_used: bool,
    /// Destructive writes already warned about once; a re-send goes ahead.
    warned_writes: Vec<Tool>,
    noticed_condensed: usize,
    noticed_dropped: usize,
}

enum TurnError {
    Cancelled,
    Failed(String),
}

enum Flow {
    Continue,
    End(DoneReason),
}

enum Gate {
    Run,
    /// Doomed before asking; the text goes back to the model.
    Fail(String),
    Denied,
    Stopped,
    Manual(String),
}

enum Parsed {
    Call {
        tool: Tool,
        id: String,
        prose: String,
        extra: bool,
        from_text: bool,
    },
    Answer(String),
    Bad {
        error: String,
    },
}

fn new_call_id() -> String {
    format!("call_{}", llm::new_message_id())
}

fn tool_message(id: &str, content: String) -> ChatMessage {
    let mut m = ChatMessage::text("tool", content);
    m.tool_call_id = Some(id.to_string());
    m
}

/// The call's arguments as a JSON object string (no `tool` tag).
fn args_json(tool: &Tool) -> String {
    let mut v = serde_json::to_value(tool).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.remove("tool");
    }
    v.to_string()
}

fn cap_display(text: &str) -> String {
    let total = text.chars().count();
    if total <= DISPLAY_OUTPUT_CAP {
        return text.to_string();
    }
    let kept: String = text.chars().take(DISPLAY_OUTPUT_CAP).collect();
    format!(
        "{kept}\n[... {} more characters not shown]",
        total - DISPLAY_OUTPUT_CAP
    )
}

fn first_program(command: &str) -> Option<String> {
    shell_words::split(command.trim()).ok()?.into_iter().next()
}

fn classify_reply(reply: &ChatReply) -> Parsed {
    if let Some(first) = reply.tool_calls.first() {
        return match proto::from_native(first) {
            Ok(tool) => Parsed::Call {
                tool,
                id: if first.id.is_empty() {
                    new_call_id()
                } else {
                    first.id.clone()
                },
                prose: reply.content.trim().to_string(),
                extra: reply.tool_calls.len() > 1,
                from_text: false,
            },
            Err(error) => Parsed::Bad { error },
        };
    }
    match proto::parse_text_reply(&reply.content) {
        TextParse::Tool {
            tool,
            prose,
            extra_blocks,
        } => Parsed::Call {
            tool,
            id: new_call_id(),
            prose: prose.trim().to_string(),
            extra: extra_blocks > 0,
            from_text: true,
        },
        TextParse::Answer(text) if text.trim().is_empty() => Parsed::Bad {
            error: EMPTY_REPLY_ERROR.into(),
        },
        TextParse::Answer(text) => Parsed::Answer(text),
        TextParse::Malformed { error, .. } => Parsed::Bad { error },
    }
}

pub fn root_note(root: &Path, grants: &[GrantedPath]) -> String {
    let mut note = format!(
        "The open folder is {}. Tool paths are relative to it, and run_command starts there. \
         If the user says \"root\" or \"the root folder\", they mean the top of this folder, \
         never one of the extra paths below.",
        root.display()
    );
    if !grants.is_empty() {
        note.push_str("\nYou may also use these absolute paths outside the folder:");
        for g in grants {
            let access = if g.read_write {
                "read-write"
            } else {
                "read-only"
            };
            let scope = if g.recursive {
                ""
            } else {
                ", top-level files only"
            };
            let why = if g.note.trim().is_empty() {
                String::new()
            } else {
                format!(" -- {}", g.note.trim())
            };
            note.push_str(&format!("\n- {} ({access}{scope}){why}", g.path));
        }
    }
    note
}

/// The request copy of the history. Native keeps the canonical form; text
/// drops `tool_calls` (the raw reply already holds the block) and turns tool
/// results into user messages. Adjacent same-role plain messages are merged
/// (many local chat templates reject or mangle consecutive same-role turns),
/// and the session record goes on the last message so the system prompt, and
/// with it the server's prefix cache, stays identical across steps.
fn build_request(
    system: String,
    messages: Vec<ChatMessage>,
    tail: Option<&str>,
    transport: Transport,
) -> Vec<ChatMessage> {
    fn plain(m: &ChatMessage) -> bool {
        m.tool_calls.is_empty() && m.tool_call_id.is_none() && m.images.is_empty()
    }
    let mut out = vec![ChatMessage::text("system", system)];
    for mut m in messages {
        if transport == Transport::Text {
            if m.role == "tool" {
                m.role = "user".into();
                m.tool_call_id = None;
            }
            m.tool_calls.clear();
        }
        match out.last_mut() {
            Some(prev)
                if prev.role == m.role && prev.role != "system" && plain(prev) && plain(&m) =>
            {
                prev.content.push_str("\n\n");
                prev.content.push_str(&m.content);
            }
            _ => out.push(m),
        }
    }
    if let (Some(tail), Some(last)) = (tail, out.last_mut()) {
        if last.role != "system" {
            last.content.push_str("\n\n");
            last.content.push_str(tail);
        }
    }
    out
}

impl AgentSession {
    fn push_note(&mut self, text: &str) {
        self.history.push(ChatMessage::text("user", text));
    }

    /// A failed reply stays in history as plain text (a broken native call
    /// can't be echoed back), followed by the correction.
    fn push_failed_reply(&mut self, reply: &ChatReply, note: &str) {
        let text = if reply.content.trim().is_empty() {
            "(no usable reply)".to_string()
        } else {
            context::truncate_with_note(&reply.content, CUT_OFF_KEEP_CHARS, "reply")
        };
        self.history.push(ChatMessage::text("assistant", text));
        self.push_note(note);
    }

    fn push_result(&mut self, tool: &Tool, id: &str, ok: bool, body: &str) {
        self.history
            .push(tool_message(id, proto::format_tool_result(tool, ok, body)));
    }

    async fn drive(&mut self, cx: &Cx<'_>, t: &mut TaskLocal) -> DoneReason {
        let cap = cx.cfg.max_auto_steps;
        loop {
            if cx.cancel.is_cancelled() {
                return self.stopped();
            }
            if cap != 0 && t.steps >= cap {
                self.push_note(proto::STEP_CAP_NOTE);
                cx.host.emit(AgentEvent::Notice {
                    text: format!("paused after {cap} steps -- send another message to continue"),
                });
                return DoneReason::StepCap;
            }
            t.steps += 1;

            let reply = match self.model_turn(cx, t, true).await {
                Ok(r) => r,
                Err(TurnError::Cancelled) => return self.stopped(),
                Err(TurnError::Failed(message)) => {
                    cx.host.emit(AgentEvent::Error { message });
                    return DoneReason::Error;
                }
            };
            if let Some(r) = reply.reasoning.as_deref().filter(|r| !r.trim().is_empty()) {
                cx.host.emit(AgentEvent::Thinking {
                    text: r.to_string(),
                });
            }
            log::debug!(
                "agent reply (finish={:?}, native calls={}): {:?}",
                reply.finish_reason,
                reply.tool_calls.len(),
                reply.content
            );

            let cut = reply.finish_reason.as_deref() == Some("length");
            let parsed = match classify_reply(&reply) {
                // A call that was cut off mid-block may have partial content.
                Parsed::Call {
                    from_text: true, ..
                } if cut => Parsed::Bad {
                    error: "the reply was cut off".into(),
                },
                p => p,
            };
            match parsed {
                Parsed::Answer(text) => {
                    // Small models often answer a failed call with "I will now
                    // read the file." and no call, which would end the task
                    // half-done. One nudge per task turns that into the call.
                    let after_error = self.last_result_failed();
                    self.history
                        .push(ChatMessage::text("assistant", reply.content.clone()));
                    if after_error && !t.error_nudge_used {
                        t.error_nudge_used = true;
                        cx.host.emit(AgentEvent::AssistantText {
                            text,
                            is_final: false,
                        });
                        self.push_note(AFTER_ERROR_NUDGE);
                        continue;
                    }
                    cx.host.emit(AgentEvent::AssistantText {
                        text,
                        is_final: true,
                    });
                    return DoneReason::Finished;
                }
                Parsed::Bad { error } => {
                    t.malformed_streak += 1;
                    let note = if cut && !t.cut_off_used {
                        t.cut_off_used = true;
                        t.malformed_streak -= 1;
                        proto::CUT_OFF_NOTE.to_string()
                    } else {
                        proto::malformed_note(&error)
                    };
                    log::info!("agent: unreadable reply ({error})");
                    if t.malformed_streak >= 2 {
                        self.push_failed_reply(&reply, &note);
                        cx.host.emit(AgentEvent::Notice {
                            text: format!(
                                "the model's tool calls could not be read twice in a row ({error}); stopping"
                            ),
                        });
                        return DoneReason::Error;
                    }
                    self.push_failed_reply(&reply, &note);
                    cx.host.emit(AgentEvent::Notice {
                        text: format!(
                            "the model's tool call could not be read ({error}); asking it to retry"
                        ),
                    });
                }
                Parsed::Call {
                    tool,
                    id,
                    prose,
                    extra,
                    ..
                } => {
                    log::debug!("agent call: {}", args_json(&tool));
                    t.malformed_streak = 0;
                    match self
                        .handle_call(cx, t, tool, id, prose, &reply.content, extra)
                        .await
                    {
                        Flow::Continue => {}
                        Flow::End(reason) => return reason,
                    }
                }
            }
        }
    }

    fn last_result_failed(&self) -> bool {
        self.history.last().is_some_and(|m| {
            m.role == "tool"
                && m.content
                    .lines()
                    .next()
                    .is_some_and(|l| l.starts_with("[tool result:") && l.ends_with("error]"))
        })
    }

    fn stopped(&mut self) -> DoneReason {
        self.push_note(proto::STOPPED_NOTE);
        DoneReason::Stopped
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_call(
        &mut self,
        cx: &Cx<'_>,
        t: &mut TaskLocal,
        tool: Tool,
        id: String,
        prose: String,
        raw: &str,
        extra: bool,
    ) -> Flow {
        let cfg = cx.cfg;
        let host = cx.host;

        self.history.push(ChatMessage {
            role: "assistant".into(),
            content: raw.to_string(),
            tool_calls: vec![ToolCall {
                id: id.clone(),
                name: tool.name().to_string(),
                arguments: args_json(&tool),
            }],
            ..Default::default()
        });

        if let Tool::Finish { summary } = &tool {
            let text = if summary.trim().is_empty() {
                prose
            } else {
                summary.clone()
            };
            self.push_result(&tool, &id, true, "task marked finished");
            if !text.trim().is_empty() {
                host.emit(AgentEvent::AssistantText {
                    text,
                    is_final: true,
                });
            }
            return Flow::End(DoneReason::Finished);
        }

        if !prose.is_empty() {
            host.emit(AgentEvent::AssistantText {
                text: prose,
                is_final: false,
            });
        }
        host.emit(AgentEvent::ToolCall {
            id: id.clone(),
            tool: tool.name().to_string(),
            title: tool.title(),
        });

        // Guards: refuse, then one tools-off wrap-up turn.
        let immediate = t.calls.last() == Some(&tool);
        let seen = t.calls.iter().filter(|c| **c == tool).count();
        if immediate || seen >= STUCK_CALL_THRESHOLD {
            let note = if immediate {
                proto::REPEATED_CALL_NOTE
            } else {
                proto::STUCK_LOOP_NOTE
            };
            self.push_result(&tool, &id, false, note);
            host.emit(AgentEvent::ToolResult {
                id,
                ok: false,
                output: note.to_string(),
            });
            host.emit(AgentEvent::Notice {
                text: "the model kept repeating the same tool call; asking it to wrap up".into(),
            });
            self.push_note(proto::FINAL_ANSWER_PROMPT);
            self.closing_turn(
                cx,
                t,
                "I stopped because I kept repeating the same tool call. Nothing more was done.",
            )
            .await;
            return Flow::End(DoneReason::Guard);
        }
        // Before recording the call, so a deliberate identical re-send isn't
        // mistaken for the model looping.
        if !t.warned_writes.contains(&tool) {
            if let Some(warning) = tools::destructive_write_warning(&self.workspace, &tool) {
                t.warned_writes.push(tool.clone());
                self.push_result(&tool, &id, false, &warning);
                host.emit(AgentEvent::ToolResult {
                    id,
                    ok: false,
                    output: warning,
                });
                return Flow::Continue;
            }
        }
        t.calls.push(tool.clone());

        let gate = self.gate(cx, t, &tool, &id).await;
        let (ok, body) = match gate {
            Gate::Manual(command) => {
                let body =
                    "[not run: this needs administrator rights, which are not available here. \
The user will run it themselves.]";
                self.push_result(&tool, &id, false, body);
                host.emit(AgentEvent::ToolResult {
                    id,
                    ok: false,
                    output: body.to_string(),
                });
                memory::record_blocked(cfg, &tool.title(), "needs administrator rights");
                host.emit(AgentEvent::ManualCommand { command });
                return Flow::End(DoneReason::Finished);
            }
            Gate::Stopped => {
                let body = proto::STOPPED_NOTE;
                self.push_result(&tool, &id, false, body);
                host.emit(AgentEvent::ToolResult {
                    id,
                    ok: false,
                    output: "stopped".into(),
                });
                return Flow::End(self.stopped());
            }
            Gate::Denied => {
                self.push_result(&tool, &id, false, proto::DENIED_NOTE);
                host.emit(AgentEvent::ToolResult {
                    id,
                    ok: false,
                    output: "denied".into(),
                });
                memory::record_blocked(cfg, &tool.title(), "the user denied it");
                self.closing_turn(cx, t, "Understood, I did not run that.")
                    .await;
                return Flow::End(DoneReason::Denied);
            }
            Gate::Fail(e) => (false, e),
            Gate::Run => {
                let (ok, body, cancelled) = self.execute(cx, t, &tool).await;
                if !tools::is_read_only(&tool) {
                    memory::record_action(cfg, &action_description(&tool, &body), ok);
                }
                if cancelled {
                    self.push_result(&tool, &id, false, &body);
                    host.emit(AgentEvent::ToolResult {
                        id,
                        ok: false,
                        output: cap_display(&body),
                    });
                    return Flow::End(self.stopped());
                }
                (ok, body)
            }
        };

        let model_body = if extra {
            format!("{EXTRA_CALLS_NOTE}\n{body}")
        } else {
            body.clone()
        };
        self.push_result(&tool, &id, ok, &model_body);
        host.emit(AgentEvent::ToolResult {
            id,
            ok,
            output: cap_display(&body),
        });
        Flow::Continue
    }

    /// One more model turn with no tools, ending the task with its text.
    /// `fallback` is shown when the model has nothing to say.
    async fn closing_turn(&mut self, cx: &Cx<'_>, t: &mut TaskLocal, fallback: &str) {
        match self.model_turn(cx, t, false).await {
            Ok(reply) => {
                if let Some(r) = reply.reasoning.as_deref().filter(|r| !r.trim().is_empty()) {
                    cx.host.emit(AgentEvent::Thinking {
                        text: r.to_string(),
                    });
                }
                let mut text = proto::strip_tool_blocks(&reply.content).trim().to_string();
                if text.is_empty() {
                    text = fallback.to_string();
                }
                self.history
                    .push(ChatMessage::text("assistant", text.clone()));
                cx.host.emit(AgentEvent::AssistantText {
                    text,
                    is_final: true,
                });
            }
            Err(TurnError::Cancelled) => {
                self.stopped();
            }
            Err(TurnError::Failed(message)) => cx.host.emit(AgentEvent::Error { message }),
        }
    }

    // --- Policy ---

    async fn gate(&mut self, cx: &Cx<'_>, t: &TaskLocal, tool: &Tool, id: &str) -> Gate {
        if tools::is_read_only(tool) {
            return Gate::Run;
        }
        match tool {
            Tool::RunCommand { command } => {
                if sandbox::needs_privilege(command).is_some() {
                    return Gate::Manual(command.clone());
                }
                if sandbox::classify_command(command) == Classification::ReadOnly {
                    return Gate::Run;
                }
                let mut allowed = cx.cfg.auto_approve.clone();
                allowed.extend(
                    self.faded
                        .iter()
                        .filter_map(|k| k.strip_prefix("cmd:").map(str::to_string)),
                );
                if sandbox::is_auto_approved(command, &allowed) {
                    return Gate::Run;
                }
                let program = first_program(command);
                let can_always_allow = program
                    .as_ref()
                    .is_some_and(|p| sandbox::is_auto_approved(command, std::slice::from_ref(p)));
                let key = program
                    .filter(|_| can_always_allow)
                    .map(|p| format!("cmd:{p}"));
                self.ask(
                    cx,
                    ConfirmRequest {
                        id: id.to_string(),
                        title: tool.title(),
                        command: Some(command.clone()),
                        diff: None,
                        can_always_allow,
                    },
                    key,
                )
                .await
            }
            other if !tools::needs_confirmation(other) => Gate::Run,
            other => {
                // Off the runtime: a stuck filesystem must not freeze Stop.
                let (ws, tool, state) = (self.workspace.clone(), other.clone(), t.state.clone());
                let job = tokio::task::spawn_blocking(move || ws.preview(&tool, &state));
                let planned = tokio::select! {
                    joined = job => joined,
                    _ = cx.cancel.cancelled() => return Gate::Stopped,
                };
                let preview = match planned {
                    Ok(Ok(p)) => p,
                    Ok(Err(e)) => return Gate::Fail(e),
                    Err(_) => return Gate::Fail("the file check failed unexpectedly".into()),
                };
                let key = format!("tool:{}", other.name());
                if self.faded.contains(&key) {
                    return Gate::Run;
                }
                self.ask(
                    cx,
                    ConfirmRequest {
                        id: id.to_string(),
                        title: preview.title,
                        command: None,
                        diff: preview.diff,
                        can_always_allow: true,
                    },
                    Some(key),
                )
                .await
            }
        }
    }

    async fn ask(&mut self, cx: &Cx<'_>, req: ConfirmRequest, key: Option<String>) -> Gate {
        let decision = cx.host.confirm(req).await;
        if cx.cancel.is_cancelled() {
            return Gate::Stopped;
        }
        match decision {
            Decision::Approve => {
                if let Some(key) = &key {
                    self.fade_threshold_note(key, cx.cfg.confirm_fade_after, cx.host);
                }
                Gate::Run
            }
            Decision::AlwaysAllow => {
                if let Some(key) = key {
                    if let Some(prog) = key.strip_prefix("cmd:") {
                        persist_auto_approve(prog);
                    }
                    self.faded.insert(key);
                }
                Gate::Run
            }
            Decision::Deny => {
                if let Some(key) = &key {
                    self.approvals.remove(key);
                    self.faded.remove(key);
                }
                Gate::Denied
            }
        }
    }

    // --- Execution ---

    /// `(ok, text for the model, cancelled)`.
    async fn execute(
        &mut self,
        cx: &Cx<'_>,
        t: &mut TaskLocal,
        tool: &Tool,
    ) -> (bool, String, bool) {
        if let Tool::RunCommand { command } = tool {
            return self.run_command(cx, command).await;
        }
        // Off the runtime, on a copy of the task state: on Stop the op may
        // finish in the background and its record is dropped with the task.
        let (ws, tool, mut state) = (self.workspace.clone(), tool.clone(), t.state.clone());
        let job = tokio::task::spawn_blocking(move || {
            let result = ws.execute(&tool, &mut state);
            (result, state)
        });
        tokio::select! {
            joined = job => match joined {
                Ok((result, state)) => {
                    t.state = state;
                    match result {
                        Ok(text) => (true, text, false),
                        Err(e) => (false, e, false),
                    }
                }
                Err(_) => (false, "the file operation failed unexpectedly".into(), false),
            },
            _ = cx.cancel.cancelled() => (false, "stopped".into(), true),
        }
    }

    async fn run_command(&self, cx: &Cx<'_>, command: &str) -> (bool, String, bool) {
        let cfg = cx.cfg;
        let shims = shim_dir();
        if let Err(e) = sandbox::ensure_shims(&shims) {
            return (false, format!("cannot set up the sandbox: {e}"), false);
        }
        let scratch = cfg.memory_enabled.then(memory::temp_dir);
        let trash = sandbox::resolve_trash_dir(cfg);
        let secs = match cfg.command_timeout_secs {
            0 => DEFAULT_COMMAND_TIMEOUT_SECS,
            n => n as u64,
        };
        let outcome = sandbox::run_sandboxed_async(
            self.workspace.root(),
            &shims,
            &cfg.granted_paths,
            scratch.as_deref(),
            &trash,
            command,
            Duration::from_secs(secs),
            cx.cancel,
        )
        .await;
        match outcome {
            Err(e) => (false, format!("could not run the command: {e}"), false),
            Ok(o) => {
                let mut body = o.stdout.clone();
                if !o.stderr.is_empty() {
                    if !body.is_empty() && !body.ends_with('\n') {
                        body.push('\n');
                    }
                    body.push_str("[stderr]\n");
                    body.push_str(&o.stderr);
                }
                if body.trim().is_empty() {
                    body = "(no output)".into();
                }
                if !body.ends_with('\n') {
                    body.push('\n');
                }
                if o.cancelled {
                    body.push_str("[stopped by the user; the command was killed]");
                } else if o.timed_out {
                    body.push_str(&format!("[timed out after {secs}s and was killed]"));
                } else {
                    body.push_str(&format!("[exit code {}]", o.exit_code));
                }
                (
                    !o.timed_out && !o.cancelled && o.exit_code == 0,
                    body,
                    o.cancelled,
                )
            }
        }
    }

    // --- Model I/O ---

    /// Fits the history to the budget and asks the model once. With
    /// `offer_tools = false` (wrap-up turns) no tools are attached.
    async fn model_turn(
        &mut self,
        cx: &Cx<'_>,
        t: &mut TaskLocal,
        offer_tools: bool,
    ) -> Result<ChatReply, TurnError> {
        let cfg = cx.cfg;
        loop {
            let native = offer_tools && self.transport == Transport::Native;
            let system = proto::system_prompt(
                self.transport,
                &root_note(self.workspace.root(), &cfg.granted_paths),
                cx.rules,
                &cfg.system_prompt,
            );
            // No scratch-dir note: small models then reach for that path with
            // the file tools, which (rightly) refuse the app's own folders.
            let record = memory::build_block(cfg).filter(|r| !r.is_empty());
            let specs = if native {
                proto::tool_specs()
            } else {
                Vec::new()
            };
            let spec_tokens = if native {
                context::estimate_tokens(
                    &serde_json::to_string(&specs_json(&specs)).unwrap_or_default(),
                )
            } else {
                0
            };
            let system_tokens = context::estimate_tokens(&system)
                + record.as_deref().map_or(0, context::estimate_tokens)
                + spec_tokens;

            let summarizer = cfg.summarize_before_dropping.then(|| context::Summarizer {
                endpoint: &cfg.endpoint,
                model: &cfg.model,
                api_key: &cfg.api_key,
            });
            let fit = context::fit_task_to_budget(
                system_tokens,
                self.history.clone(),
                cfg.max_context_tokens as usize,
                summarizer,
                self.task_start,
                cfg.memory_enabled,
            )
            .await;
            if let Some(rewritten) = fit.rewritten_history {
                self.history = rewritten;
                self.task_start = fit.rewritten_task_start;
                if let Some(summary) = &fit.summary {
                    log::warn!(
                        "agent: summarized {} old message(s):\n{summary}",
                        fit.summarized
                    );
                    cx.host.emit(AgentEvent::Notice {
                        text: format!(
                            "summarized {} older messages to fit the context window (model-written, may be lossy)",
                            fit.summarized
                        ),
                    });
                }
            }
            if fit.condensed > t.noticed_condensed {
                t.noticed_condensed = fit.condensed;
                cx.host.emit(AgentEvent::Notice {
                    text: format!(
                        "condensed {} earlier step(s) to fit the context window",
                        fit.condensed
                    ),
                });
            }
            if fit.dropped > t.noticed_dropped {
                t.noticed_dropped = fit.dropped;
                cx.host.emit(AgentEvent::Notice {
                    text: format!(
                        "dropped {} old message(s) to fit the context window",
                        fit.dropped
                    ),
                });
            }

            log::debug!("agent request: ~{} tokens estimated", fit.estimated_tokens);
            if t.steps == 1 {
                log::debug!("agent system prompt ({:?}):\n{system}", self.transport);
            }
            let request = build_request(system, fit.messages, record.as_deref(), self.transport);
            let opts = ChatOptions {
                temperature: cfg.temperature,
                tools: native.then_some(specs.as_slice()),
                ..Default::default()
            };
            let send =
                llm::send_chat_full(&cfg.endpoint, &cfg.model, &cfg.api_key, &request, &opts);
            let result = tokio::select! {
                r = send => r,
                _ = cx.cancel.cancelled() => return Err(TurnError::Cancelled),
            };
            match result {
                Ok(reply) => return Ok(reply),
                Err(e)
                    if native
                        && cfg.tool_calling.trim() == "auto"
                        && llm::is_tools_unsupported(&e) =>
                {
                    log::info!("agent: model rejected native tools ({e}); using text mode");
                    self.fell_back = true;
                    self.transport = Transport::Text;
                    cx.host.emit(AgentEvent::Notice {
                        text: "this model doesn't support native tools; using text mode".into(),
                    });
                }
                Err(e) => return Err(TurnError::Failed(e.to_string())),
            }
        }
    }
}

fn specs_json(specs: &[llm::ToolSpec]) -> Vec<serde_json::Value> {
    specs
        .iter()
        .map(|s| serde_json::json!({"name": s.name, "description": s.description, "parameters": s.parameters}))
        .collect()
}

fn action_description(tool: &Tool, body: &str) -> String {
    match tool {
        Tool::RunCommand { .. } => match body.rsplit_once("[exit code ") {
            Some((_, code)) => format!(
                "{} (exit {})",
                tool.title(),
                code.trim_end_matches(']').trim()
            ),
            None => tool.title(),
        },
        _ => tool.title(),
    }
}

/// "Always allow": remember the program across sessions. Best effort.
fn persist_auto_approve(program: &str) {
    match config::load_or_init() {
        Ok(mut cfg) => {
            if !cfg.auto_approve.iter().any(|b| b == program) {
                cfg.auto_approve.push(program.to_string());
                if let Err(e) = config::save(&cfg) {
                    log::warn!("agent: could not save auto-approve for {program}: {e}");
                }
            }
        }
        Err(e) => log::warn!("agent: could not load config to save auto-approve: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    // --- Scripted LLM endpoint ---

    type Requests = Arc<Mutex<Vec<serde_json::Value>>>;

    /// One scripted reply per request, in order: `(status, body)`.
    fn mock_llm(script: Vec<(u16, String)>) -> (String, Requests) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let requests: Requests = Arc::default();
        let seen = requests.clone();
        std::thread::spawn(move || {
            let mut script: VecDeque<_> = script.into();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (body_start, len) = loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break (0, 0);
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (p + 4, len);
                    }
                };
                while buf.len() < body_start + len {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                if let Ok(v) = serde_json::from_slice(&buf[body_start..]) {
                    seen.lock().unwrap().push(v);
                }
                let (status, body) = script
                    .pop_front()
                    .unwrap_or((500, json!({"error": "script exhausted"}).to_string()));
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (url, requests)
    }

    fn text_reply(content: &str) -> (u16, String) {
        (
            200,
            json!({"choices": [{"message": {"role": "assistant", "content": content}, "finish_reason": "stop"}]})
                .to_string(),
        )
    }

    fn call_reply(name: &str, args: serde_json::Value) -> (u16, String) {
        (
            200,
            json!({"choices": [{"message": {"role": "assistant", "content": "", "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": name, "arguments": args.to_string()}}
            ]}, "finish_reason": "tool_calls"}]})
            .to_string(),
        )
    }

    // --- Host ---

    #[derive(Default)]
    struct TestHost {
        events: Mutex<Vec<AgentEvent>>,
        decisions: Mutex<VecDeque<Decision>>,
        confirms: Mutex<Vec<ConfirmRequest>>,
    }

    impl TestHost {
        fn with(decisions: &[Decision]) -> Self {
            Self {
                decisions: Mutex::new(decisions.iter().copied().collect()),
                ..Default::default()
            }
        }
        fn events(&self) -> Vec<AgentEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl AgentHost for TestHost {
        fn emit(&self, ev: AgentEvent) {
            self.events.lock().unwrap().push(ev);
        }
        fn confirm<'a>(&'a self, req: ConfirmRequest) -> BoxFuture<'a, Decision> {
            self.confirms.lock().unwrap().push(req);
            let d = self
                .decisions
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Decision::Deny);
            Box::pin(async move { d })
        }
    }

    // --- Fixture ---

    struct Fx {
        dir: PathBuf,
        cfg: AppConfig,
        session: AgentSession,
    }

    fn fixture(name: &str, endpoint: String, tool_calling: &str) -> Fx {
        let dir =
            std::env::temp_dir().join(format!("llm-assistant-agent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("work");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "buy milk\n").unwrap();
        let cfg = AppConfig {
            endpoint,
            model: "test".into(),
            tool_calling: tool_calling.into(),
            memory_enabled: false,
            sandbox_trash_dir: dir.join("trash").display().to_string(),
            max_context_tokens: 0,
            ..Default::default()
        };
        let session = AgentSession::new(&root, &cfg).unwrap();
        Fx { dir, cfg, session }
    }

    async fn task(fx: &mut Fx, host: &TestHost, msg: &str) -> TaskOutcome {
        run_task_with(
            &fx.cfg,
            &mut fx.session,
            "",
            msg.into(),
            host,
            &Cancel::new(),
        )
        .await
    }

    fn final_text(host: &TestHost) -> Option<String> {
        host.events().into_iter().find_map(|e| match e {
            AgentEvent::AssistantText {
                text,
                is_final: true,
            } => Some(text),
            _ => None,
        })
    }

    #[tokio::test]
    async fn native_call_then_answer() {
        let (url, reqs) = mock_llm(vec![
            call_reply("list_dir", json!({"path": "."})),
            text_reply("The folder has a.txt."),
        ]);
        let mut fx = fixture("native", url, "auto");
        let host = TestHost::default();
        let out = task(&mut fx, &host, "what is here?").await;
        assert_eq!(out.reason, DoneReason::Finished);

        let ev = host.events();
        assert!(ev
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { tool, .. } if tool == "list_dir")));
        assert!(ev.iter().any(|e| matches!(e, AgentEvent::ToolResult { ok: true, output, .. } if output.contains("a.txt"))));
        assert_eq!(final_text(&host).as_deref(), Some("The folder has a.txt."));
        assert!(matches!(
            ev.last(),
            Some(AgentEvent::Done {
                reason: DoneReason::Finished
            })
        ));

        let reqs = reqs.lock().unwrap();
        assert!(reqs[0].get("tools").is_some(), "native mode attaches tools");
        let msgs = reqs[1]["messages"].as_array().unwrap();
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool"]);
        assert_eq!(msgs[3]["tool_call_id"], "call_1");
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "list_dir");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn a_stop_right_after_a_failed_call_gets_one_nudge() {
        let (url, reqs) = mock_llm(vec![
            call_reply(
                "edit_file",
                json!({"path": "a.txt", "edits": [{"old_text": "x", "new_text": "y"}]}),
            ),
            text_reply("I will read the file first."),
            call_reply("read_file", json!({"path": "a.txt"})),
            text_reply("It is read."),
        ]);
        let mut fx = fixture("nudge", url, "native");
        let host = TestHost::default();
        let out = task(&mut fx, &host, "change a.txt").await;
        assert_eq!(out.reason, DoneReason::Finished);
        assert_eq!(final_text(&host).as_deref(), Some("It is read."));
        let reqs = reqs.lock().unwrap();
        assert_eq!(reqs.len(), 4, "the announce-only reply was not the end");
        let nudged = reqs[2]["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(nudged.contains("make that tool call now"), "{nudged}");
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn a_write_that_wipes_most_of_a_file_is_questioned_once() {
        let (url, _) = mock_llm(vec![
            call_reply("read_file", json!({"path": "a.txt"})),
            call_reply("write_file", json!({"path": "a.txt", "content": "eggs\n"})),
            call_reply(
                "write_file",
                json!({"path": "a.txt", "content": "eggs\n", "append": true}),
            ),
            text_reply("Added."),
        ]);
        let mut fx = fixture("wipe", url, "native");
        let host = TestHost::with(&[Decision::Approve]);
        let out = task(&mut fx, &host, "add eggs").await;
        assert_eq!(out.reason, DoneReason::Finished);
        let text = std::fs::read_to_string(fx.session.root().join("a.txt")).unwrap();
        assert!(text.contains("milk") && text.ends_with("eggs\n"), "{text}");
        assert_eq!(
            host.confirms.lock().unwrap().len(),
            1,
            "only the append was asked"
        );
        assert!(host.events().iter().any(|e| matches!(e, AgentEvent::ToolResult { ok: false, output, .. } if output.contains("append: true"))));
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn a_deliberate_full_replace_goes_through_on_resend() {
        let (url, _) = mock_llm(vec![
            call_reply("read_file", json!({"path": "a.txt"})),
            call_reply("write_file", json!({"path": "a.txt", "content": "new\n"})),
            call_reply("write_file", json!({"path": "a.txt", "content": "new\n"})),
            text_reply("Replaced."),
        ]);
        let mut fx = fixture("replace", url, "native");
        let host = TestHost::with(&[Decision::Approve]);
        let out = task(&mut fx, &host, "replace a.txt with new").await;
        assert_eq!(out.reason, DoneReason::Finished);
        assert_eq!(
            std::fs::read_to_string(fx.session.root().join("a.txt")).unwrap(),
            "new\n"
        );
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn approved_write_lands_and_is_reported() {
        let (url, _) = mock_llm(vec![
            call_reply("write_file", json!({"path": "b.txt", "content": "eggs\n"})),
            text_reply("Done."),
        ]);
        let mut fx = fixture("write", url, "native");
        let host = TestHost::with(&[Decision::Approve]);
        let out = task(&mut fx, &host, "make b.txt").await;
        assert_eq!(out.reason, DoneReason::Finished);
        assert_eq!(
            std::fs::read_to_string(fx.session.root().join("b.txt")).unwrap(),
            "eggs\n"
        );

        let confirms = host.confirms.lock().unwrap();
        assert_eq!(confirms.len(), 1);
        assert!(confirms[0].diff.as_deref().unwrap_or("").contains("+eggs"));
        assert!(host
            .events()
            .iter()
            .any(|e| matches!(e, AgentEvent::ChangedFiles { files }
            if files.len() == 1 && files[0].path == "b.txt" && files[0].change == "created")));
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn approved_append_keeps_old_content() {
        let (url, _) = mock_llm(vec![
            call_reply(
                "write_file",
                json!({"path": "a.txt", "content": "eggs\n", "append": true}),
            ),
            text_reply("Done."),
        ]);
        let mut fx = fixture("append", url, "native");
        let host = TestHost::with(&[Decision::Approve]);
        let out = task(&mut fx, &host, "add eggs to a.txt").await;
        assert_eq!(out.reason, DoneReason::Finished);
        assert_eq!(
            std::fs::read_to_string(fx.session.root().join("a.txt")).unwrap(),
            "buy milk\neggs\n"
        );
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn denied_write_changes_nothing_and_gets_one_closing_turn() {
        let (url, reqs) = mock_llm(vec![
            text_reply("```write b.txt\neggs\n```"),
            text_reply("Okay, I left it alone."),
        ]);
        let mut fx = fixture("deny", url, "text");
        let host = TestHost::with(&[Decision::Deny]);
        let out = task(&mut fx, &host, "make b.txt").await;
        assert_eq!(out.reason, DoneReason::Denied);
        assert!(!fx.session.root().join("b.txt").exists());
        assert_eq!(final_text(&host).as_deref(), Some("Okay, I left it alone."));

        let reqs = reqs.lock().unwrap();
        assert!(
            reqs[0].get("tools").is_none(),
            "text mode attaches no tools"
        );
        assert!(reqs[1].get("tools").is_none());
        let msgs = reqs[1]["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        assert_eq!(last["role"], "user");
        assert!(last["content"].as_str().unwrap().contains("DENIED"));
        assert!(msgs.iter().all(|m| m.get("tool_calls").is_none()));
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn a_model_without_tools_falls_back_to_text_for_the_session() {
        let (url, reqs) = mock_llm(vec![
            (
                400,
                json!({"error": {"message": "model does not support tools"}}).to_string(),
            ),
            text_reply("```tool\n{\"tool\": \"list_dir\", \"path\": \".\"}\n```"),
            text_reply("It has a.txt."),
        ]);
        let mut fx = fixture("fallback", url, "auto");
        let host = TestHost::default();
        let out = task(&mut fx, &host, "what is here?").await;
        assert_eq!(out.reason, DoneReason::Finished);
        assert!(host
            .events()
            .iter()
            .any(|e| matches!(e, AgentEvent::Notice { text } if text.contains("text mode"))));
        assert_eq!(fx.session.transport, Transport::Text);
        let reqs = reqs.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        assert!(reqs[0].get("tools").is_some());
        assert!(reqs[1].get("tools").is_none());
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn forced_native_surfaces_the_error_instead_of_falling_back() {
        let (url, _) = mock_llm(vec![(
            400,
            json!({"error": {"message": "model does not support tools"}}).to_string(),
        )]);
        let mut fx = fixture("forced", url, "native");
        let host = TestHost::default();
        let out = task(&mut fx, &host, "hi").await;
        assert_eq!(out.reason, DoneReason::Error);
        assert!(host
            .events()
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { .. })));
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn two_unreadable_replies_in_a_row_end_the_task() {
        let bad = "```tool\n{\"tool\": \"read_file\", \"oops\": }\n```";
        let (url, reqs) = mock_llm(vec![text_reply(bad), text_reply(bad)]);
        let mut fx = fixture("malformed", url, "text");
        let host = TestHost::default();
        let out = task(&mut fx, &host, "read it").await;
        assert_eq!(out.reason, DoneReason::Error);
        let reqs = reqs.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        let last = reqs[1]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert!(last["content"]
            .as_str()
            .unwrap()
            .contains("could not be read"));
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn an_immediately_repeated_call_is_refused_and_wrapped_up() {
        let (url, reqs) = mock_llm(vec![
            call_reply("list_dir", json!({})),
            call_reply("list_dir", json!({})),
            text_reply("The folder has a.txt."),
        ]);
        let mut fx = fixture("repeat", url, "native");
        let host = TestHost::default();
        let out = task(&mut fx, &host, "list it").await;
        assert_eq!(out.reason, DoneReason::Guard);
        assert_eq!(final_text(&host).as_deref(), Some("The folder has a.txt."));
        let reqs = reqs.lock().unwrap();
        assert!(
            reqs[2].get("tools").is_none(),
            "the wrap-up offers no tools"
        );
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn the_step_cap_ends_the_task() {
        let (url, _) = mock_llm(vec![
            call_reply("list_dir", json!({})),
            call_reply("read_file", json!({"path": "a.txt"})),
        ]);
        let mut fx = fixture("cap", url, "native");
        fx.cfg.max_auto_steps = 2;
        let host = TestHost::default();
        let out = task(&mut fx, &host, "go").await;
        assert_eq!(out.reason, DoneReason::StepCap);
        assert!(host
            .events()
            .iter()
            .any(|e| matches!(e, AgentEvent::Notice { text } if text.contains("paused"))));
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn a_cancelled_task_stops_without_asking_the_model() {
        let (url, reqs) = mock_llm(vec![text_reply("never")]);
        let mut fx = fixture("cancel", url, "native");
        let host = TestHost::default();
        let cancel = Cancel::new();
        cancel.cancel();
        let out = run_task_with(&fx.cfg, &mut fx.session, "", "go".into(), &host, &cancel).await;
        assert_eq!(out.reason, DoneReason::Stopped);
        assert!(reqs.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[tokio::test]
    async fn a_second_task_continues_the_same_history_in_valid_shape() {
        let (url, reqs) = mock_llm(vec![
            call_reply("read_file", json!({"path": "a.txt"})),
            text_reply("It says buy milk."),
            text_reply("You're welcome."),
        ]);
        let mut fx = fixture("two", url, "native");
        let host = TestHost::default();
        task(&mut fx, &host, "read a.txt").await;
        task(&mut fx, &host, "thanks").await;
        assert_eq!(fx.session.task_start, 4);
        let reqs = reqs.lock().unwrap();
        let roles: Vec<&str> = reqs[2]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(
            roles,
            ["system", "user", "assistant", "tool", "assistant", "user"]
        );
        assert_eq!(fx.session.history().len(), 6);
        let _ = std::fs::remove_dir_all(&fx.dir);
    }

    #[test]
    fn requests_merge_adjacent_roles_and_text_mode_drops_tool_calls() {
        let mut call = ChatMessage::text("assistant", "```tool\n{}\n```");
        call.tool_calls = vec![ToolCall {
            id: "c".into(),
            name: "list_dir".into(),
            arguments: "{}".into(),
        }];
        let mut result = ChatMessage::text("tool", "[tool result: list_dir — ok]\nx");
        result.tool_call_id = Some("c".into());
        let hist = vec![
            ChatMessage::text("user", "one"),
            ChatMessage::text("user", "two"),
            call,
            result,
            ChatMessage::text("user", "note"),
        ];

        let text = build_request("sys".into(), hist.clone(), Some("RECORD"), Transport::Text);
        let roles: Vec<&str> = text.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
        assert!(text[1].content.contains("one\n\ntwo"));
        assert!(text[3].content.starts_with("[tool result: list_dir"));
        assert!(text[3].content.ends_with("note\n\nRECORD"));
        assert!(text.iter().all(|m| m.tool_calls.is_empty()));

        let native = build_request("sys".into(), hist, None, Transport::Native);
        let roles: Vec<&str> = native.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "tool", "user"]);
        assert_eq!(native[3].tool_call_id.as_deref(), Some("c"));
    }

    #[test]
    fn events_serialize_in_the_documented_shape() {
        let v = serde_json::to_value(AgentEvent::AssistantText {
            text: "hi".into(),
            is_final: true,
        })
        .unwrap();
        assert_eq!(
            v,
            json!({"type": "assistantText", "text": "hi", "final": true})
        );
        let v = serde_json::to_value(AgentEvent::Confirm {
            id: "1".into(),
            title: "t".into(),
            command: None,
            diff: None,
            can_always_allow: true,
        })
        .unwrap();
        assert_eq!(v["type"], "confirm");
        assert_eq!(v["canAlwaysAllow"], true);
        let v = serde_json::to_value(AgentEvent::Done {
            reason: DoneReason::StepCap,
        })
        .unwrap();
        assert_eq!(v, json!({"type": "done", "reason": "stepCap"}));
    }
}
