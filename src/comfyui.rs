//! ComfyUI image generation: a pasted API-format workflow (exported from
//! ComfyUI's own UI) plus a manual field mapping, the same pattern Open
//! WebUI's ComfyUI integration uses -- everything the workflow doesn't need
//! touched (LoRA nodes, seed handling, the Image Saver node) stays exactly
//! as the user set it up; only the handful of mapped fields get overwritten
//! per-request from the prompt the chat director's builder writes
//! (`chat_director::build_image_prompt`).
//!
//! Stored separately from `config.toml` (`<app-config-dir>/comfyui.json`)
//! since the pasted workflow JSON can be several KB -- same reasoning
//! personas/rulesets live in their own files instead of being crammed into
//! one config blob.

use crate::paths::{app_config_dir, sanitize_filename, write_atomic};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Each field holds a `"node_id.input_key"` path into the pasted workflow,
/// or empty if that field isn't mapped. The frontend never lets a user type
/// a node id directly (see the Settings "Image Gen" tab) -- it's built from
/// a `<select>` populated by parsing the pasted workflow, since node ids are
/// specific to one export and would silently stop matching after pasting a
/// different workflow.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ComfyUiMapping {
    #[serde(default)]
    pub checkpoint: String,
    #[serde(default)]
    pub positive: String,
    #[serde(default)]
    pub negative: String,
    #[serde(default)]
    pub width: String,
    #[serde(default)]
    pub height: String,
    #[serde(default)]
    pub sampler: String,
    #[serde(default)]
    pub scheduler: String,
    #[serde(default)]
    pub cfg: String,
    #[serde(default)]
    pub steps: String,
}

/// What the character does once a generated image lands: always continue the
/// conversation in character, decide for herself whether continuing fits
/// (`Optional`), or say nothing (`Never`, no extra request at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReactionMode {
    #[default]
    Always,
    Optional,
    /// Wire value `"none"`, not the `snake_case`-derived `"never"` -- matches
    /// the Settings dropdown's own option value and how this was asked for.
    #[serde(rename = "none")]
    Never,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComfyUiConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub workflow_json: String,
    #[serde(default)]
    pub mapping: ComfyUiMapping,
    #[serde(default = "default_output_dir")]
    pub output_dir: String,
    #[serde(default = "default_filename_pattern")]
    pub filename_pattern: String,
    #[serde(default)]
    pub reaction_mode: ReactionMode,
    /// Unload the chat model for the duration of an image generation
    /// (`gpu::with_image_slot`), for running both on one GPU.
    #[serde(default)]
    pub unload_llm_during_generation: bool,
    /// Ask ComfyUI to drop its models and cached memory after each image.
    #[serde(default)]
    pub free_after_generation: bool,
}

fn default_output_dir() -> String {
    app_config_dir()
        .join("chat")
        .join("generated-images")
        .display()
        .to_string()
}

fn default_filename_pattern() -> String {
    "{session}-{timestamp}".to_string()
}

impl Default for ComfyUiConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            workflow_json: String::new(),
            mapping: ComfyUiMapping::default(),
            output_dir: default_output_dir(),
            filename_pattern: default_filename_pattern(),
            reaction_mode: ReactionMode::default(),
            unload_llm_during_generation: false,
            free_after_generation: false,
        }
    }
}

fn config_path() -> PathBuf {
    app_config_dir().join("comfyui.json")
}

pub fn load_or_init() -> anyhow::Result<ComfyUiConfig> {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let cfg = ComfyUiConfig::default();
            save(&cfg)?;
            Ok(cfg)
        }
        Err(e) => Err(anyhow::anyhow!("{}: {e}", path.display())),
    }
}

pub fn save(cfg: &ComfyUiConfig) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(&path, serde_json::to_string_pretty(cfg)?.as_bytes(), None)?;
    Ok(())
}

/// One image request's optional overrides. Anything left `None` keeps
/// whatever value is already in the pasted workflow JSON for that mapped
/// field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImagePromptFields {
    #[serde(default)]
    pub checkpoint: Option<String>,
    #[serde(default)]
    pub positive: Option<String>,
    #[serde(default)]
    pub negative: Option<String>,
    #[serde(default)]
    pub width: Option<i64>,
    #[serde(default)]
    pub height: Option<i64>,
    #[serde(default)]
    pub sampler: Option<String>,
    #[serde(default)]
    pub scheduler: Option<String>,
    #[serde(default)]
    pub cfg: Option<f64>,
    #[serde(default)]
    pub steps: Option<i64>,
}

