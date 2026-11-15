//! Sharing one GPU between the chat model and ComfyUI. Two opt-in switches
//! (`ComfyUiConfig::unload_llm_during_generation` / `free_after_generation`)
//! drive `with_image_slot`, the one wrapper every image generation goes
//! through: unload the configured chat model, generate, free ComfyUI's
//! memory, reload the model with the settings it had.
//!
//! The "GPU gate" is what keeps this honest. Every LLM request holds a read
//! permit for its whole duration; an image swap takes the write side, so it
//! waits for in-flight calls and any later call (a background state summary,
//! say) waits until the swap is done instead of reloading the model
//! mid-generation. Nothing may call the LLM while holding `exclusive()`.

use crate::comfyui::{self, ComfyUiConfig};
use crate::config::{AppConfig, ModelTarget};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Loading a ~10 GB model can take a minute; unloading is fast but shares the cap.
const SWAP_TIMEOUT: Duration = Duration::from_secs(180);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);

static GATE: RwLock<()> = RwLock::const_new(());

/// Hosts (`base_host`) whose helper/embedding model is unloaded for the
/// running image generation; requests to them must queue on the gate.
static PARKED: Mutex<Vec<String>> = Mutex::new(Vec::new());

tokio::task_local! {
    /// Set by `ungated`: requests to a model on another machine than the
    /// chat model skip the gate (an image swap never touches their GPU).
    static UNGATED: ();
}

/// Held by every LLM request for its whole duration, stream included.
/// `None` inside `ungated`.
pub async fn llm_permit() -> Option<RwLockReadGuard<'static, ()>> {
    if UNGATED.try_with(|_| ()).is_ok() {
        return None;
    }
    if let Ok(guard) = GATE.try_read() {
        return Some(guard);
    }
    log::debug!("gpu: LLM request waiting for an image generation to finish");
    Some(GATE.read().await)
}

/// Runs `fut` with every LLM request in it skipping the gate.
pub async fn ungated<F: Future>(fut: F) -> F::Output {
    UNGATED.scope((), fut).await
}

/// Held by an image generation that unloads the chat model. Waits for
/// in-flight LLM requests.
pub async fn exclusive() -> RwLockWriteGuard<'static, ()> {
    if let Ok(guard) = GATE.try_write() {
        return guard;
    }
    log::debug!("gpu: image generation waiting for in-flight LLM requests");
    GATE.write().await
}

/// One loaded LM Studio instance, as it was before we unloaded it.
#[derive(Debug, Clone, PartialEq)]
struct Instance {
    key: String,
    id: String,
    /// `loaded_instances[].config` verbatim (context_length, eval_batch_size,
    /// flash_attention, num_experts, offload_kv_cache_to_gpu, ...).
    config: serde_json::Map<String, Value>,
}

/// What `unload_model` took off the GPU, so `reload_model` can put
/// it back. Empty for Ollama (reloads itself) and unknown servers.
#[derive(Debug, Default)]
pub struct Unloaded {
    instances: Vec<Instance>,
}

/// `scheme://host:port` of the chat endpoint (`.../v1/chat/completions` etc.
/// stripped), where LM Studio's and Ollama's management APIs live.
pub(crate) fn base_host(endpoint: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(endpoint.trim()).ok()?;
    url.host_str()?;
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Some(url.as_str().trim_end_matches('/').to_string())
}

/// Whether requests to `endpoint` must take the GPU gate: it runs on the chat
/// model's machine. Host compare only (ports ignored, no DNS); loopback names
/// count as one host; an unparsable URL counts as shared.
pub(crate) fn shares_gpu_gate(endpoint: &str, chat_endpoint: &str) -> bool {
    let host = |s: &str| {
        reqwest::Url::parse(s.trim())
            .ok()
            .and_then(|u| u.host_str().map(str::to_lowercase))
    };
    let (Some(a), Some(b)) = (host(endpoint), host(chat_endpoint)) else {
        return true;
    };
    let loopback = |h: &str| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    };
    a == b || (loopback(&a) && loopback(&b))
}

