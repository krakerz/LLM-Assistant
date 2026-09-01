//! ComfyUI image generation: a pasted API-format workflow (exported from
//! ComfyUI's own UI) plus a manual field mapping, the same pattern Open
//! WebUI's ComfyUI integration uses -- everything the workflow doesn't need
//! touched (LoRA nodes, seed handling, the Image Saver node) stays exactly
//! as the user set it up; only the handful of mapped fields get overwritten
//! per-request from the model's ` ```image-prompt``` ` block (see
//! `rules::extract_image_prompt_request`).
//!
//! Stored separately from `config.toml` (`<app-config-dir>/comfyui.json`)
//! since the pasted workflow JSON can be several KB -- same reasoning
//! personas/rulesets live in their own files instead of being crammed into
//! one config blob.

use crate::paths::app_config_dir;
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
        }
    }
}

fn config_path() -> PathBuf {
    app_config_dir().join("comfyui.json")
}

pub fn load_or_init() -> anyhow::Result<ComfyUiConfig> {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(_) => {
            let cfg = ComfyUiConfig::default();
            save(&cfg)?;
            Ok(cfg)
        }
    }
}

pub fn save(cfg: &ComfyUiConfig) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

/// One image request's optional overrides, straight from a
/// ` ```image-prompt``` ` block (see `rules::extract_image_prompt_request`).
/// Anything left `None` keeps whatever value is already in the pasted
/// workflow JSON for that mapped field.
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

/// The `` ```image-prompt``` `` fence's actual mechanics -- injected by
/// `rules::build_dispatch_system_content` whenever the
/// `ruleset::IMAGE_GENERATION_RULESET_NAME` ruleset is loaded, independent
/// of that ruleset file's own content. See `ruleset::IMAGE_GENERATION_RULESET_NAME`'s
/// doc comment for why this lives in the app rather than the file.
pub const IMAGE_PROMPT_PROTOCOL: &str = "\
Request an image with a fenced block on its own, one `key: value` pair per line, only the fields \
you actually want to set:

```image-prompt
positive: a description of what the image should contain
negative: things to avoid, if the workflow uses a negative prompt
```

All fields are optional and independent -- anything you leave out keeps whatever value is already \
configured in the app's saved ComfyUI workflow, it does not get cleared. The recognized keys are \
`positive`, `negative`, `width`/`height` (whole numbers), `steps` (a whole number), `cfg` (a \
number, may have a decimal), `sampler`/`scheduler` (must match a value the configured workflow \
actually supports -- when unsure, omit and let the workflow's own default apply), and \
`checkpoint` (a model/checkpoint filename). A field only does something if it's been mapped in \
Settings' \"Image Gen\" tab -- an unmapped field here is just ignored, not an error. If nothing is \
configured yet, the app will say so plainly rather than silently doing nothing.";

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

/// One image downloaded from ComfyUI. `filename` is ComfyUI's own name
/// (extension included) for the saved output -- `render_filename` reuses
/// its extension rather than assuming PNG.
pub struct GeneratedImage {
    pub bytes: Vec<u8>,
    pub filename: String,
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
    let client = reqwest::Client::new();
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

/// Scans a `/history/{prompt_id}` entry's `outputs` for the first node with
/// an `images` array, returning `(subfolder, filename, type)` for the first
/// image -- matches the single-image-output shape every ComfyUI workflow
/// (including the sample one this feature was built against) produces.
fn find_first_image(history_entry: &serde_json::Value) -> Option<(String, String, String)> {
    let outputs = history_entry.get("outputs")?.as_object()?;
    for node_output in outputs.values() {
        let images = node_output.get("images")?.as_array()?;
        let Some(first) = images.first() else {
            continue;
        };
        let filename = first.get("filename")?.as_str()?.to_string();
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
        return Some((subfolder, filename, file_type));
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
pub fn render_filename(pattern: &str, session_id: &str, source_filename: &str) -> String {
    let ext = Path::new(source_filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png");
    let stem = pattern.replace("{session}", session_id).replace(
        "{timestamp}",
        &chrono::Local::now().format("%Y%m%d-%H%M%S").to_string(),
    );
    format!("{stem}.{ext}")
}

/// Saves `image` under `cfg.output_dir`/`render_filename(...)`, creating the
/// directory if it doesn't exist yet, and returns the full path.
pub fn save_generated_image(
    cfg: &ComfyUiConfig,
    session_id: &str,
    image: &GeneratedImage,
) -> anyhow::Result<PathBuf> {
    let dir = PathBuf::from(&cfg.output_dir);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(render_filename(
        &cfg.filename_pattern,
        session_id,
        &image.filename,
    ));
    std::fs::write(&path, &image.bytes)?;
    Ok(path)
}

/// Reads an already-saved generated image back as a `data:` URL, for
/// redisplaying one when a chat session is reopened, or right after
/// `save_generated_image` for the live turn that just created it.
pub fn read_as_data_url(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    let mime = match path.extension().and_then(|e| e.to_str()) {
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
}
