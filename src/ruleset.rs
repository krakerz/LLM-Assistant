//! The two user-editable preference texts, plain `.md` files under
//! `<app-config-dir>/rulesets/` (same shape as `persona.rs`):
//!
//! - `image-generation-prompt`: style rules for the image prompt builder,
//!   passed to it verbatim (`chat_director::build_image_prompt`).
//! - `web-search`: content policy for the search answer
//!   (`chat_turn::web_search`).
//!
//! Both are always used; there is no on-demand loading. They ship blank: the
//! editor's "see an example" popup (`example_for`) shows what a file could
//! contain without ever writing it to disk, so an empty file stays a
//! deliberate state.

use crate::paths::{app_config_dir, sanitize_filename};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize)]
pub struct RulesetSummary {
    pub name: String,
}

// `XDG_CONFIG_HOME` is process-wide, so tests isolate through this
// thread-local instead.
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

/// Style rules for the image prompt builder.
pub const IMAGE_GENERATION_RULESET_NAME: &str = "image-generation-prompt";

/// Shown by the editor's "see an example" popup; never written to disk.
pub const IMAGE_GENERATION_EXAMPLE: &str = "\
- Always start `positive` with: masterpiece, best quality
- Always include in `negative`: bad hands, blurry, watermark
- Always use `checkpoint`: my_favorite_model.safetensors
- Always use `width`: 832
- Always use `height`: 1216
- Always use `sampler`: euler
- Always use `scheduler`: normal
- Always use `cfg`: 7
- Always use `steps`: 30

Any field left out keeps whatever value the workflow already has \
configured for it -- only include the ones you actually want to fix.";

/// Content policy for the search answer: what a search should never return.
pub const WEB_SEARCH_RULESET_NAME: &str = "web-search";

/// See `IMAGE_GENERATION_EXAMPLE`.
pub const WEB_SEARCH_EXAMPLE: &str = "\
- Never return NSFW content in a search result or summary, even if the search itself turns some up
- Don't search for or discuss political topics -- decline and steer the conversation elsewhere instead
- Prefer summarizing search results in your own words over quoting them at length";

/// Example content for the ruleset editor's "see an example" popup, if this
/// ruleset has one -- `None` means that link/button stays hidden for it.
pub fn example_for(name: &str) -> Option<&'static str> {
    match name {
        IMAGE_GENERATION_RULESET_NAME => Some(IMAGE_GENERATION_EXAMPLE),
        WEB_SEARCH_RULESET_NAME => Some(WEB_SEARCH_EXAMPLE),
        _ => None,
    }
}

pub fn list_rulesets() -> anyhow::Result<Vec<RulesetSummary>> {
    let dir = rulesets_dir();
    fs::create_dir_all(&dir)?;
    let image_gen = ruleset_path(IMAGE_GENERATION_RULESET_NAME);
    if !image_gen.exists() {
        fs::write(&image_gen, "")?;
    }
    let web_search = ruleset_path(WEB_SEARCH_RULESET_NAME);
    if !web_search.exists() {
        fs::write(&web_search, "")?;
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
    crate::paths::write_atomic(&dest, content.as_bytes(), None)?;
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
                "web-search".to_string()
            ]
        );
        // Blank by default; the example popup documents what a file could hold.
        assert_eq!(load_ruleset("image-generation-prompt").unwrap(), "");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn example_for_is_offered_for_both_built_in_rulesets_only() {
        assert_eq!(
            example_for(IMAGE_GENERATION_RULESET_NAME),
            Some(IMAGE_GENERATION_EXAMPLE)
        );
        assert_eq!(
            example_for(WEB_SEARCH_RULESET_NAME),
            Some(WEB_SEARCH_EXAMPLE)
        );
        assert_eq!(example_for("some-made-up-name"), None);
    }

    #[test]
    fn load_reads_back_seeded_content() {
        let dir = scratch("load");
        list_rulesets().unwrap(); // seeds the two default files -- both blank
        assert_eq!(load_ruleset("web-search").unwrap(), "");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_overwrites_a_seeded_rulesets_content() {
        let dir = scratch("update");
        list_rulesets().unwrap(); // seeds the two default files
        update_ruleset("web-search", "Never return NSFW results").unwrap();
        assert_eq!(
            load_ruleset("web-search").unwrap(),
            "Never return NSFW results"
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
