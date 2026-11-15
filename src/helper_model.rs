//! Mechanical chat calls on the optional helper model; any failure is retried
//! once on the chat model, so chat never breaks.

use crate::config::{AppConfig, ModelTarget};
use crate::gpu;
use crate::llm::{self, ChatMessage};
use serde_json::Value;
use std::future::Future;
use std::time::Duration;

/// Measured 1-3 s per call; the margin covers a first-call model load.
const HELPER_TIMEOUT: Duration = Duration::from_secs(45);

/// `llm::send_chat_json` on the helper (when set), falling back to the chat model.
pub async fn send_json(
    cfg: &AppConfig,
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: Option<u32>,
    schema_name: &str,
    schema: &Value,
) -> anyhow::Result<Value> {
    if let Some(h) = cfg.helper() {
        let call = llm::send_chat_json(
            h.endpoint,
            h.model,
            h.api_key,
            messages,
            temperature,
            max_tokens,
            schema_name,
            schema,
        );
        let r = on_helper(cfg, h, call).await;
        if !json_needs_fallback(&r) {
            return r;
        }
        let reason = match &r {
            Err(e) => e.to_string(),
            Ok(Value::Object(_)) => "empty JSON object".into(),
            Ok(_) => "not a JSON object".into(),
        };
        log::warn!(
            "helper model {} failed ({reason}); retrying on the chat model",
            h.model
        );
    }
    let main = cfg.main_target();
    llm::send_chat_json(
        main.endpoint,
        main.model,
        main.api_key,
        messages,
        temperature,
        max_tokens,
        schema_name,
        schema,
    )
    .await
}

/// `llm::send_chat` on the helper (when set), falling back to the chat model.
pub async fn send_text(
    cfg: &AppConfig,
    temperature: f32,
    messages: &[ChatMessage],
) -> anyhow::Result<String> {
    if let Some(h) = cfg.helper() {
        let call = llm::send_chat(h.endpoint, h.model, h.api_key, temperature, messages);
        let r = on_helper(cfg, h, call).await;
        if !text_needs_fallback(&r) {
            return r;
        }
        let reason = match &r {
            Err(e) => e.to_string(),
            Ok(_) => "empty reply".into(),
        };
        log::warn!(
            "helper model {} failed ({reason}); retrying on the chat model",
            h.model
        );
    }
    let main = cfg.main_target();
    llm::send_chat(
        main.endpoint,
        main.model,
        main.api_key,
        temperature,
        messages,
    )
    .await
}

/// Runs `call` under the helper timeout; it only takes the GPU gate when the
/// helper's machine needs it (see `gpu::needs_gate`).
async fn on_helper<T>(
    cfg: &AppConfig,
    helper: ModelTarget<'_>,
    call: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let run = async {
        if gpu::needs_gate(helper.endpoint, &cfg.endpoint) {
            call.await
        } else {
            gpu::ungated(call).await
        }
    };
    match tokio::time::timeout(HELPER_TIMEOUT, run).await {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!(
            "timed out after {} s",
            HELPER_TIMEOUT.as_secs()
        )),
    }
}

fn json_needs_fallback(r: &anyhow::Result<Value>) -> bool {
    match r {
        Ok(Value::Object(m)) => m.is_empty(),
        _ => true,
    }
}

fn text_needs_fallback(r: &anyhow::Result<String>) -> bool {
    r.as_ref().map_or(true, |t| t.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn completion(content: &str) -> String {
        json!({"choices": [{"message": {"role": "assistant", "content": content},
            "finish_reason": "stop"}]})
        .to_string()
    }

    fn cfg_for(main: &str, helper: &str) -> AppConfig {
        AppConfig {
            endpoint: format!("{main}/v1/chat/completions"),
            model: "main".into(),
            helper_endpoint: format!("{helper}/v1/chat/completions"),
            helper_model: "small".into(),
            ..Default::default()
        }
    }

    fn schema() -> Value {
        json!({"type": "object", "properties": {"answer": {"type": "string"}}})
    }

    #[test]
    fn json_fallback_rules() {
        assert!(json_needs_fallback(&Err(anyhow::anyhow!("x"))));
        assert!(json_needs_fallback(&Ok(json!({}))));
        assert!(json_needs_fallback(&Ok(json!([]))));
        assert!(json_needs_fallback(&Ok(json!("x"))));
        assert!(!json_needs_fallback(&Ok(json!({"a": 1}))));
    }

    #[test]
    fn text_fallback_rules() {
        assert!(text_needs_fallback(&Err(anyhow::anyhow!("x"))));
        assert!(text_needs_fallback(&Ok("  ".into())));
        assert!(!text_needs_fallback(&Ok("ok".into())));
    }

    #[tokio::test]
    async fn a_failing_helper_falls_back_to_the_chat_model() {
        let (helper, _) = gpu::tests::fake_server(|_, _, _| (500, "{}".into()));
        let (main, main_seen) =
            gpu::tests::fake_server(|_, _, _| (200, completion("{\"answer\": \"main\"}")));
        let cfg = cfg_for(&main, &helper);
        let msgs = [ChatMessage::text("user", "q")];
        let v = send_json(&cfg, &msgs, 0.0, None, "a", &schema())
            .await
            .unwrap();
        assert_eq!(v["answer"], "main");
        assert!(!main_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_working_helper_leaves_the_chat_model_alone() {
        let (helper, _) =
            gpu::tests::fake_server(|_, _, _| (200, completion("{\"answer\": \"yes\"}")));
        let (main, main_seen) = gpu::tests::fake_server(|_, _, _| (500, "{}".into()));
        let cfg = cfg_for(&main, &helper);
        let msgs = [ChatMessage::text("user", "q")];
        let v = send_json(&cfg, &msgs, 0.0, None, "a", &schema())
            .await
            .unwrap();
        assert_eq!(v["answer"], "yes");
        assert!(main_seen.lock().unwrap().is_empty());
    }
}
