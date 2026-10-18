//! OpenAI-compatible `/chat/completions` client (Ollama, LM Studio, llama.cpp,
//! vLLM). Everything the rest of the app needs from a local model goes
//! through here: plain chat, streaming, native tool calls, JSON-mode calls.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// One client for the whole app. `read_timeout` applies between reads, so a
/// long generation or stream is fine but a stalled server isn't.
pub(crate) fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(300))
            .build()
            .unwrap_or_default()
    })
}

/// 16 hex chars; unique enough for message/tool-call ids, not security sensitive.
pub fn new_message_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{:016x}", h.finish())
}

/// A native tool call. `arguments` is the raw JSON string, as on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// `content` stays a plain string for all app logic; `images` (base64 data
/// URLs) only matter at the wire boundary in `to_wire`. Every other field is
/// `serde(default, skip_serializing_if)` so persisted `history.json` files and
/// plain-string construction keep working.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// Stable local id (UI/history bookkeeping). Never sent to the model.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Attachments sent *to* the model as vision input.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// Local paths of ComfyUI output on an assistant message. Separate from
    /// `images` so generated images are never re-sent as input.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generated_images: Vec<String>,
    /// Display-only reasoning (chat mode, `chat_persist_thinking`). Never sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Assistant message: native tool calls it made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Role `tool` message: which call this is the result of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        ChatMessage {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    /// Fills `id` if it's empty.
    pub fn with_id(mut self) -> Self {
        if self.id.is_empty() {
            self.id = new_message_id();
        }
        self
    }
}

/// Function-tool definition; `parameters` is a JSON-schema object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct ChatOptions<'a> {
    pub temperature: f32,
    pub max_tokens: Option<u32>,
    pub tools: Option<&'a [ToolSpec]>,
    pub response_format: Option<serde_json::Value>,
    /// OpenAI-style `reasoning_effort`; "none" turns a thinking model's
    /// reasoning off (LM Studio, vLLM, OpenAI). Unset = server default.
    pub reasoning_effort: Option<&'a str>,
}

#[derive(Debug, Clone, Default)]
pub struct ChatReply {
    /// Reply text with any reasoning removed.
    pub content: String,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
}

/// Non-2xx response. Callers `downcast_ref::<HttpError>()` to branch on it.
#[derive(Debug, Clone)]
pub struct HttpError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match reqwest::StatusCode::from_u16(self.status) {
            Ok(s) => write!(f, "LLM endpoint returned {s}: {}", self.body),
            Err(_) => write!(f, "LLM endpoint returned {}: {}", self.status, self.body),
        }
    }
}

impl std::error::Error for HttpError {}

fn http_error(err: &anyhow::Error) -> Option<&HttpError> {
    err.downcast_ref::<HttpError>()
}

/// The model/server doesn't support native tool calling (Ollama: 400 "... does
/// not support tools").
pub fn is_tools_unsupported(err: &anyhow::Error) -> bool {
    http_error(err).is_some_and(|e| {
        matches!(e.status, 400 | 404 | 422 | 500) && e.body.to_lowercase().contains("tool")
    })
}

/// The server rejected `response_format` (or its schema/grammar).
pub fn is_response_format_unsupported(err: &anyhow::Error) -> bool {
    http_error(err).is_some_and(|e| {
        let b = e.body.to_lowercase();
        b.contains("response_format") || b.contains("json_schema") || b.contains("grammar")
    })
}

// --- Wire (request) types ---

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    temperature: f32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<WireTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<&'a serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
}

impl<'a> ChatRequest<'a> {
    fn new(
        model: &'a str,
        messages: &'a [ChatMessage],
        opts: &'a ChatOptions<'a>,
        stream: bool,
    ) -> Self {
        let tools = opts.tools.filter(|t| !t.is_empty());
        ChatRequest {
            model,
            messages: messages.iter().map(to_wire).collect(),
            temperature: opts.temperature,
            stream,
            max_tokens: opts.max_tokens,
            tools: tools.map(|t| {
                t.iter()
                    .map(|s| WireTool {
                        kind: "function",
                        function: WireToolFn {
                            name: &s.name,
                            description: &s.description,
                            parameters: &s.parameters,
                        },
                    })
                    .collect()
            }),
            tool_choice: tools.map(|_| "auto"),
            parallel_tool_calls: tools.map(|_| false),
            response_format: opts.response_format.as_ref(),
            reasoning_effort: opts.reasoning_effort,
        }
    }
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolFn<'a>,
}

#[derive(Serialize)]
struct WireToolFn<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

/// OpenAI vision content part.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentPart<'a> {
    Text { text: &'a str },
    ImageUrl { image_url: ImageUrl<'a> },
}

#[derive(Serialize)]
struct ImageUrl<'a> {
    url: &'a str,
}

/// Plain string unless the message has images.
#[derive(Serialize)]
#[serde(untagged)]
enum WireContent<'a> {
    Text(&'a str),
    Parts(Vec<ContentPart<'a>>),
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: WireContent<'a>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolCallFn<'a>,
}

#[derive(Serialize)]
struct WireToolCallFn<'a> {
    name: &'a str,
    arguments: &'a str,
}