/// Splits a `"node_id.input_key"` mapping string and writes `value` into
/// `workflow[node_id]["inputs"][input_key]`. Errors (rather than silently
/// no-opping) if `node_id` isn't actually a node in `workflow` -- a typo'd
/// or stale mapping (e.g. after pasting a different workflow without
/// re-mapping) should be visible, not swallowed. A blank mapping path is a
/// deliberately-unmapped field, not an error.
fn set_mapped(
    workflow: &mut serde_json::Value,
    mapping_path: &str,
    value: serde_json::Value,
) -> anyhow::Result<()> {
    let mapping_path = mapping_path.trim();
    if mapping_path.is_empty() {
        return Ok(());
    }
    let (node_id, input_key) = mapping_path.split_once('.').ok_or_else(|| {
        anyhow::anyhow!("malformed mapping \"{mapping_path}\" (expected \"node_id.input_key\")")
    })?;
    let node = workflow.get_mut(node_id).ok_or_else(|| {
        anyhow::anyhow!("mapped node \"{node_id}\" doesn't exist in this workflow")
    })?;
    let inputs = node
        .get_mut("inputs")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| anyhow::anyhow!("node \"{node_id}\" has no \"inputs\" object"))?;
    inputs.insert(input_key.to_string(), value);
    Ok(())
}

/// Applies whatever the model actually requested onto `workflow` in place --
/// everything not mapped, or mapped but omitted from `fields`, is left
/// exactly as the pasted workflow JSON already has it.
pub fn apply_mapping(
    workflow: &mut serde_json::Value,
    mapping: &ComfyUiMapping,
    fields: &ImagePromptFields,
) -> anyhow::Result<()> {
    if let Some(v) = &fields.checkpoint {
        set_mapped(
            workflow,
            &mapping.checkpoint,
            serde_json::Value::String(v.clone()),
        )?;
    }
    if let Some(v) = &fields.positive {
        set_mapped(
            workflow,
            &mapping.positive,
            serde_json::Value::String(v.clone()),
        )?;
    }
    if let Some(v) = &fields.negative {
        set_mapped(
            workflow,
            &mapping.negative,
            serde_json::Value::String(v.clone()),
        )?;
    }
    if let Some(v) = fields.width {
        set_mapped(workflow, &mapping.width, serde_json::Value::from(v))?;
    }
    if let Some(v) = fields.height {
        set_mapped(workflow, &mapping.height, serde_json::Value::from(v))?;
    }
    if let Some(v) = &fields.sampler {
        set_mapped(
            workflow,
            &mapping.sampler,
            serde_json::Value::String(v.clone()),
        )?;
    }
    if let Some(v) = &fields.scheduler {
        set_mapped(
            workflow,
            &mapping.scheduler,
            serde_json::Value::String(v.clone()),
        )?;
    }
    if let Some(v) = fields.cfg {
        set_mapped(workflow, &mapping.cfg, serde_json::Value::from(v))?;
    }
    if let Some(v) = fields.steps {
        set_mapped(workflow, &mapping.steps, serde_json::Value::from(v))?;
    }
    Ok(())
}

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const POLL_TIMEOUT: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per HTTP request, not for the whole generation -- `POLL_TIMEOUT` bounds that.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// One image downloaded from ComfyUI. `filename` is ComfyUI's own name
/// (extension included) for the saved output -- `render_filename` reuses
/// its extension rather than assuming PNG.
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub filename: String,
}

/// `POST {base_url}/free`: ComfyUI unloads its models and frees cached VRAM.
/// Never fatal -- the image is already made.
pub async fn free_memory(cfg: &ComfyUiConfig) {
    let started = std::time::Instant::now();
    let base_url = cfg.base_url.trim().trim_end_matches('/');
    let res = async {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()?
            .post(format!("{base_url}/free"))
            .json(&serde_json::json!({ "unload_models": true, "free_memory": true }))
            .send()
            .await?
            .error_for_status()
    }
    .await;
    match res {
        Ok(_) => log::info!(
            "gpu: comfyui freed in {:.1}s",
            started.elapsed().as_secs_f32()
        ),
        Err(e) => log::warn!("gpu: freeing ComfyUI's memory failed: {e}"),
    }
}

