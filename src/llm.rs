use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    stream: bool,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChatMessage,
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
        messages,
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
