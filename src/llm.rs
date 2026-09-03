use serde::{Deserialize, Serialize};

/// `content` stays a plain string everywhere in the app's own logic --
/// condensing, `rules::extract_command`/`strip_command_fences`, context
/// estimation, chat-mode's state-block parsing, tests -- none of that
/// changes or needs to know images exist. `images` (base64 data URLs, e.g.
/// `data:image/png;base64,...`) is the one addition, and it only matters at
/// the wire boundary in `send_chat` below, which builds the OpenAI vision
/// content-array shape for a message that has any. `#[serde(default,
/// skip_serializing_if)]` keeps every already-persisted `history.json` and
/// every existing caller's plain-string construction unaffected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// Local file paths (not base64 -- keeps `history.json` small) of
    /// ComfyUI-generated images attached to this (always `assistant`)
    /// message. Deliberately a separate field from `images` above, not a
    /// reuse of it: `images` holds attachments *sent to* the model as
    /// vision input, and `to_wire` below only ever reads that field --
    /// `generated_images` must never be resent as input on a later turn the
    /// way a user's own attachment would be.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generated_images: Vec<String>,
    /// Chat mode only, and only when `AppConfig.chat_persist_thinking` is on
    /// -- the model's own reasoning for this reply, kept purely for display
    /// (a completed session's "Thinking" disclosure survives reopening it).
    /// Never read by `to_wire` below, so it's never re-sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

impl ChatMessage {
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        ChatMessage {
            role: role.into(),
            content: content.into(),
            images: Vec::new(),
            generated_images: Vec::new(),
            thinking: None,
        }
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    temperature: f32,
    stream: bool,
}

/// One content part in the OpenAI vision shape:
/// `{"type": "text", "text": "..."}` or
/// `{"type": "image_url", "image_url": {"url": "data:..."}}`.
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

/// Plain text for every message that has no images (the overwhelming
/// majority -- all of operation mode, all of chat mode without an
/// attachment), the multi-part array only for the ones that do.
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
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

/// The reply is always plain text -- this app never receives an image back
/// from the chat endpoint (that's the separate, not-yet-built ComfyUI
/// feature) -- so the response side stays a plain string, unlike the
/// request side above.
#[derive(Deserialize)]
struct ResponseMessage {
    content: String,
    /// Some OpenAI-compatible reasoning-model servers (vLLM, llama.cpp
    /// server's `--reasoning-format`, LM Studio's reasoning models, etc.)
    /// return the model's reasoning in this separate field instead of an
    /// inline `<think>...</think>` tag inside `content` -- previously
    /// dropped entirely here, since only `content` was ever read, which
    /// silently discarded any real reasoning a backend like that produced
    /// regardless of `chat_show_thinking`. Folded into `content` by
    /// `fold_reasoning_into_content` below rather than plumbed through as
    /// its own field, so `rules::extract_thinking_block`/
    /// `strip_thinking_blocks` (which only ever look for the tag) keep
    /// working unchanged no matter which shape the backend actually used.
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// Normalizes a `reasoning_content`-shaped response into the `<think>` tag
/// convention the rest of the app already understands -- see
/// `ResponseMessage::reasoning_content`'s doc comment. Only synthesizes a
/// tag when `content` doesn't already have one of its own, in case a
/// backend somehow sends both.
fn fold_reasoning_into_content(content: String, reasoning_content: Option<String>) -> String {
    match reasoning_content {
        Some(reasoning) if !reasoning.trim().is_empty() && !content.contains("<think") => {
            format!("<think>{reasoning}</think>{content}")
        }
        _ => content,
    }
}

/// Talks to any OpenAI-compatible `/chat/completions` endpoint -- this is
/// what both Ollama and LM Studio expose locally, so no vendor SDK needed.
/// `api_key`, when non-empty, is sent as a standard `Authorization: Bearer`
/// header (LM Studio can be configured to require one).
pub async fn send_chat(
    endpoint: &str,
    model: &str,
    api_key: &str,
    temperature: f32,
    messages: &[ChatMessage],
) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let req = ChatRequest {
        model,
        messages: messages.iter().map(to_wire).collect(),
        temperature,
        stream: false,
    };
    let mut builder = client.post(endpoint).json(&req);
    if !api_key.trim().is_empty() {
        builder = builder.bearer_auth(api_key);
    }

    let resp = builder.send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("LLM endpoint returned {status}: {body}");
    }

    let parsed: ChatResponse = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("failed to parse LLM response ({e}): {body}"))?;
    Ok(parsed
        .choices
        .into_iter()
        .next()
        .map(|c| fold_reasoning_into_content(c.message.content, c.message.reasoning_content))
        .unwrap_or_default())
}