/// Whether `endpoint`'s host is parked for the running image generation.
pub(crate) fn is_parked(endpoint: &str) -> bool {
    let Some(host) = base_host(endpoint) else {
        return false;
    };
    PARKED.lock().expect("lock poisoned").contains(&host)
}

/// Whether a helper/embedding request to `endpoint` must take the gate.
pub(crate) fn needs_gate(endpoint: &str, chat_endpoint: &str) -> bool {
    shares_gpu_gate(endpoint, chat_endpoint) || is_parked(endpoint)
}

/// Removes its hosts from `PARKED` on drop, so an early return or panic can't
/// leave a host parked forever.
struct Parked(Vec<String>);

impl Parked {
    fn new(hosts: Vec<String>) -> Self {
        PARKED
            .lock()
            .expect("lock poisoned")
            .extend(hosts.iter().cloned());
        Parked(hosts)
    }
}

impl Drop for Parked {
    fn drop(&mut self) {
        let mut parked = PARKED.lock().expect("lock poisoned");
        for host in &self.0 {
            if let Some(pos) = parked.iter().position(|h| h == host) {
                parked.remove(pos);
            }
        }
    }
}

/// LM Studio can suffix a key with `@variant` (quantization); compare without it.
fn same_model(candidate: &str, wanted: &str) -> bool {
    let strip = |s: &str| s.split('@').next().unwrap_or(s).to_string();
    candidate == wanted || strip(candidate) == strip(wanted)
}

/// The loaded instances of `model` in an LM Studio `/api/v1/models` body.
/// Other models the user has loaded are left alone. `None` means the body
/// isn't LM Studio's.
fn find_instances(body: &Value, model: &str) -> Option<Vec<Instance>> {
    let models = body.get("models")?.as_array()?;
    let mut out = Vec::new();
    for entry in models {
        let Some(key) = entry.get("key").and_then(Value::as_str) else {
            continue;
        };
        let key_matches = same_model(key, model);
        let loaded = entry
            .get("loaded_instances")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for inst in loaded {
            let Some(id) = inst.get("id").and_then(Value::as_str) else {
                continue;
            };
            if key_matches || same_model(id, model) {
                out.push(Instance {
                    key: key.to_string(),
                    id: id.to_string(),
                    config: inst
                        .get("config")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default(),
                });
            }
        }
    }
    Some(out)
}

fn request(
    api_key: &str,
    method: reqwest::Method,
    url: String,
    timeout: Duration,
) -> reqwest::RequestBuilder {
    let mut builder = crate::llm::client().request(method, url).timeout(timeout);
    if !api_key.trim().is_empty() {
        builder = builder.bearer_auth(api_key);
    }
    builder
}

/// Asks the server to drop `target`'s model from memory. Never fatal: on any
/// failure the generation just runs with the model still loaded.
pub async fn unload_model(target: ModelTarget<'_>) -> Unloaded {
    let started = Instant::now();
    let Some(host) = base_host(target.endpoint) else {
        log::warn!(
            "gpu: can't unload {}, endpoint isn't a URL: {}",
            target.model,
            target.endpoint
        );
        return Unloaded::default();
    };

    // LM Studio: its native API lists loaded instances with their config.
    let listing = request(
        target.api_key,
        reqwest::Method::GET,
        format!("{host}/api/v1/models"),
        PROBE_TIMEOUT,
    )
    .send()
    .await;
    if let Ok(resp) = listing {
        if resp.status().is_success() {
            if let Ok(body) = resp.json::<Value>().await {
                if let Some(found) = find_instances(&body, target.model) {
                    return unload_lm_studio(target, &host, found, started).await;
                }
            }
        }
    }

    // Ollama: `keep_alive: 0` with no prompt unloads right away.
    let is_ollama = match request(
        target.api_key,
        reqwest::Method::GET,
        format!("{host}/api/version"),
        PROBE_TIMEOUT,
    )
    .send()
    .await
    {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    };
    if is_ollama {
        let res = request(
            target.api_key,
            reqwest::Method::POST,
            format!("{host}/api/generate"),
            SWAP_TIMEOUT,
        )
        .json(&json!({ "model": target.model, "keep_alive": 0 }))
        .send()
        .await
        .and_then(|r| r.error_for_status());
        match res {
            Ok(_) => log::info!(
                "gpu: unloaded {} from Ollama in {:.1}s",
                target.model,
                started.elapsed().as_secs_f32()
            ),
            Err(e) => log::warn!("gpu: Ollama unload of {} failed: {e}", target.model),
        }
        return Unloaded::default();
    }

    log::warn!("gpu: this LLM server ({host}) can't be asked to unload; the model stays in memory");
    Unloaded::default()
}

