//! On-demand tool/capability rulesets: plain `.md` files under
//! `<app-config-dir>/rulesets/`, freeform, same shape as `persona.rs`. The
//! model is never handed their content by default -- only their names, via
//! `rules::build_ruleset_availability_note` -- and only pulls one in mid
//! conversation by requesting it with a ` ```ruleset <name> ``` ` fence (see
//! `rules::extract_ruleset_request`). Once loaded for a chat session it
//! stays loaded for the rest of it (`chat_session::add_loaded_ruleset`).
//!
//! Two seed files ship so there's always something to request: an
//! image-generation-prompt ruleset (for a future ComfyUI integration -- this
//! app doesn't talk to ComfyUI yet, this is just the doc the model will read
//! once it does) and a free-form "other tools" ruleset (e.g. a SearXNG URL
//! for web browsing, filled in by the user).

use crate::paths::{app_config_dir, sanitize_filename};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize)]
pub struct RulesetSummary {
    pub name: String,
}

// See the identical fix/note in `persona.rs`/`memory.rs`: `XDG_CONFIG_HOME`
// is process-wide, not thread-local, so it can't isolate parallel tests.
#[cfg(test)]
thread_local! {
    static TEST_RULESETS_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

fn rulesets_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = TEST_RULESETS_DIR.with(|d| d.borrow().clone()) {
        return dir;
    }
    app_config_dir().join("rulesets")
}

fn ruleset_path(name: &str) -> PathBuf {
    rulesets_dir().join(format!("{}.md", sanitize_filename(name, "ruleset")))
}

#[cfg(test)]
pub(crate) fn ruleset_path_for_test(name: &str) -> PathBuf {
    ruleset_path(name)
}

pub const SEED_IMAGE_GENERATION_PROMPT: &str = "\
# Image generation prompts

This app doesn't talk to an image generator yet -- this ruleset is a
placeholder for when it does (planned: ComfyUI's API). Once that lands,
follow whatever prompt-format rules end up here before requesting an
image generation.

For now, if asked to generate an image, say the feature isn't wired up yet
instead of describing an image in prose as if it were generated.
";

pub const SEED_OTHER_TOOLS: &str = "\
# Other tools

Free-form notes about extra tools/services available outside this app's
built-in commands. Fill this in yourself -- for example:

- Web search: a SearXNG instance at <URL not set> -- describe how to query
  it (e.g. `<url>/search?q=...&format=json`) once you have one running.

Nothing here is wired into the app automatically; it's reference material
for you to point the model at.
";

pub fn list_rulesets() -> anyhow::Result<Vec<RulesetSummary>> {
    let dir = rulesets_dir();
    fs::create_dir_all(&dir)?;
    let image_gen = ruleset_path("image-generation-prompt");
    if !image_gen.exists() {
        fs::write(&image_gen, SEED_IMAGE_GENERATION_PROMPT)?;
    }
    let other_tools = ruleset_path("other-tools");
    if !other_tools.exists() {
        fs::write(&other_tools, SEED_OTHER_TOOLS)?;
    }

    let mut names: Vec<String> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect();
    names.sort();
    Ok(names
        .into_iter()
        .map(|name| RulesetSummary { name })
        .collect())
}

pub fn load_ruleset(name: &str) -> anyhow::Result<String> {
    Ok(fs::read_to_string(ruleset_path(name))?)
}

/// Overwrites an existing ruleset's content -- the ruleset editor's save,
/// same reasoning as `persona::update_persona`. Errors if `name` doesn't
/// match an existing ruleset rather than creating one, since the editor
/// only ever offers names `list_rulesets` already returned.
pub fn update_ruleset(name: &str, content: &str) -> anyhow::Result<()> {
    let dest = ruleset_path(name);
    if !dest.exists() {
        anyhow::bail!("no ruleset named \"{name}\" to edit");
    }
    fs::write(&dest, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-assistant-ruleset-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        TEST_RULESETS_DIR.with(|d| *d.borrow_mut() = Some(dir.join("rulesets")));
        dir
    }

    #[test]
    fn list_seeds_the_two_default_rulesets_on_first_call() {
        let dir = scratch("seed");
        let names: Vec<String> = list_rulesets()
            .unwrap()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "image-generation-prompt".to_string(),
                "other-tools".to_string()
            ]
        );
        assert_eq!(
            load_ruleset("image-generation-prompt").unwrap(),
            SEED_IMAGE_GENERATION_PROMPT
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reads_back_seeded_content() {
        let dir = scratch("load");
        list_rulesets().unwrap(); // seeds the two default files
        assert_eq!(load_ruleset("other-tools").unwrap(), SEED_OTHER_TOOLS);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_overwrites_a_seeded_rulesets_content() {
        let dir = scratch("update");
        list_rulesets().unwrap(); // seeds the two default files
        update_ruleset("other-tools", "SearXNG URL: http://localhost:8080").unwrap();
        assert_eq!(
            load_ruleset("other-tools").unwrap(),
            "SearXNG URL: http://localhost:8080"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_refuses_a_name_that_does_not_exist() {
        let dir = scratch("update-missing");
        assert!(update_ruleset("ghost", "x").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_with_path_separators_cannot_escape_the_rulesets_dir() {
        let dir = scratch("traversal");
        let path = ruleset_path_for_test("../../evil");
        assert_eq!(
            path.parent().unwrap(),
            rulesets_dir(),
            "the file must land inside the rulesets dir, not escape it: {path:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