/// One chunk's worth of a streaming reply -- either regular reply text or,
/// for a backend that uses the separate `reasoning_content` delta field
/// (see `ResponseMessage::reasoning_content`'s doc comment), its reasoning.
/// A backend that instead embeds an inline `<think>...</think>` tag in its
/// regular content stream has no equivalent live signal here -- that tag
/// only gets extracted/hidden once `send_chat_streaming` returns and the
/// caller runs the same whole-string extraction `send_chat` already
/// relies on. Known, accepted limitation, not solved by this type.
pub enum ChatDelta<'a> {
    Content(&'a str),
    Reasoning(&'a str),
}

#[derive(Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
}

#[derive(Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// Reads one already-decoded, newline-stripped SSE line and returns its
/// payload if it's a real `data:` line worth parsing -- `None` for a blank
/// line, a `:`-prefixed keep-alive comment, or any other SSE field this app
/// doesn't use (`event:`, `id:`, `retry:`). Pulled out as its own pure
/// function specifically so the framing logic (not the JSON payload it
/// carries) has real unit test coverage, independent of a live network
/// stream.
fn sse_data_line(line: &str) -> Option<&str> {
    let payload = line.strip_prefix("data:")?.trim();
    (!payload.is_empty()).then_some(payload)
}

/// Streaming sibling of `send_chat` -- same request shape (`stream: true`
/// instead of `false`), same eventual return contract (a fully folded
/// string via the same `fold_reasoning_into_content` `send_chat` already
/// uses, so callers built around `send_chat`'s return value don't need to
/// change), but calls `on_delta` once per parsed SSE chunk as it arrives so
/// a caller can forward partial text live. `on_delta` is a plain sync
/// callback, not async -- both real callers (a Tauri `Channel::send`, an
/// axum `mpsc::UnboundedSender::send`) are themselves synchronous, and
/// `+ Send` since both run this inside a spawned task.
pub async fn send_chat_streaming(
    endpoint: &str,
    model: &str,
    api_key: &str,
    temperature: f32,
    messages: &[ChatMessage],
    mut on_delta: impl FnMut(ChatDelta<'_>) + Send,
) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let req = ChatRequest {
        model,
        messages: messages.iter().map(to_wire).collect(),
        temperature,
        stream: true,
    };
    let mut builder = client.post(endpoint).json(&req);
    if !api_key.trim().is_empty() {
        builder = builder.bearer_auth(api_key);
    }

    let resp = builder.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("LLM endpoint returned {status}: {body}");
    }

    let mut content_acc = String::new();
    let mut reasoning_acc = String::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut byte_stream = resp.bytes_stream();

    // Buffers raw bytes (not text) across polls, decoding only once a
    // complete `\n`-terminated line is assembled -- a `String::from_utf8`
    // per network chunk would risk mangling a multi-byte UTF-8 character
    // split across two chunks. Assumes one `data:` line per SSE event, true
    // for every backend this app targets (OpenAI/vLLM/Ollama/llama.cpp/LM
    // Studio) -- real SSE allows multi-line events, deliberately not
    // handled, since none of those actually send one.
    use tokio_stream::StreamExt as _;
    'outer: while let Some(chunk) = byte_stream.next().await {
        buf.extend_from_slice(&chunk?);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches(['\r', '\n']);
            let Some(payload) = sse_data_line(line) else {
                continue;
            };
            if payload == "[DONE]" {
                break 'outer;
            }
            let parsed: StreamChunk = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("send_chat_streaming: skipping unparsable chunk ({e}): {payload}");
                    continue;
                }
            };
            for choice in parsed.choices {
                if let Some(piece) = choice.delta.content.filter(|s| !s.is_empty()) {
                    on_delta(ChatDelta::Content(&piece));
                    content_acc.push_str(&piece);
                }
                if let Some(piece) = choice.delta.reasoning_content.filter(|s| !s.is_empty()) {
                    on_delta(ChatDelta::Reasoning(&piece));
                    reasoning_acc.push_str(&piece);
                }
            }
        }
    }

    Ok(fold_reasoning_into_content(
        content_acc,
        (!reasoning_acc.is_empty()).then_some(reasoning_acc),
    ))
}

// --- Vision capability probes (best-effort, backend-specific hints only) ---
//
// Neither of these is authoritative -- they're each one specific backend's
// own extension, not something the generic `/v1/chat/completions` endpoint
// this app is built around exposes. A miss just means "couldn't tell,"
// never "definitely no vision" -- a completely different OpenAI-compatible
// server won't have either endpoint at all. `test_vision_support` (in
// `main.rs`) sending a real test image is the only actually authoritative
// check; these are just a hint for whether to bother showing the
// image-attach control by default.

/// Best-effort: strips a trailing `/v1/...` (or any path) off `endpoint` to
/// get at the bare host these backend-specific APIs live under. Returns
/// `None` if `endpoint` doesn't even parse as a URL.
fn base_host(endpoint: &str) -> Option<String> {
    let url = url::Url::parse(endpoint).ok()?;
    Some(format!(
        "{}://{}",
        url.scheme(),
        url.host_str().map(|h| match url.port() {
            Some(p) => format!("{h}:{p}"),
            None => h.to_string(),
        })?
    ))
}