async fn unload_lm_studio(
    target: ModelTarget<'_>,
    host: &str,
    found: Vec<Instance>,
    started: Instant,
) -> Unloaded {
    if found.is_empty() {
        log::info!(
            "gpu: {} isn't loaded in LM Studio, nothing to unload",
            target.model
        );
        return Unloaded::default();
    }
    let mut done = Vec::new();
    for inst in found {
        let res = request(
            target.api_key,
            reqwest::Method::POST,
            format!("{host}/api/v1/models/unload"),
            SWAP_TIMEOUT,
        )
        .json(&json!({ "instance_id": inst.id }))
        .send()
        .await
        .and_then(|r| r.error_for_status());
        match res {
            Ok(_) => done.push(inst),
            Err(e) => log::warn!("gpu: LM Studio unload of {} failed: {e}", inst.id),
        }
    }
    if !done.is_empty() {
        log::info!(
            "gpu: unloaded {} from LM Studio in {:.1}s",
            target.model,
            started.elapsed().as_secs_f32()
        );
    }
    Unloaded { instances: done }
}

/// Puts back what `unload_model` took, with the same load settings.
/// Never fatal: LM Studio's just-in-time loading would still load the model
/// on the next request.
pub async fn reload_model(target: ModelTarget<'_>, unloaded: &Unloaded) {
    if unloaded.instances.is_empty() {
        return;
    }
    let Some(host) = base_host(target.endpoint) else {
        return;
    };
    let started = Instant::now();
    for inst in &unloaded.instances {
        let mut body = inst.config.clone();
        body.insert("model".into(), json!(inst.key));
        let res = request(
            target.api_key,
            reqwest::Method::POST,
            format!("{host}/api/v1/models/load"),
            SWAP_TIMEOUT,
        )
        .json(&body)
        .send()
        .await
        .and_then(|r| r.error_for_status());
        match res {
            Ok(_) => log::info!(
                "gpu: reloaded {} in {:.1}s",
                inst.key,
                started.elapsed().as_secs_f32()
            ),
            Err(e) => log::warn!(
                "gpu: reloading {} failed ({e}); LM Studio will load it on the next request",
                inst.key
            ),
        }
    }
}