fn to_wire(m: &ChatMessage) -> WireMessage<'_> {
    let content = if m.images.is_empty() {
        WireContent::Text(&m.content)
    } else {
        let mut parts = Vec::with_capacity(1 + m.images.len());
        if !m.content.is_empty() {
            parts.push(ContentPart::Text { text: &m.content });
        }
        parts.extend(m.images.iter().map(|url| ContentPart::ImageUrl {
            image_url: ImageUrl { url },
        }));
        WireContent::Parts(parts)
    };
    WireMessage {
        role: &m.role,
        content,
        tool_calls: m
            .tool_calls
            .iter()
            .map(|c| WireToolCall {
                id: &c.id,
                kind: "function",
                function: WireToolCallFn {
                    name: &c.name,
                    arguments: &c.arguments,
                },
            })
            .collect(),
        tool_call_id: m.tool_call_id.as_deref(),
    }
}

// --- Response parsing ---

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// `content` is null for tool-call-only or reasoning-only replies. Servers
/// use `reasoning_content` (vLLM, llama.cpp, LM Studio) or `reasoning`
/// (OpenRouter, newer LM Studio); both are separate fields because a server
/// sending both would make a serde alias a duplicate-field error.
#[derive(Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<InToolCall>>,
}

#[derive(Deserialize)]
struct InToolCall {
    #[serde(default)]
    id: Option<String>,
    function: InToolFn,
}

#[derive(Deserialize)]
struct InToolFn {
    #[serde(default)]
    name: String,
    /// A JSON string per spec, but some servers send an object.
    #[serde(default)]
    arguments: serde_json::Value,
}

fn normalize_tool_call(c: InToolCall) -> ToolCall {
    let arguments = match c.function.arguments {
        serde_json::Value::String(s) => s,
        serde_json::Value::Null => "{}".to_string(),
        other => other.to_string(),
    };
    ToolCall {
        id: c
            .id
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("call_{}", new_message_id())),
        name: c.function.name,
        arguments,
    }
}

/// Merges the separate reasoning field with any inline `<think>` tags in the
/// content; returns `(reasoning, clean content)`.
fn build_reply_text(content: String, reasoning_field: Option<String>) -> (Option<String>, String) {
    let (inline, content) = split_thinking(&content);
    let field = reasoning_field.filter(|r| !r.trim().is_empty());
    let reasoning = match (field, inline) {
        (Some(a), Some(b)) => Some(format!("{}\n\n{}", a.trim(), b)),
        (Some(a), None) => Some(a.trim().to_string()),
        (None, b) => b,
    };
    (reasoning, content)
}

fn parse_response(body: &str) -> anyhow::Result<ChatReply> {
    let parsed: ChatResponse = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("failed to parse LLM response ({e}): {body}"))?;
    let Some(choice) = parsed.choices.into_iter().next() else {
        match parsed.error {
            Some(err) => anyhow::bail!("endpoint returned no choices: {}", error_message(&err)),
            None => anyhow::bail!("endpoint returned no choices"),
        }
    };
    let m = choice.message;
    let field = m
        .reasoning_content
        .filter(|r| !r.trim().is_empty())
        .or(m.reasoning);
    let (reasoning, content) = build_reply_text(m.content.unwrap_or_default(), field);
    Ok(ChatReply {
        content,
        reasoning,
        tool_calls: m
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(normalize_tool_call)
            .collect(),
        finish_reason: choice.finish_reason,
    })
}

fn error_message(err: &serde_json::Value) -> String {
    match err {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(o) => o
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string()),
        other => other.to_string(),
    }
}

/// The legacy string contract: reasoning folded in as a `<think>` prefix.
fn fold_reply(reasoning: Option<String>, content: String) -> String {
    match reasoning {
        Some(r) => format!("<think>{r}</think>{content}"),
        None => content,
    }
}

// --- Reasoning-tag handling ---

/// Earliest occurrence of any of `tags` at or after `from`, as `(start, end)`.
fn find_tag(lower: &str, from: usize, tags: &[&str]) -> Option<(usize, usize)> {
    tags.iter()
        .filter_map(|t| {
            lower[from..]
                .find(t)
                .map(|i| (from + i, from + i + t.len()))
        })
        .min_by_key(|&(s, _)| s)
}

const OPEN_TAGS: [&str; 2] = ["<think>", "<thinking>"];
const CLOSE_TAGS: [&str; 2] = ["</think>", "</thinking>"];

