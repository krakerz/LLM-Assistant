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
pub async fn send_chat(
    endpoint: &str,
    model: &str,
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
    let resp = client
        .post(endpoint)
        .json(&req)
        .send()
        .await?
        .error_for_status()?;
    let parsed: ChatResponse = resp.json().await?;
    Ok(parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .unwrap_or_default())
}