/// This computer's own interface addresses, so `192.168.1.2` and
/// `localhost` count as the same machine.
fn local_addrs() -> HashSet<IpAddr> {
    let mut out = HashSet::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs hands back a linked list that is only read here and
    // freed exactly once with freeifaddrs; each sockaddr is cast according to
    // its own sa_family.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while !cur.is_null() {
            let addr = (*cur).ifa_addr;
            if !addr.is_null() {
                match i32::from((*addr).sa_family) {
                    libc::AF_INET => {
                        let sin = addr as *const libc::sockaddr_in;
                        out.insert(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                            (*sin).sin_addr.s_addr,
                        ))));
                    }
                    libc::AF_INET6 => {
                        let sin6 = addr as *const libc::sockaddr_in6;
                        out.insert(IpAddr::V6(Ipv6Addr::from((*sin6).sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            cur = (*cur).ifa_next;
        }
        libc::freeifaddrs(list);
    }
    out
}

/// Every address a URL's host resolves to; `None` if it can't be resolved.
async fn resolve_url(url: &str) -> Option<Vec<IpAddr>> {
    let url = reqwest::Url::parse(url.trim()).ok()?;
    let host = url
        .host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(vec![ip]);
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let addrs = tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .ok()?
        .ok()?;
    let ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
    (!ips.is_empty()).then_some(ips)
}

/// Whether the chat model and ComfyUI run on one machine (and so share its
/// GPU). `None` when a host can't be resolved.
pub async fn same_machine(llm_endpoint: &str, comfy_url: &str) -> Option<bool> {
    let a = resolve_url(llm_endpoint).await?;
    let b = resolve_url(comfy_url).await?;
    let local = local_addrs();
    let is_local = |ip: &IpAddr| ip.is_loopback() || local.contains(ip);
    Some((a.iter().any(is_local) && b.iter().any(is_local)) || a.iter().any(|ip| b.contains(ip)))
}

/// Runs one image generation with the configured GPU handling. The reload
/// and the ComfyUI free happen on the error path too, and the gate is
/// released only after the model is back, so callers must run any LLM
/// follow-up (the reaction turn) after this returns.
/// With only helper/embedding models unloaded the gate is still exclusive, so
/// chat requests wait for the image too (the trade-off of that opt-in switch).
pub async fn with_image_slot<T, F, Fut>(cfg: &AppConfig, comfy: &ComfyUiConfig, generate: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    // Unloading a model on another machine frees nothing here and costs a
    // reload (plus its prompt cache) on the next reply, so it only happens
    // when both really share this GPU.
    let on_comfy_machine = |endpoint: &str, what: &'static str| {
        let endpoint = endpoint.to_string();
        async move {
            match same_machine(&endpoint, &comfy.base_url).await {
                Some(true) => true,
                Some(false) => {
                    log::info!("gpu: {what} and ComfyUI are on different machines; not unloading");
                    false
                }
                None => {
                    log::warn!(
                        "gpu: couldn't tell whether {what} and ComfyUI share a machine; \
                         not unloading"
                    );
                    false
                }
            }
        }
    };
    let share = comfy.unload_llm_during_generation
        && on_comfy_machine(&cfg.endpoint, "the chat model").await;

    let mut helpers: Vec<ModelTarget<'_>> = Vec::new();
    if comfy.unload_helper_during_generation {
        let mut wanted: Vec<ModelTarget<'_>> = cfg.helper().into_iter().collect();
        if !cfg.chat_recall_embedding_model.trim().is_empty() {
            wanted.push(cfg.embedding_target());
        }
        for t in wanted {
            let key = (base_host(t.endpoint), t.model);
            let is_chat = share && key == (base_host(&cfg.endpoint), cfg.model.as_str());
            let dup = helpers
                .iter()
                .any(|h| (base_host(h.endpoint), h.model) == key);
            if is_chat || dup || !on_comfy_machine(t.endpoint, "the helper model").await {
                continue;
            }
            helpers.push(t);
        }
    }

    // Park first so new helper/embedding requests queue on the gate.
    let parked = Parked::new(
        helpers
            .iter()
            .filter_map(|t| base_host(t.endpoint))
            .collect(),
    );
    let gate = if share || !helpers.is_empty() {
        Some(exclusive().await)
    } else {
        None
    };
    let unloaded_chat = if share {
        Some(unload_model(cfg.main_target()).await)
    } else {
        None
    };
    let mut unloaded_helpers = Vec::new();
    for t in &helpers {
        unloaded_helpers.push(unload_model(*t).await);
    }
    let out = generate().await;
    if comfy.free_after_generation {
        comfyui::free_memory(comfy).await;
    }
    if let Some(unloaded) = unloaded_chat {
        reload_model(cfg.main_target(), &unloaded).await;
    }
    for (t, unloaded) in helpers.iter().zip(&unloaded_helpers) {
        reload_model(*t, unloaded).await;
    }
    drop(parked);
    drop(gate);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex as StdMutex};

    pub(crate) type Seen = Arc<StdMutex<Vec<(String, String, String)>>>;

    /// Multi-request fake server: `handler(method, path, body)` returns
    /// `(status, json body)`; every request is recorded as (method, path, body).
    pub(crate) fn fake_server(
        handler: impl Fn(&str, &str, &str) -> (u16, String) + Send + 'static,
    ) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = Arc::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let body_start = loop {
                    let n = stream.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(p + 4);
                    }
                };
                let Some(body_start) = body_start else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buf[..body_start]).to_string();
                let len: usize = head
                    .to_lowercase()
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").map(str::to_string))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while buf.len() < body_start + len {
                    let n = stream.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body = String::from_utf8_lossy(&buf[body_start..]).to_string();
                let mut parts = head.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let (status, reply) = handler(&method, &path, &body);
                log.lock().unwrap().push((method, path, body));
                let out = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = stream.write_all(out.as_bytes());
            }
        });
        (host, seen)
    }

    fn cfg_for(host: &str, model: &str) -> AppConfig {
        AppConfig {
            endpoint: format!("{host}/v1/chat/completions"),
            model: model.into(),
            ..AppConfig::default()
        }
    }

    fn lm_studio_models() -> String {
        json!({"models": [
            {"type": "llm", "key": "google/gemma-3-4b", "loaded_instances": [
                {"id": "google/gemma-3-4b", "config": {
                    "context_length": 8192, "eval_batch_size": 512,
                    "flash_attention": true, "offload_kv_cache_to_gpu": true}}]},
            {"type": "llm", "key": "qwen/other-model", "loaded_instances": [
                {"id": "qwen/other-model", "config": {"context_length": 4096}}]},
            {"type": "llm", "key": "not/loaded", "loaded_instances": []}
        ]})
        .to_string()
    }

    fn lm_studio_handler(method: &str, path: &str, _body: &str) -> (u16, String) {
        match (method, path) {
            ("GET", "/api/v1/models") => (200, lm_studio_models()),
            ("POST", "/api/v1/models/unload") | ("POST", "/api/v1/models/load") => {
                (200, "{}".into())
            }
            _ => (404, "{}".into()),
        }
    }

    fn requests(seen: &Seen) -> Vec<(String, String)> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|(m, p, _)| (m.clone(), p.clone()))
            .collect()
    }

    #[tokio::test]
    async fn same_machine_detection() {
        assert_eq!(
            same_machine(
                "http://localhost:1234/v1/chat/completions",
                "http://127.0.0.1:8188"
            )
            .await,
            Some(true)
        );
        // This computer's own LAN address counts as local.
        if let Some(own) = local_addrs()
            .into_iter()
            .find(|ip| ip.is_ipv4() && !ip.is_loopback())
        {
            assert_eq!(
                same_machine(
                    &format!("http://{own}:1234/v1/chat/completions"),
                    "http://localhost:8188"
                )
                .await,
                Some(true)
            );
        }
        // TEST-NET-1 is never assigned to a real interface.
        assert_eq!(
            same_machine(
                "http://192.0.2.10:1234/v1/chat/completions",
                "http://localhost:8188"
            )
            .await,
            Some(false)
        );
        assert_eq!(
            same_machine("http://192.0.2.10:1234/v1", "http://192.0.2.10:8188").await,
            Some(true),
            "both on the same remote box"
        );
        assert_eq!(
            same_machine(
                "http://no-such-host.invalid:1234/v1",
                "http://localhost:8188"
            )
            .await,
            None
        );
    }

    #[tokio::test]
    async fn a_remote_chat_model_is_never_unloaded() {
        let (comfy, seen) = fake_server(|_, _, _| (200, "{}".into()));
        let comfy_cfg = ComfyUiConfig {
            base_url: comfy.clone(),
            unload_llm_during_generation: true,
            free_after_generation: true,
            ..ComfyUiConfig::default()
        };
        // Unroutable: any unload attempt would hang or fail, not reach the fake.
        let cfg = cfg_for("http://192.0.2.10:1234", "m");
        let out = with_image_slot(&cfg, &comfy_cfg, || async { 7 }).await;
        assert_eq!(out, 7);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1, "/free");
    }

    #[test]
    fn base_host_strips_the_path() {
        assert_eq!(
            base_host("http://192.168.1.5:1234/v1/chat/completions?x=1").as_deref(),
            Some("http://192.168.1.5:1234")
        );
        assert_eq!(base_host("not a url"), None);
    }

    #[test]
    fn model_matching_tolerates_variant_suffix_only() {
        assert!(same_model("google/gemma-3-4b@q4_k_m", "google/gemma-3-4b"));
        assert!(same_model("google/gemma-3-4b", "google/gemma-3-4b@q8_0"));
        assert!(!same_model("google/gemma-3-4b-qat", "google/gemma-3-4b"));
    }

    #[tokio::test]
    async fn lm_studio_unloads_only_the_configured_model_and_reloads_with_its_config() {
        let (host, seen) = fake_server(lm_studio_handler);
        let cfg = cfg_for(&host, "google/gemma-3-4b");

        let unloaded = unload_model(cfg.main_target()).await;
        assert_eq!(unloaded.instances.len(), 1);
        {
            let seen = seen.lock().unwrap();
            let unloads: Vec<_> = seen
                .iter()
                .filter(|(_, p, _)| p == "/api/v1/models/unload")
                .collect();
            assert_eq!(unloads.len(), 1);
            let body: Value = serde_json::from_str(&unloads[0].2).unwrap();
            assert_eq!(body, json!({"instance_id": "google/gemma-3-4b"}));
        }

        reload_model(cfg.main_target(), &unloaded).await;
        let seen = seen.lock().unwrap();
        let load = seen
            .iter()
            .find(|(_, p, _)| p == "/api/v1/models/load")
            .expect("a load request");
        let body: Value = serde_json::from_str(&load.2).unwrap();
        assert_eq!(
            body,
            json!({"model": "google/gemma-3-4b", "context_length": 8192,
                   "eval_batch_size": 512, "flash_attention": true,
                   "offload_kv_cache_to_gpu": true})
        );
    }

    #[tokio::test]
    async fn lm_studio_matches_a_variant_and_sends_the_bearer_key() {
        let (host, seen) = fake_server(lm_studio_handler);
        let mut cfg = cfg_for(&host, "google/gemma-3-4b@q4_k_m");
        cfg.api_key = "secret".into();
        let unloaded = unload_model(cfg.main_target()).await;
        assert_eq!(unloaded.instances.len(), 1);
        assert_eq!(unloaded.instances[0].key, "google/gemma-3-4b");
        assert!(!requests(&seen).is_empty());
    }

    #[tokio::test]
    async fn ollama_is_unloaded_with_keep_alive_zero_and_needs_no_reload() {
        let (host, seen) = fake_server(|m, p, _| match (m, p) {
            ("GET", "/api/version") => (200, r#"{"version":"0.5.0"}"#.into()),
            ("POST", "/api/generate") => (200, "{}".into()),
            _ => (404, "{}".into()),
        });
        let cfg = cfg_for(&host, "gemma3:1b");
        let unloaded = unload_model(cfg.main_target()).await;
        assert!(unloaded.instances.is_empty());
        {
            let seen = seen.lock().unwrap();
            let gen = seen
                .iter()
                .find(|(_, p, _)| p == "/api/generate")
                .expect("a generate request");
            let body: Value = serde_json::from_str(&gen.2).unwrap();
            assert_eq!(body, json!({"model": "gemma3:1b", "keep_alive": 0}));
        }
        let before = seen.lock().unwrap().len();
        reload_model(cfg.main_target(), &unloaded).await;
        assert_eq!(seen.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn unknown_server_is_left_alone() {
        let (host, seen) = fake_server(|_, _, _| (404, "{}".into()));
        let cfg = cfg_for(&host, "m");
        let unloaded = unload_model(cfg.main_target()).await;
        reload_model(cfg.main_target(), &unloaded).await;
        assert!(unloaded.instances.is_empty());
        assert!(requests(&seen).iter().all(|(m, _)| m == "GET"));
    }

    #[tokio::test]
    async fn llm_permit_waits_while_the_gate_is_exclusive() {
        let write = exclusive().await;
        let waited = tokio::time::timeout(Duration::from_millis(150), llm_permit()).await;
        assert!(waited.is_err(), "permit must wait for the image swap");
        drop(write);
        let _guard = tokio::time::timeout(Duration::from_secs(5), llm_permit())
            .await
            .expect("permit after the swap");
    }

    #[tokio::test]
    async fn exclusive_waits_for_in_flight_llm_requests() {
        let read = llm_permit().await;
        let waited = tokio::time::timeout(Duration::from_millis(150), exclusive()).await;
        assert!(waited.is_err(), "swap must wait for the LLM request");
        drop(read);
        let _guard = tokio::time::timeout(Duration::from_secs(5), exclusive())
            .await
            .expect("exclusive once the request is done");
    }

    #[tokio::test]
    async fn image_slot_frees_and_reloads_even_when_generation_fails() {
        let (host, seen) = fake_server(|m, p, b| match (m, p) {
            ("POST", "/free") => {
                assert_eq!(
                    serde_json::from_str::<Value>(b).unwrap(),
                    json!({"unload_models": true, "free_memory": true})
                );
                (200, "{}".into())
            }
            _ => lm_studio_handler(m, p, b),
        });
        let cfg = cfg_for(&host, "google/gemma-3-4b");
        let comfy = ComfyUiConfig {
            base_url: host.clone(),
            unload_llm_during_generation: true,
            free_after_generation: true,
            ..ComfyUiConfig::default()
        };
        let out: Result<(), &str> = with_image_slot(&cfg, &comfy, || async { Err("boom") }).await;
        assert_eq!(out, Err("boom"));
        let posts: Vec<String> = requests(&seen)
            .into_iter()
            .filter(|(m, _)| m == "POST")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(
            posts,
            ["/api/v1/models/unload", "/free", "/api/v1/models/load"]
        );
        // The gate is released again.
        let _guard = tokio::time::timeout(Duration::from_secs(5), llm_permit())
            .await
            .expect("gate released");
    }

    #[tokio::test]
    async fn image_slot_does_nothing_extra_when_both_options_are_off() {
        let (host, seen) = fake_server(lm_studio_handler);
        let cfg = cfg_for(&host, "google/gemma-3-4b");
        let comfy = ComfyUiConfig::default();
        let out = with_image_slot(&cfg, &comfy, || async { 7 }).await;
        assert_eq!(out, 7);
        assert!(requests(&seen).is_empty());
    }

    #[test]
    fn shares_gpu_gate_compares_hosts_only() {
        let chat = "http://192.168.1.3:1234/v1/chat/completions";
        assert!(shares_gpu_gate("http://192.168.1.3:11434", chat));
        assert!(!shares_gpu_gate("http://192.168.1.7:1234", chat));
        assert!(shares_gpu_gate(
            "http://localhost:1/v1",
            "http://127.0.0.1:1234/v1"
        ));
        assert!(shares_gpu_gate("not a url", chat));
    }

    #[tokio::test]
    async fn ungated_requests_skip_an_exclusive_gate() {
        let write = exclusive().await;
        let permit = tokio::time::timeout(Duration::from_secs(1), ungated(llm_permit()))
            .await
            .expect("no wait");
        assert!(permit.is_none());
        drop(write);
    }

    fn helper_cfg(chat: &str, helper: &str) -> AppConfig {
        AppConfig {
            endpoint: format!("{chat}/v1/chat/completions"),
            model: "chat-model".into(),
            helper_endpoint: format!("{helper}/v1/chat/completions"),
            helper_model: "small".into(),
            ..AppConfig::default()
        }
    }

    fn posts(seen: &Seen) -> Vec<(String, String)> {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(m, p, _)| m == "POST" && p.starts_with("/api/v1/models/"))
            .map(|(_, p, b)| (p.clone(), b.clone()))
            .collect()
    }

    fn small_model_handler(method: &str, path: &str, body: &str) -> (u16, String) {
        match (method, path) {
            ("GET", "/api/v1/models") => (
                200,
                json!({"models": [{"type": "llm", "key": "small", "loaded_instances": [
                    {"id": "small", "config": {"context_length": 2048}}]}]})
                .to_string(),
            ),
            _ => lm_studio_handler(method, path, body),
        }
    }

    #[tokio::test]
    async fn helper_on_comfys_machine_is_unloaded_when_the_option_is_on() {
        let (host, seen) = fake_server(small_model_handler);
        // Chat elsewhere and not unloaded: only the helper moves.
        let cfg = helper_cfg("http://192.0.2.10:1234", &host);
        let comfy = ComfyUiConfig {
            base_url: host.clone(),
            unload_helper_during_generation: true,
            ..ComfyUiConfig::default()
        };
        let parked_during = with_image_slot(&cfg, &comfy, || async { is_parked(&host) }).await;
        assert!(parked_during, "helper host is parked while generating");
        assert!(!is_parked(&host), "parked entry removed afterwards");
        let posts = posts(&seen);
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].0, "/api/v1/models/unload");
        assert_eq!(posts[1].0, "/api/v1/models/load");
        assert!(posts[1].1.contains("\"small\""));
    }

    #[tokio::test]
    async fn helper_stays_loaded_when_the_option_is_off() {
        let (host, seen) = fake_server(small_model_handler);
        let cfg = helper_cfg("http://192.0.2.10:1234", &host);
        let comfy = ComfyUiConfig {
            base_url: host.clone(),
            ..ComfyUiConfig::default()
        };
        with_image_slot(&cfg, &comfy, || async {}).await;
        assert!(requests(&seen).is_empty());
    }

    #[tokio::test]
    async fn parked_host_is_released_after_a_panicking_generation() {
        let (host, _seen) = fake_server(small_model_handler);
        let cfg = helper_cfg("http://192.0.2.10:1234", &host);
        let comfy = ComfyUiConfig {
            base_url: host.clone(),
            unload_helper_during_generation: true,
            ..ComfyUiConfig::default()
        };
        let task = tokio::spawn({
            let host = host.clone();
            async move {
                with_image_slot(&cfg, &comfy, || async { panic!("boom") }).await;
                host
            }
        });
        assert!(task.await.is_err());
        assert!(!is_parked(&host), "guard must unpark on unwind");
        let _guard = tokio::time::timeout(Duration::from_secs(5), llm_permit())
            .await
            .expect("gate released");
    }

    #[test]
    fn needs_gate_is_true_while_parked() {
        let chat = "http://192.168.1.3:1234/v1/chat/completions";
        let other = "http://192.0.2.77:1234/v1/chat/completions";
        assert!(!needs_gate(other, chat));
        let guard = Parked::new(vec![base_host(other).unwrap()]);
        assert!(needs_gate(other, chat));
        drop(guard);
        assert!(!needs_gate(other, chat));
    }
}