/// Splits `<think>`/`<thinking>` reasoning from the reply. Also handles a
/// lone `</think>` (Qwen3/R1 templates put the opening tag in the prompt, so
/// the reply starts mid-reasoning) and an unclosed trailing `<think>`
/// (truncated). Returns `(reasoning joined by blank lines, trimmed content)`.
pub fn split_thinking(text: &str) -> (Option<String>, String) {
    // ASCII lowercasing keeps byte offsets aligned with `text`.
    let lower = text.to_ascii_lowercase();
    let mut reasoning: Vec<&str> = Vec::new();
    let mut out = String::new();
    let mut pos = 0;

    if let Some((cs, ce)) = find_tag(&lower, 0, &CLOSE_TAGS) {
        let open_first = find_tag(&lower, 0, &OPEN_TAGS).is_some_and(|(os, _)| os < cs);
        if !open_first {
            reasoning.push(&text[..cs]);
            pos = ce;
        }
    }
    while let Some((os, oe)) = find_tag(&lower, pos, &OPEN_TAGS) {
        out.push_str(&text[pos..os]);
        match find_tag(&lower, oe, &CLOSE_TAGS) {
            Some((cs, ce)) => {
                reasoning.push(&text[oe..cs]);
                pos = ce;
            }
            None => {
                reasoning.push(&text[oe..]);
                pos = text.len();
            }
        }
    }
    out.push_str(&text[pos..]);

    let parts: Vec<&str> = reasoning
        .iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .collect();
    let reasoning = (!parts.is_empty()).then(|| parts.join("\n\n"));
    (reasoning, out.trim().to_string())
}

// --- Requests ---

async fn post(
    endpoint: &str,
    api_key: &str,
    req: &ChatRequest<'_>,
) -> anyhow::Result<reqwest::Response> {
    let mut builder = client().post(endpoint).json(req);
    if !api_key.trim().is_empty() {
        builder = builder.bearer_auth(api_key);
    }
    let resp = builder.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(HttpError {
            status: status.as_u16(),
            body,
        }
        .into());
    }
    Ok(resp)
}

/// Full-fidelity non-streaming call: reasoning, tool calls and finish reason
/// come back separately. `api_key`, when non-empty, is sent as a Bearer token.
pub async fn send_chat_full(
    endpoint: &str,
    model: &str,
    api_key: &str,
    messages: &[ChatMessage],
    opts: &ChatOptions<'_>,
) -> anyhow::Result<ChatReply> {
    let _permit = crate::gpu::llm_permit().await;
    let req = ChatRequest::new(model, messages, opts, false);
    let body = post(endpoint, api_key, &req).await?.text().await?;
    parse_response(&body)
}

/// Plain chat returning a string with any reasoning folded in as a
/// `<think>..</think>` prefix (what `rules::extract_thinking_block` expects).
pub async fn send_chat(
    endpoint: &str,
    model: &str,
    api_key: &str,
    temperature: f32,
    messages: &[ChatMessage],
) -> anyhow::Result<String> {
    let opts = ChatOptions {
        temperature,
        ..Default::default()
    };
    let reply = send_chat_full(endpoint, model, api_key, messages, &opts).await?;
    Ok(fold_reply(reply.reasoning, reply.content))
}

// --- Embeddings ---

/// A busy model server loads the embedding model on first use, so this is
/// generous, but a hung one must not stall a chat turn for the 300 s read timeout.
const EMBED_TIMEOUT: Duration = Duration::from_secs(60);

/// `data[i].embedding` of an OpenAI-shape body, in input order (`index` wins
/// when present). Fails unless there is exactly one non-empty vector per input.
fn parse_embeddings(body: &str, expected: usize) -> anyhow::Result<Vec<Vec<f32>>> {
    let value: serde_json::Value = serde_json::from_str(body)?;
    let data = value
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow::anyhow!("embedding response has no data array"))?;
    let mut rows: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
    for (i, item) in data.iter().enumerate() {
        let vector: Vec<f32> = item
            .get("embedding")
            .and_then(|e| e.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|n| n.as_f64())
                    .map(|n| n as f32)
                    .collect()
            })
            .unwrap_or_default();
        let index = item
            .get("index")
            .and_then(|n| n.as_u64())
            .map_or(i, |n| n as usize);
        rows.push((index, vector));
    }
    rows.sort_by_key(|(i, _)| *i);
    if rows.len() != expected || rows.iter().any(|(_, v)| v.is_empty()) {
        anyhow::bail!(
            "embedding response had {} usable vectors for {expected} inputs",
            rows.iter().filter(|(_, v)| !v.is_empty()).count()
        );
    }
    Ok(rows.into_iter().map(|(_, v)| v).collect())
}

/// `POST {host}/v1/embeddings` on the chat endpoint's own server. Holds the
/// GPU permit like any other model request, so it waits out an image swap.
pub async fn embed(
    endpoint: &str,
    api_key: &str,
    model: &str,
    texts: &[String],
) -> anyhow::Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let host = crate::gpu::base_host(endpoint)
        .ok_or_else(|| anyhow::anyhow!("can't derive a server address from {endpoint:?}"))?;
    let _permit = crate::gpu::llm_permit().await;
    let mut builder = client()
        .post(format!("{host}/v1/embeddings"))
        .timeout(EMBED_TIMEOUT)
        .json(&serde_json::json!({"model": model, "input": texts}));
    if !api_key.trim().is_empty() {
        builder = builder.bearer_auth(api_key);
    }
    let resp = builder.send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(HttpError {
            status: status.as_u16(),
            body,
        }
        .into());
    }
    parse_embeddings(&body, texts.len())
}

// --- JSON-mode call ---

