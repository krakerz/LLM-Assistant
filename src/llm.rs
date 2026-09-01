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
        .map(|c| c.message.content)
        .unwrap_or_default())
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
}