/// POSTs the mapped workflow to `{base_url}/prompt`, polls
/// `{base_url}/history/{prompt_id}` until it has output, then downloads the
/// first image found via `{base_url}/view`. `cfg.base_url`/`workflow_json`
/// being empty or invalid is what "ComfyUI isn't configured yet" looks
/// like -- surfaced as a real error, not a silent no-op.
pub async fn generate_image(
    cfg: &ComfyUiConfig,
    fields: &ImagePromptFields,
) -> anyhow::Result<GeneratedImage> {
    if cfg.base_url.trim().is_empty() {
        anyhow::bail!("ComfyUI isn't configured yet -- set a base URL in Settings' Image Gen tab");
    }
    let mut workflow: serde_json::Value =
        serde_json::from_str(cfg.workflow_json.trim()).map_err(|e| {
            anyhow::anyhow!(
                "the saved ComfyUI workflow JSON doesn't parse ({e}) -- re-paste it in Settings"
            )
        })?;
    apply_mapping(&mut workflow, &cfg.mapping, fields)?;

    let base_url = cfg.base_url.trim().trim_end_matches('/');
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?;
    let client_id = format!(
        "llm-assistant-{}-{}",
        std::process::id(),
        chrono::Local::now().timestamp_nanos_opt().unwrap_or(0)
    );

    let submit: serde_json::Value = client
        .post(format!("{base_url}/prompt"))
        .json(&serde_json::json!({ "prompt": workflow, "client_id": client_id }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let prompt_id = submit
        .get("prompt_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("ComfyUI's /prompt response had no prompt_id: {submit}"))?
        .to_string();

    let deadline = std::time::Instant::now() + POLL_TIMEOUT;
    let (subfolder, filename, file_type) = loop {
        if std::time::Instant::now() > deadline {
            anyhow::bail!(
                "timed out waiting for ComfyUI to finish generating (waited {POLL_TIMEOUT:?})"
            );
        }
        let history: serde_json::Value = client
            .get(format!("{base_url}/history/{prompt_id}"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(entry) = history.get(&prompt_id) {
            if let Some(found) = find_first_image(entry) {
                break found;
            }
            if let Some(reason) = poll_failure(entry) {
                anyhow::bail!("ComfyUI failed to generate the image: {reason}");
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };

    let view_url = format!(
        "{base_url}/view?filename={}&subfolder={}&type={}",
        url_component(&filename),
        url_component(&subfolder),
        url_component(&file_type),
    );
    let bytes = client
        .get(&view_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?
        .to_vec();

    Ok(GeneratedImage { bytes, filename })
}

/// Scans a `/history/{prompt_id}` entry's `outputs` for an `images` array
/// and returns `(subfolder, filename, type)` for its first image. Nodes
/// without images (text/metadata outputs) are skipped, and a saved
/// `type == "output"` image wins over a `temp` preview.
fn find_first_image(history_entry: &serde_json::Value) -> Option<(String, String, String)> {
    let outputs = history_entry.get("outputs")?.as_object()?;
    let mut temp_fallback = None;
    for node_output in outputs.values() {
        let Some(images) = node_output.get("images").and_then(|v| v.as_array()) else {
            continue;
        };
        let Some(first) = images.first() else {
            continue;
        };
        let Some(filename) = first.get("filename").and_then(|v| v.as_str()) else {
            continue;
        };
        let subfolder = first
            .get("subfolder")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let file_type = first
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("output")
            .to_string();
        let found = (subfolder, filename.to_string(), file_type);
        if found.2 == "output" {
            return Some(found);
        }
        temp_fallback.get_or_insert(found);
    }
    temp_fallback
}

/// Why a history entry that has no image will never get one: ComfyUI marked
/// it `error` (its `execution_error` message is surfaced when present), or it
/// finished without producing any image output.
fn poll_failure(entry: &serde_json::Value) -> Option<String> {
    let status = entry.get("status")?;
    if status.get("status_str").and_then(|v| v.as_str()) == Some("error") {
        let detail = status
            .get("messages")
            .and_then(|m| m.as_array())
            .and_then(|msgs| {
                msgs.iter().find_map(|m| {
                    let pair = m.as_array()?;
                    if pair.first()?.as_str()? != "execution_error" {
                        return None;
                    }
                    let data = pair.get(1)?;
                    let msg = data.get("exception_message")?.as_str()?.trim();
                    Some(match data.get("node_type").and_then(|v| v.as_str()) {
                        Some(node) => format!("{msg} (node {node})"),
                        None => msg.to_string(),
                    })
                })
            });
        return Some(detail.unwrap_or_else(|| "execution error (see ComfyUI's console)".into()));
    }
    if status.get("completed").and_then(|v| v.as_bool()) == Some(true) {
        return Some("the workflow finished but produced no image output".into());
    }
    None
}

/// Minimal query-string escaping -- just enough for filenames ComfyUI
/// itself generates (dates, model names, seeds), not a general-purpose
/// encoder. Avoids pulling in a whole URL-encoding crate for three fields.
fn url_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Renders `filename_pattern`'s two placeholders and appends `source_filename`'s
/// own extension (ComfyUI's own choice for the format it actually saved in --
/// assuming PNG would be wrong for a workflow configured to save JPEG/WEBP).
/// The stem is sanitized: a pattern or id containing `/` must not escape the
/// output dir.
pub fn render_filename(pattern: &str, session_id: &str, source_filename: &str) -> String {
    let ext = Path::new(source_filename)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .unwrap_or("png");
    let stem = pattern.replace("{session}", session_id).replace(
        "{timestamp}",
        &chrono::Local::now().format("%Y%m%d-%H%M%S").to_string(),
    );
    format!("{}.{ext}", sanitize_filename(&stem, "image"))
}

/// Saves `image` under `cfg.output_dir`/`render_filename(...)`, creating the
/// directory if it doesn't exist yet, and returns the full path. Never
/// overwrites: a taken name gets `-2`, `-3`, ... before the extension
/// (`create_new` makes the check-and-claim atomic).
pub fn save_generated_image(
    cfg: &ComfyUiConfig,
    session_id: &str,
    image: &GeneratedImage,
) -> anyhow::Result<PathBuf> {
    use std::io::Write;
    let dir = PathBuf::from(&cfg.output_dir);
    std::fs::create_dir_all(&dir)?;
    let name = render_filename(&cfg.filename_pattern, session_id, &image.filename);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_string(), ext.to_string()),
        None => (name.clone(), "png".to_string()),
    };
    for n in 1.. {
        let candidate = if n == 1 {
            dir.join(&name)
        } else {
            dir.join(format!("{stem}-{n}.{ext}"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                file.write_all(&image.bytes)?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!()
}

const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "gif"];

/// The only paths a client may ask the app to read back or copy out: a real
/// image file inside `output_dir`, after resolving symlinks and `..`.
fn validate_under(path: &Path, root: &Path) -> anyhow::Result<PathBuf> {
    let root = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("can't resolve the image output folder: {e}"))?;
    let resolved = path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("can't resolve image path: {e}"))?;
    if !resolved.starts_with(&root) {
        anyhow::bail!("image path is outside the configured output folder");
    }
    let is_image = resolved
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
    if !resolved.is_file() || !is_image {
        anyhow::bail!("not an image file");
    }
    Ok(resolved)
}

/// See `validate_under`; the root is the configured `ComfyUiConfig.output_dir`.
pub fn validate_generated_path(path: &Path) -> anyhow::Result<PathBuf> {
    let cfg = load_or_init()?;
    validate_under(path, Path::new(&cfg.output_dir))
}

/// Reads an already-saved generated image back as a `data:` URL, for
/// redisplaying one when a chat session is reopened, or right after
/// `save_generated_image` for the live turn that just created it.
pub fn read_as_data_url(path: &Path) -> anyhow::Result<String> {
    let path = validate_generated_path(path)?;
    let bytes = std::fs::read(&path)?;
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/png",
    };
    Ok(format!("data:{mime};base64,{}", STANDARD.encode(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed-down shape of the sample `JANKU v60.json` this feature was
    /// built against -- node 54/55 both have a `prompt` input (positive and
    /// negative), node 52 has `width`/`height`, node 58 has
    /// `cfg`/`steps_total`/`sampler_name`/`scheduler`, node 20 has
    /// `ckpt_name`. Real workflows also have array-reference inputs (node
    /// wiring, e.g. `"opt_model": ["20", 0]`) sitting alongside the literal
    /// ones -- included here too, since `apply_mapping` must never touch
    /// those even if a mapping were (wrongly) pointed at one.
    fn sample_workflow() -> serde_json::Value {
        serde_json::json!({
            "20": { "inputs": { "ckpt_name": "JANKUTrainedNoobaiRouwei_v60.safetensors" }, "class_type": "Checkpoint Loader with Name (Image Saver)" },
            "52": { "inputs": { "width": 1128, "height": 2000, "batch_size": 1 }, "class_type": "EmptyLatentImage" },
            "54": { "inputs": { "prompt": "asuna \\(blue archive\\), masterpiece, ", "opt_model": ["20", 0] }, "class_type": "Power Prompt (rgthree)" },
            "55": { "inputs": { "prompt": "embedding:lazyhand_1450646" }, "class_type": "Power Prompt (rgthree)" },
            "58": { "inputs": { "steps_total": 35, "cfg": 5, "sampler_name": "euler_ancestral", "scheduler": "sgm_uniform" }, "class_type": "KSampler Config (rgthree)" }
        })
    }

    fn sample_mapping() -> ComfyUiMapping {
        ComfyUiMapping {
            checkpoint: "20.ckpt_name".to_string(),
            positive: "54.prompt".to_string(),
            negative: "55.prompt".to_string(),
            width: "52.width".to_string(),
            height: "52.height".to_string(),
            sampler: "58.sampler_name".to_string(),
            scheduler: "58.scheduler".to_string(),
            cfg: "58.cfg".to_string(),
            steps: "58.steps_total".to_string(),
        }
    }

    #[test]
    fn apply_mapping_overrides_only_the_fields_actually_provided() {
        let mut workflow = sample_workflow();
        let mapping = sample_mapping();
        let fields = ImagePromptFields {
            positive: Some("a red circle".to_string()),
            width: Some(512),
            ..Default::default()
        };
        apply_mapping(&mut workflow, &mapping, &fields).unwrap();

        assert_eq!(workflow["54"]["inputs"]["prompt"], "a red circle");
        assert_eq!(workflow["52"]["inputs"]["width"], 512);
        // Untouched -- not in `fields`.
        assert_eq!(
            workflow["55"]["inputs"]["prompt"],
            "embedding:lazyhand_1450646"
        );
        assert_eq!(workflow["58"]["inputs"]["cfg"], 5);
        assert_eq!(
            workflow["20"]["inputs"]["ckpt_name"],
            "JANKUTrainedNoobaiRouwei_v60.safetensors"
        );
        // The node-wiring reference beside the mapped field must survive.
        assert_eq!(workflow["54"]["inputs"]["opt_model"][0], "20");
    }

    #[test]
    fn apply_mapping_leaves_the_workflow_untouched_when_nothing_is_requested() {
        let mut workflow = sample_workflow();
        let original = workflow.clone();
        apply_mapping(
            &mut workflow,
            &sample_mapping(),
            &ImagePromptFields::default(),
        )
        .unwrap();
        assert_eq!(workflow, original);
    }

    #[test]
    fn apply_mapping_errors_on_a_node_id_that_does_not_exist() {
        let mut workflow = sample_workflow();
        let mut mapping = sample_mapping();
        mapping.positive = "999.prompt".to_string();
        let fields = ImagePromptFields {
            positive: Some("x".to_string()),
            ..Default::default()
        };
        assert!(apply_mapping(&mut workflow, &mapping, &fields).is_err());
    }

    #[test]
    fn apply_mapping_ignores_an_unmapped_blank_field() {
        let mut workflow = sample_workflow();
        let mut mapping = sample_mapping();
        mapping.negative = String::new(); // deliberately unmapped
        let fields = ImagePromptFields {
            negative: Some("should be ignored".to_string()),
            ..Default::default()
        };
        apply_mapping(&mut workflow, &mapping, &fields).unwrap();
        assert_eq!(
            workflow["55"]["inputs"]["prompt"],
            "embedding:lazyhand_1450646"
        );
    }

    #[test]
    fn render_filename_substitutes_both_placeholders_and_keeps_the_source_extension() {
        let name = render_filename(
            "{session}-{timestamp}",
            "session-20260902-010101",
            "ComfyUI_00001_.png",
        );
        assert!(name.starts_with("session-20260902-010101-"));
        assert!(name.ends_with(".png"));
    }

    #[test]
    fn render_filename_falls_back_to_png_without_a_source_extension() {
        let name = render_filename("{session}", "sess", "no-extension-filename");
        assert!(name.ends_with(".png"));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("llm-assistant-comfy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn validate_under_accepts_images_inside_and_rejects_the_rest() {
        let dir = scratch("validate");
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let ok = out.join("a.PNG");
        std::fs::write(&ok, b"x").unwrap();
        let txt = out.join("a.txt");
        std::fs::write(&txt, b"x").unwrap();
        let outside = dir.join("secret.png");
        std::fs::write(&outside, b"x").unwrap();

        assert!(validate_under(&ok, &out).is_ok());
        assert!(validate_under(&txt, &out).is_err(), "non-image extension");
        assert!(validate_under(&outside, &out).is_err(), "outside the dir");
        let traversal = out.join("..").join("secret.png");
        assert!(validate_under(&traversal, &out).is_err(), "traversal");
        assert!(validate_under(&out.join("missing.png"), &out).is_err());
        assert!(validate_under(&ok, &dir.join("no-such-root")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_the_same_name_twice_never_overwrites() {
        let dir = scratch("collision");
        let cfg = ComfyUiConfig {
            output_dir: dir.display().to_string(),
            filename_pattern: "{session}".to_string(),
            ..Default::default()
        };
        let image = |b: &[u8]| GeneratedImage {
            bytes: b.to_vec(),
            filename: "ComfyUI_1.png".to_string(),
        };
        let a = save_generated_image(&cfg, "sess", &image(b"a")).unwrap();
        let b = save_generated_image(&cfg, "sess", &image(b"b")).unwrap();
        let c = save_generated_image(&cfg, "sess", &image(b"c")).unwrap();
        assert_eq!(a.file_name().unwrap(), "sess.png");
        assert_eq!(b.file_name().unwrap(), "sess-2.png");
        assert_eq!(c.file_name().unwrap(), "sess-3.png");
        assert_eq!(std::fs::read(&a).unwrap(), b"a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_filename_cannot_escape_the_output_dir() {
        let name = render_filename("{session}", "../../x", "a.png");
        assert!(!name.contains('/') && !name.starts_with('.'), "{name}");
        let name = render_filename(".hidden", "s", "a.png");
        assert!(!name.starts_with('.'));
    }

    #[test]
    fn find_first_image_skips_imageless_nodes_and_prefers_output() {
        let entry = serde_json::json!({ "outputs": {
            "1": { "text": ["hi"] },
            "2": { "images": [] },
            "3": { "images": [{ "filename": "t.png", "type": "temp" }] },
            "4": { "images": [{ "filename": "o.png", "subfolder": "s", "type": "output" }] },
        }});
        assert_eq!(
            find_first_image(&entry),
            Some(("s".into(), "o.png".into(), "output".into()))
        );
        let temp_only = serde_json::json!({ "outputs": {
            "1": { "images": [{ "filename": "t.png", "type": "temp" }] } }});
        assert_eq!(find_first_image(&temp_only).unwrap().1, "t.png");
        assert!(find_first_image(&serde_json::json!({ "outputs": {} })).is_none());
    }

    #[test]
    fn poll_failure_reports_errors_and_empty_completions() {
        let err = serde_json::json!({ "status": { "status_str": "error", "messages": [
            ["execution_start", {}],
            ["execution_error", { "exception_message": "Out of memory", "node_type": "KSampler" }]
        ]}});
        assert_eq!(poll_failure(&err).unwrap(), "Out of memory (node KSampler)");
        let bare = serde_json::json!({ "status": { "status_str": "error" } });
        assert!(poll_failure(&bare).is_some());
        let empty = serde_json::json!({ "status": { "status_str": "success", "completed": true } });
        assert!(poll_failure(&empty).is_some());
        let running =
            serde_json::json!({ "status": { "status_str": "success", "completed": false } });
        assert!(poll_failure(&running).is_none());
        assert!(poll_failure(&serde_json::json!({})).is_none());
    }

    #[test]
    fn reaction_mode_wire_values_match_the_settings_dropdown() {
        // The Settings UI's <select> option values are literal "always" /
        // "optional" / "none" -- a mismatch here would silently fail to
        // deserialize a saved config instead of loudly erroring.
        assert_eq!(
            serde_json::from_str::<ReactionMode>("\"always\"").unwrap(),
            ReactionMode::Always
        );
        assert_eq!(
            serde_json::from_str::<ReactionMode>("\"optional\"").unwrap(),
            ReactionMode::Optional
        );
        assert_eq!(
            serde_json::from_str::<ReactionMode>("\"none\"").unwrap(),
            ReactionMode::Never
        );
        assert_eq!(
            serde_json::to_string(&ReactionMode::Never).unwrap(),
            "\"none\""
        );
    }
}