/// Cheapest-to-strictest `response_format` support level per (endpoint, model):
/// 0 = json_schema, 1 = json_object, 2 = none.
fn json_level_cache() -> &'static Mutex<HashMap<String, usize>> {
    static CACHE: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn response_format_for(
    level: usize,
    name: &str,
    schema: &serde_json::Value,
) -> Option<serde_json::Value> {
    match level {
        0 => Some(serde_json::json!({
            "type": "json_schema",
            "json_schema": { "name": name, "strict": true, "schema": schema },
        })),
        1 => Some(serde_json::json!({ "type": "json_object" })),
        _ => None,
    }
}

/// Chat call that must return a JSON object. Tries `json_schema`, then
/// `json_object`, then no `response_format`, remembering what worked; the
/// reply is parsed leniently (see `extract_json_object`).
#[allow(clippy::too_many_arguments)]
pub async fn send_chat_json(
    endpoint: &str,
    model: &str,
    api_key: &str,
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: Option<u32>,
    schema_name: &str,
    schema: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let key = format!("{endpoint}\n{model}");
    let mut level = json_level_cache()
        .lock()
        .ok()
        .and_then(|c| c.get(&key).copied())
        .unwrap_or(0);
    // These are short mechanical answers: a thinking model would spend the
    // token budget reasoning and return nothing (measured on gemma-4: empty
    // at max_tokens 10, 6.6 s uncapped, 0.26 s with reasoning off).
    let mut effort = (!effort_rejected(&key)).then_some("none");
    let mut cap = max_tokens;
    loop {
        let opts = ChatOptions {
            temperature,
            max_tokens: cap,
            tools: None,
            response_format: response_format_for(level, schema_name, schema),
            reasoning_effort: effort,
        };
        match send_chat_full(endpoint, model, api_key, messages, &opts).await {
            Ok(reply) => {
                if let Ok(mut c) = json_level_cache().lock() {
                    c.insert(key.clone(), level);
                }
                if let Some(v) = extract_json_object(&reply.content) {
                    return Ok(v);
                }
                // Cut off before the answer (a server that ignores
                // reasoning_effort): once more without the cap.
                if cap.is_some() && reply.finish_reason.as_deref() == Some("length") {
                    log::info!("send_chat_json: reply cut off at max_tokens, retrying uncapped");
                    cap = None;
                    continue;
                }
                return Err(anyhow::anyhow!("model reply was not a JSON object"));
            }
            Err(e)
                if effort.is_some()
                    && http_error(&e).is_some_and(|h| {
                        matches!(h.status, 400 | 422) && h.body.to_lowercase().contains("reasoning")
                    }) =>
            {
                log::info!("send_chat_json: reasoning_effort rejected ({e}), sending without it");
                if let Ok(mut c) = effort_rejected_cache().lock() {
                    c.insert(key.clone());
                }
                effort = None;
            }
            Err(e)
                if level < 2
                    && (is_response_format_unsupported(&e)
                        || http_error(&e).is_some_and(|h| matches!(h.status, 400 | 422))) =>
            {
                log::info!("send_chat_json: level {level} rejected ({e}), falling back");
                level += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

fn effort_rejected_cache() -> &'static Mutex<std::collections::HashSet<String>> {
    static CACHE: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn effort_rejected(key: &str) -> bool {
    effort_rejected_cache()
        .lock()
        .map(|c| c.contains(key))
        .unwrap_or(false)
}

fn parse_object(s: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(s.trim())
        .ok()
        .filter(|v| v.is_object())
}

/// Index of the `}` matching the `{` at `start`, respecting strings/escapes.
fn matching_brace(s: &str, start: usize) -> Option<usize> {
    let (mut depth, mut in_str, mut esc) = (0usize, false, false);
    for (i, b) in s.bytes().enumerate().skip(start) {
        if in_str {
            match (esc, b) {
                (true, _) => esc = false,
                (false, b'\\') => esc = true,
                (false, b'"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Lenient JSON-object extraction: whole text, then a ``` fence body (any
/// tag), then the first balanced `{...}`. Reasoning tags are dropped first.
pub fn extract_json_object(text: &str) -> Option<serde_json::Value> {
    let (_, body) = split_thinking(text);
    if let Some(v) = parse_object(&body) {
        return Some(v);
    }
    let mut rest = body.as_str();
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let Some(close) = after.find("```") else {
            break;
        };
        let inner = &after[..close];
        // Drop the info string (```json) up to the first newline.
        let inner = inner.split_once('\n').map_or(inner, |(_, b)| b);
        if let Some(v) = parse_object(inner) {
            return Some(v);
        }
        rest = &after[close + 3..];
    }
    let mut from = 0;
    while let Some(i) = body[from..].find('{') {
        let start = from + i;
        if let Some(end) = matching_brace(&body, start) {
            if let Some(v) = parse_object(&body[start..=end]) {
                return Some(v);
            }
        }
        from = start + 1;
    }
    None
}

// --- Streaming ---

/// One chunk's worth of a streaming reply. A backend that streams an inline
/// `<think>` tag in `content` gives no live signal; the caller extracts it
/// from the returned string afterwards.
pub enum ChatDelta<'a> {
    Content(&'a str),
    Reasoning(&'a str),
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: StreamDelta,
}

/// Tool-call and finish deltas are ignored (streaming is plain chat only).
#[derive(Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
}

/// Payload of a real `data:` line; `None` for blanks, `:` keep-alives and
/// other SSE fields.
fn sse_data_line(line: &str) -> Option<&str> {
    let payload = line.strip_prefix("data:")?.trim();
    (!payload.is_empty()).then_some(payload)
}

#[derive(Default)]
struct StreamAcc {
    content: String,
    reasoning: String,
}

#[derive(Debug, PartialEq)]
enum SseStep {
    Continue,
    Done,
}

/// Handles one newline-stripped SSE line. A `{"error": ...}` frame is an `Err`.
fn handle_sse_line(
    line: &str,
    acc: &mut StreamAcc,
    on_delta: &mut dyn FnMut(ChatDelta<'_>),
) -> anyhow::Result<SseStep> {
    let Some(payload) = sse_data_line(line) else {
        return Ok(SseStep::Continue);
    };
    if payload == "[DONE]" {
        return Ok(SseStep::Done);
    }
    let parsed: StreamChunk = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("send_chat_streaming: skipping unparsable chunk ({e}): {payload}");
            return Ok(SseStep::Continue);
        }
    };
    if let Some(err) = parsed.error.filter(|e| !e.is_null()) {
        anyhow::bail!("LLM stream error: {}", error_message(&err));
    }
    for choice in parsed.choices {
        let d = choice.delta;
        if let Some(piece) = d.content.filter(|s| !s.is_empty()) {
            on_delta(ChatDelta::Content(&piece));
            acc.content.push_str(&piece);
        }
        if let Some(piece) = d
            .reasoning_content
            .filter(|s| !s.is_empty())
            .or(d.reasoning.filter(|s| !s.is_empty()))
        {
            on_delta(ChatDelta::Reasoning(&piece));
            acc.reasoning.push_str(&piece);
        }
    }
    Ok(SseStep::Continue)
}

/// Streaming sibling of `send_chat`: same request and return contract, but
/// calls `on_delta` per chunk. `on_delta` is a sync callback (both callers,
/// a Tauri `Channel` and an axum `mpsc` sender, are sync).
pub async fn send_chat_streaming(
    endpoint: &str,
    model: &str,
    api_key: &str,
    temperature: f32,
    messages: &[ChatMessage],
    mut on_delta: impl FnMut(ChatDelta<'_>) + Send,
) -> anyhow::Result<String> {
    let opts = ChatOptions {
        temperature,
        ..Default::default()
    };
    let _permit = crate::gpu::llm_permit().await;
    let req = ChatRequest::new(model, messages, &opts, true);
    let resp = post(endpoint, api_key, &req).await?;

    let mut acc = StreamAcc::default();
    // Raw bytes, decoded per complete line, so a multi-byte UTF-8 char split
    // across network chunks isn't mangled. One `data:` line per event, as
    // every target backend sends.
    let mut buf: Vec<u8> = Vec::new();
    let mut byte_stream = resp.bytes_stream();
    use tokio_stream::StreamExt as _;
    let mut done = false;
    'outer: while let Some(chunk) = byte_stream.next().await {
        buf.extend_from_slice(&chunk?);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches(['\r', '\n']);
            if handle_sse_line(line, &mut acc, &mut on_delta)? == SseStep::Done {
                done = true;
                break 'outer;
            }
        }
    }
    // Stream closed without a trailing newline (or `[DONE]`).
    if !done && !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf);
        handle_sse_line(line.trim_end_matches(['\r', '\n']), &mut acc, &mut on_delta)?;
    }

    let (reasoning, content) = build_reply_text(
        acc.content,
        (!acc.reasoning.is_empty()).then_some(acc.reasoning),
    );
    Ok(fold_reply(reasoning, content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wire(m: &ChatMessage) -> serde_json::Value {
        serde_json::to_value(to_wire(m)).unwrap()
    }

    // --- SSE ---

    #[test]
    fn sse_data_line_reads_the_payload() {
        assert_eq!(
            sse_data_line(r#"data: {"choices":[]}"#),
            Some(r#"{"choices":[]}"#)
        );
        assert_eq!(sse_data_line("data: [DONE]"), Some("[DONE]"));
    }

    #[test]
    fn sse_data_line_ignores_non_data_lines() {
        for l in [
            "",
            "data:",
            "data: ",
            ": keep-alive",
            "event: message",
            "id: 42",
        ] {
            assert_eq!(sse_data_line(l), None, "{l:?}");
        }
    }

    fn run_sse(lines: &[&str]) -> (anyhow::Result<SseStep>, StreamAcc, Vec<String>) {
        let mut acc = StreamAcc::default();
        let mut seen = Vec::new();
        let mut cb = |d: ChatDelta<'_>| match d {
            ChatDelta::Content(t) => seen.push(format!("c:{t}")),
            ChatDelta::Reasoning(t) => seen.push(format!("r:{t}")),
        };
        let mut last = Ok(SseStep::Continue);
        for l in lines {
            last = handle_sse_line(l, &mut acc, &mut cb);
            if last.is_err() || last.as_ref().is_ok_and(|s| *s == SseStep::Done) {
                break;
            }
        }
        (last, acc, seen)
    }

    #[test]
    fn sse_accumulates_content_and_reasoning() {
        let (r, acc, seen) = run_sse(&[
            r#"data: {"choices":[{"delta":{"reasoning_content":"hm"}}]}"#,
            r#"data: {"choices":[{"delta":{"reasoning":"!"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0}]},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ]);
        assert_eq!(r.unwrap(), SseStep::Done);
        assert_eq!(acc.content, "hi");
        assert_eq!(acc.reasoning, "hm!");
        assert_eq!(seen, ["r:hm", "r:!", "c:hi"]);
    }

    #[test]
    fn sse_error_frame_is_an_error() {
        let (r, _, _) = run_sse(&[r#"data: {"error":{"message":"out of memory"}}"#]);
        assert!(r.unwrap_err().to_string().contains("out of memory"));
        let (r, _, _) = run_sse(&[r#"data: {"error":"boom"}"#]);
        assert!(r.unwrap_err().to_string().contains("boom"));
    }

    #[test]
    fn sse_final_line_without_newline_is_still_handled() {
        // The streaming loop flushes the leftover buffer through the same fn.
        let (r, acc, _) = run_sse(&[r#"data: {"choices":[{"delta":{"content":"tail"}}]}"#]);
        assert_eq!(r.unwrap(), SseStep::Continue);
        assert_eq!(acc.content, "tail");
    }

    #[test]
    fn sse_skips_unparsable_chunk() {
        let (r, acc, _) = run_sse(&["data: {not json"]);
        assert_eq!(r.unwrap(), SseStep::Continue);
        assert!(acc.content.is_empty());
    }

    // --- misc ---

    #[test]
    fn message_ids() {
        let a = new_message_id();
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, new_message_id());
        let m = ChatMessage::text("user", "x").with_id();
        assert_eq!(m.id.len(), 16);
        assert_eq!(m.clone().with_id().id, m.id);
    }

    // --- wire ---

    #[test]
    fn wire_plain_message_has_no_extra_fields() {
        let mut m = ChatMessage::text("user", "hi").with_id();
        m.thinking = Some("t".into());
        m.generated_images = vec!["/x.png".into()];
        assert_eq!(wire(&m), json!({"role": "user", "content": "hi"}));
    }

    #[test]
    fn wire_assistant_tool_calls() {
        let mut m = ChatMessage::text("assistant", "");
        m.tool_calls = vec![ToolCall {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"a"}"#.into(),
        }];
        assert_eq!(
            wire(&m),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"},
                }],
            })
        );
    }

    #[test]
    fn wire_tool_message_carries_tool_call_id() {
        let mut m = ChatMessage::text("tool", "ok");
        m.tool_call_id = Some("call_1".into());
        assert_eq!(
            wire(&m),
            json!({"role": "tool", "content": "ok", "tool_call_id": "call_1"})
        );
    }

    #[test]
    fn wire_images_use_parts() {
        let mut m = ChatMessage::text("user", "look");
        m.images = vec!["data:image/png;base64,AA".into()];
        let v = wire(&m);
        assert_eq!(v["content"][0], json!({"type": "text", "text": "look"}));
        assert_eq!(v["content"][1]["type"], "image_url");
    }

    #[test]
    fn request_with_tools_and_format() {
        let tools = [ToolSpec {
            name: "t".into(),
            description: "d".into(),
            parameters: json!({"type": "object"}),
        }];
        let opts = ChatOptions {
            temperature: 0.5,
            max_tokens: Some(64),
            tools: Some(&tools),
            response_format: Some(json!({"type": "json_object"})),
            reasoning_effort: None,
        };
        let msgs = [ChatMessage::text("user", "hi")];
        let v = serde_json::to_value(ChatRequest::new("m", &msgs, &opts, false)).unwrap();
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "t");
        assert_eq!(v["tool_choice"], "auto");
        assert_eq!(v["parallel_tool_calls"], false);
        assert_eq!(v["max_tokens"], 64);
        assert_eq!(v["response_format"]["type"], "json_object");

        let plain =
            serde_json::to_value(ChatRequest::new("m", &msgs, &Default::default(), true)).unwrap();
        for k in [
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "max_tokens",
            "response_format",
        ] {
            assert!(plain.get(k).is_none(), "{k}");
        }
        assert_eq!(plain["stream"], true);
    }

    // --- response parsing ---

    fn resp(message: serde_json::Value, finish: &str) -> String {
        json!({"choices": [{"message": message, "finish_reason": finish}]}).to_string()
    }

    #[test]
    fn parse_plain_reply() {
        let r = parse_response(&resp(json!({"content": "hello"}), "stop")).unwrap();
        assert_eq!(r.content, "hello");
        assert_eq!(r.finish_reason.as_deref(), Some("stop"));
        assert!(r.reasoning.is_none() && r.tool_calls.is_empty());
    }

    #[test]
    fn parse_null_content() {
        let r = parse_response(&resp(json!({"content": null}), "length")).unwrap();
        assert_eq!(r.content, "");
        assert_eq!(r.finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn parse_tool_calls_string_and_object_arguments() {
        let r = parse_response(&resp(
            json!({"content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "a", "arguments": "{\"x\":1}"}},
                {"function": {"name": "b", "arguments": {"y": 2}}},
                {"id": "", "function": {"name": "c"}},
            ]}),
            "tool_calls",
        ))
        .unwrap();
        assert_eq!(r.tool_calls.len(), 3);
        assert_eq!(r.tool_calls[0].id, "c1");
        assert_eq!(r.tool_calls[0].arguments, r#"{"x":1}"#);
        assert_eq!(r.tool_calls[1].arguments, r#"{"y":2}"#);
        assert!(r.tool_calls[1].id.starts_with("call_"));
        assert_eq!(r.tool_calls[2].arguments, "{}");
        assert!(!r.tool_calls[2].id.is_empty());
    }

    #[test]
    fn parse_reasoning_fields() {
        let r = parse_response(&resp(
            json!({"content": "4", "reasoning_content": "2+2"}),
            "stop",
        ))
        .unwrap();
        assert_eq!(
            (r.reasoning.as_deref(), r.content.as_str()),
            (Some("2+2"), "4")
        );
        let r = parse_response(&resp(json!({"content": "4", "reasoning": "2+2"}), "stop")).unwrap();
        assert_eq!(r.reasoning.as_deref(), Some("2+2"));
        // Both fields at once must not be a duplicate-field error.
        let r = parse_response(&resp(
            json!({"content": "4", "reasoning": "a", "reasoning_content": "b"}),
            "stop",
        ))
        .unwrap();
        assert_eq!(r.reasoning.as_deref(), Some("b"));
    }

    #[test]
    fn parse_inline_think_is_split_out() {
        let r = parse_response(&resp(
            json!({"content": "<think>why</think>\nanswer"}),
            "stop",
        ))
        .unwrap();
        assert_eq!(r.reasoning.as_deref(), Some("why"));
        assert_eq!(r.content, "answer");
        assert_eq!(
            fold_reply(r.reasoning, r.content),
            "<think>why</think>answer"
        );
    }

    #[test]
    fn parse_empty_choices_is_an_error() {
        let e = parse_response(r#"{"choices":[]}"#).unwrap_err();
        assert!(e.to_string().contains("no choices"));
        let e = parse_response(r#"{"error":{"message":"model not loaded"}}"#).unwrap_err();
        assert!(e.to_string().contains("model not loaded"));
    }

    // --- split_thinking ---

    #[test]
    fn split_thinking_paired() {
        assert_eq!(
            split_thinking("<think>a</think>b"),
            (Some("a".into()), "b".into())
        );
        assert_eq!(
            split_thinking("<THINKING> a </THINKING> b"),
            (Some("a".into()), "b".into())
        );
    }

    #[test]
    fn split_thinking_multiple() {
        assert_eq!(
            split_thinking("<think>a</think>x<think>b</think>y"),
            (Some("a\n\nb".into()), "xy".into())
        );
    }

    #[test]
    fn split_thinking_lone_closing_tag() {
        assert_eq!(
            split_thinking("reasoning here</think>\n\nthe answer"),
            (Some("reasoning here".into()), "the answer".into())
        );
    }

    #[test]
    fn split_thinking_unclosed() {
        assert_eq!(
            split_thinking("answer<think>cut off"),
            (Some("cut off".into()), "answer".into())
        );
    }

    #[test]
    fn split_thinking_none() {
        assert_eq!(split_thinking("  plain "), (None, "plain".into()));
        assert_eq!(split_thinking("<think></think>x"), (None, "x".into()));
    }

    // --- JSON extraction ---

    #[test]
    fn json_plain_and_non_objects() {
        assert_eq!(extract_json_object(r#" {"a":1} "#), Some(json!({"a": 1})));
        assert_eq!(extract_json_object("[1,2]"), None);
        assert_eq!(extract_json_object("42"), None);
        assert_eq!(extract_json_object("no json"), None);
    }

    #[test]
    fn json_fenced() {
        assert_eq!(
            extract_json_object("Here:\n```json\n{\"a\": [1, 2]}\n```\nbye"),
            Some(json!({"a": [1, 2]}))
        );
        assert_eq!(
            extract_json_object("```\n{\"a\":1}\n```"),
            Some(json!({"a": 1}))
        );
    }

    #[test]
    fn json_with_prose_and_braces_in_strings() {
        assert_eq!(
            extract_json_object(r#"Sure! {"a": "} { \" }", "b": {"c": 1}} done {x}"#),
            Some(json!({"a": "} { \" }", "b": {"c": 1}}))
        );
    }

    #[test]
    fn json_after_thinking() {
        assert_eq!(
            extract_json_object(r#"<think>maybe {"a": 0}</think>{"a": 1}"#),
            Some(json!({"a": 1}))
        );
        assert_eq!(
            extract_json_object("thinking it over {</think>{\"a\": 1}"),
            Some(json!({"a": 1}))
        );
    }

    // --- error classification ---

    fn http(status: u16, body: &str) -> anyhow::Error {
        HttpError {
            status,
            body: body.into(),
        }
        .into()
    }

    #[test]
    fn tools_unsupported_detection() {
        assert!(is_tools_unsupported(&http(
            400,
            r#"{"error":"registry.ollama.ai/x does not support tools"}"#
        )));
        assert!(is_tools_unsupported(&http(
            500,
            "Tool calling not supported"
        )));
        assert!(!is_tools_unsupported(&http(400, "context length exceeded")));
        assert!(!is_tools_unsupported(&http(401, "bad tool key")));
        assert!(!is_tools_unsupported(&anyhow::anyhow!("tool")));
    }

    #[test]
    fn response_format_unsupported_detection() {
        assert!(is_response_format_unsupported(&http(
            400,
            "'response_format.type' must be 'json_schema' or 'text'"
        )));
        assert!(is_response_format_unsupported(&http(
            500,
            "failed to parse grammar"
        )));
        assert!(!is_response_format_unsupported(&http(400, "bad request")));
    }

    #[test]
    fn http_error_display_and_downcast() {
        let e = http(404, "nope");
        assert_eq!(e.downcast_ref::<HttpError>().unwrap().status, 404);
        assert_eq!(e.to_string(), "LLM endpoint returned 404 Not Found: nope");
    }

    #[test]
    fn json_level_fallbacks() {
        let schema = json!({"type": "object"});
        assert_eq!(
            response_format_for(0, "n", &schema).unwrap()["type"],
            "json_schema"
        );
        assert_eq!(
            response_format_for(1, "n", &schema).unwrap()["type"],
            "json_object"
        );
        assert!(response_format_for(2, "n", &schema).is_none());
    }

    fn completion(content: &str, finish: &str) -> String {
        serde_json::json!({"choices": [{"message": {"role": "assistant", "content": content},
            "finish_reason": finish}]})
        .to_string()
    }

    fn schema() -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"answer": {"type": "string"}}})
    }

    #[tokio::test]
    async fn json_calls_turn_reasoning_off() {
        let (host, seen) = crate::gpu::tests::fake_server(|_, _, _| {
            (200, completion("{\"answer\": \"yes\"}", "stop"))
        });
        let msgs = [ChatMessage::text("user", "q")];
        let v = send_chat_json(
            &format!("{host}/v1/chat/completions"),
            "m-effort",
            "",
            &msgs,
            0.0,
            Some(24),
            "a",
            &schema(),
        )
        .await
        .unwrap();
        assert_eq!(v["answer"], "yes");
        let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0].2).unwrap();
        assert_eq!(body["reasoning_effort"], "none");
    }

    #[tokio::test]
    async fn a_server_rejecting_reasoning_effort_is_retried_without_it() {
        let (host, seen) = crate::gpu::tests::fake_server(|_, _, body| {
            if body.contains("reasoning_effort") {
                (
                    400,
                    r#"{"error": "unknown parameter reasoning_effort"}"#.to_string(),
                )
            } else {
                (200, completion("{\"answer\": \"no\"}", "stop"))
            }
        });
        let msgs = [ChatMessage::text("user", "q")];
        let endpoint = format!("{host}/v1/chat/completions");
        let v = send_chat_json(
            &endpoint,
            "m-reject",
            "",
            &msgs,
            0.0,
            Some(24),
            "a",
            &schema(),
        )
        .await
        .unwrap();
        assert_eq!(v["answer"], "no");
        // Remembered: the next call doesn't try it again.
        send_chat_json(
            &endpoint,
            "m-reject",
            "",
            &msgs,
            0.0,
            Some(24),
            "a",
            &schema(),
        )
        .await
        .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(!seen[2].2.contains("reasoning_effort"));
    }

    #[test]
    fn embedding_responses_are_ordered_by_index_and_counted() {
        let body = r#"{"data":[{"index":1,"embedding":[2.0]},{"index":0,"embedding":[1.0]}]}"#;
        assert_eq!(
            parse_embeddings(body, 2).unwrap(),
            vec![vec![1.0], vec![2.0]]
        );
        assert!(parse_embeddings(body, 3).is_err());
        assert!(parse_embeddings(r#"{"data":[{"embedding":[]}]}"#, 1).is_err());
        assert!(parse_embeddings("{}", 1).is_err());
    }

    #[tokio::test]
    async fn an_answer_cut_off_by_the_cap_is_retried_uncapped() {
        let (host, seen) = crate::gpu::tests::fake_server(|_, _, body| {
            if body.contains("max_tokens") {
                (200, completion("", "length"))
            } else {
                (200, completion("{\"answer\": \"yes\"}", "stop"))
            }
        });
        let msgs = [ChatMessage::text("user", "q")];
        let v = send_chat_json(
            &format!("{host}/v1/chat/completions"),
            "m-cap",
            "",
            &msgs,
            0.0,
            Some(10),
            "a",
            &schema(),
        )
        .await
        .unwrap();
        assert_eq!(v["answer"], "yes");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }
}