/// `POST {host}/api/show {"model": "<name>"}` -> `{"capabilities": [...]}`.
/// Source: <https://github.com/ollama/ollama/blob/main/docs/api.md>.
pub async fn probe_ollama_vision(endpoint: &str, model: &str) -> Option<bool> {
    let host = base_host(endpoint)?;
    #[derive(Deserialize)]
    struct ShowResponse {
        #[serde(default)]
        capabilities: Vec<String>,
    }
    let resp = reqwest::Client::new()
        .post(format!("{host}/api/show"))
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let parsed: ShowResponse = resp.json().await.ok()?;
    Some(parsed.capabilities.iter().any(|c| c == "vision"))
}

/// `GET {host}/api/v0/models` -> `{"data": [{"id": ..., "type": "vlm"|"llm"|...}]}`.
/// LM Studio's own extended REST API, not the plain OpenAI-compatible one.
/// Source: <https://lmstudio.ai/docs/developer/rest/endpoints>.
pub async fn probe_lmstudio_vision(endpoint: &str, model: &str) -> Option<bool> {
    let host = base_host(endpoint)?;
    #[derive(Deserialize)]
    struct ModelsResponse {
        data: Vec<ModelEntry>,
    }
    #[derive(Deserialize)]
    struct ModelEntry {
        id: String,
        #[serde(rename = "type", default)]
        kind: String,
    }
    let resp = reqwest::Client::new()
        .get(format!("{host}/api/v0/models"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let parsed: ModelsResponse = resp.json().await.ok()?;
    parsed
        .data
        .iter()
        .find(|m| m.id == model)
        .map(|m| m.kind == "vlm")
}

/// Tries both known backend extensions, first one that actually answers
/// wins. `None` means neither backend was present at all -- a third-party
/// OpenAI-compatible server, most likely -- not "no vision."
pub async fn probe_vision_capability(endpoint: &str, model: &str) -> Option<bool> {
    if let Some(v) = probe_ollama_vision(endpoint, model).await {
        return Some(v);
    }
    probe_lmstudio_vision(endpoint, model).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_data_line_reads_the_payload() {
        assert_eq!(
            sse_data_line(r#"data: {"choices":[]}"#),
            Some(r#"{"choices":[]}"#)
        );
    }

    #[test]
    fn sse_data_line_recognizes_the_done_sentinel() {
        assert_eq!(sse_data_line("data: [DONE]"), Some("[DONE]"));
    }

    #[test]
    fn sse_data_line_is_none_for_a_blank_line() {
        assert_eq!(sse_data_line(""), None);
    }

    #[test]
    fn sse_data_line_is_none_for_an_empty_data_field() {
        assert_eq!(sse_data_line("data:"), None);
        assert_eq!(sse_data_line("data: "), None);
    }

    #[test]
    fn sse_data_line_is_none_for_a_keep_alive_comment() {
        assert_eq!(sse_data_line(": keep-alive"), None);
    }

    #[test]
    fn sse_data_line_is_none_for_an_unused_sse_field() {
        assert_eq!(sse_data_line("event: message"), None);
        assert_eq!(sse_data_line("id: 42"), None);
    }

    #[test]
    fn base_host_strips_the_path() {
        assert_eq!(
            base_host("http://localhost:1234/v1/chat/completions").as_deref(),
            Some("http://localhost:1234")
        );
    }

    #[test]
    fn base_host_handles_a_default_port() {
        assert_eq!(
            base_host("https://example.com/v1/chat/completions").as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn base_host_is_none_for_garbage() {
        assert_eq!(base_host("not a url"), None);
    }

    #[test]
    fn fold_reasoning_into_content_wraps_it_as_a_think_tag() {
        assert_eq!(
            fold_reasoning_into_content("The answer is 4.".to_string(), Some("2+2=4".to_string())),
            "<think>2+2=4</think>The answer is 4."
        );
    }

    #[test]
    fn fold_reasoning_into_content_is_a_no_op_without_reasoning() {
        assert_eq!(
            fold_reasoning_into_content("just an answer".to_string(), None),
            "just an answer"
        );
    }

    #[test]
    fn fold_reasoning_into_content_ignores_a_blank_reasoning_field() {
        assert_eq!(
            fold_reasoning_into_content("just an answer".to_string(), Some("   ".to_string())),
            "just an answer"
        );
    }

    #[test]
    fn fold_reasoning_into_content_never_double_wraps_an_existing_tag() {
        let content = "<think>already reasoned</think>done".to_string();
        assert_eq!(
            fold_reasoning_into_content(content.clone(), Some("more reasoning".to_string())),
            content
        );
    }
}
