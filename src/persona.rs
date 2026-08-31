//! Character sheets for chat mode: plain `.md` files under
//! `<app-config-dir>/personas/`, freeform -- the whole file's content is the
//! persona's system-prompt material, the filename (minus `.md`) is its
//! display name. No schema, so an existing character card can be dropped in
//! as-is.
//!
//! Deliberately not soft-deleted into a trash directory the way operation
//! mode's sandboxed files are: these are small, hand-authored text files
//! outside any working folder, not something a model can destroy by
//! surprise. If that turns out wrong in practice, add it then.

use crate::paths::app_config_dir;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize)]
pub struct PersonaSummary {
    pub name: String,
}

// `XDG_CONFIG_HOME` is a process-wide env var, not thread-local, so it can't
// isolate parallel tests (see the identical fix and note in `memory.rs`).
// This override lets each test point at its own scratch directory instead.
#[cfg(test)]
thread_local! {
    static TEST_PERSONAS_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

fn personas_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = TEST_PERSONAS_DIR.with(|d| d.borrow().clone()) {
        return dir;
    }
    app_config_dir().join("personas")
}

/// Keeps a persona name safe as a bare filename: no path separators, no
/// leading dot (would make it a hidden file, and `..` a traversal), never
/// empty. Applied to both imported filenames and user-typed names.
fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| if c == '/' || c == '\\' { '-' } else { c })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').trim();
    if cleaned.is_empty() {
        "persona".to_string()
    } else {
        cleaned.to_string()
    }
}

fn persona_path(name: &str) -> PathBuf {
    personas_dir().join(format!("{}.md", sanitize_name(name)))
}

#[cfg(test)]
pub(crate) fn persona_path_for_test(name: &str) -> PathBuf {
    persona_path(name)
}

pub fn list_personas() -> anyhow::Result<Vec<PersonaSummary>> {
    let dir = personas_dir();
    fs::create_dir_all(&dir)?;
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
        .map(|name| PersonaSummary { name })
        .collect())
}

pub fn load_persona(name: &str) -> anyhow::Result<String> {
    Ok(fs::read_to_string(persona_path(name))?)
}

/// Copies a file the user picked (native file dialog, filtered to `.md`)
/// into the personas directory, using its own filename as the persona name.
/// Refuses to clobber an existing persona of the same name -- the caller
/// can rename the source file and retry, rather than this silently losing
/// someone's existing character sheet.
pub fn import_persona(source: &Path) -> anyhow::Result<PersonaSummary> {
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("couldn't read a filename from {source:?}"))?;
    let name = sanitize_name(stem);
    let dest = persona_path(&name);
    if dest.exists() {
        anyhow::bail!("a persona named \"{name}\" already exists");
    }
    fs::create_dir_all(personas_dir())?;
    fs::copy(source, &dest)?;
    Ok(PersonaSummary { name })
}

/// Writes a persona authored directly in the app (the "New persona" dialog:
/// a name field plus a textarea). Same no-clobber rule as `import_persona`.
pub fn save_new_persona(name: &str, content: &str) -> anyhow::Result<PersonaSummary> {
    let name = sanitize_name(name);
    let dest = persona_path(&name);
    if dest.exists() {
        anyhow::bail!("a persona named \"{name}\" already exists");
    }
    fs::create_dir_all(personas_dir())?;
    fs::write(&dest, content)?;
    Ok(PersonaSummary { name })
}

pub fn delete_persona(name: &str) -> anyhow::Result<()> {
    fs::remove_file(persona_path(name))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-assistant-persona-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        TEST_PERSONAS_DIR.with(|d| *d.borrow_mut() = Some(dir.join("personas")));
        dir
    }

    #[test]
    fn list_is_empty_before_anything_is_added() {
        let dir = scratch("empty");
        assert!(list_personas().unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = scratch("roundtrip");
        save_new_persona("Aria", "# Aria\nA cheerful shopkeeper.").unwrap();
        assert_eq!(
            load_persona("Aria").unwrap(),
            "# Aria\nA cheerful shopkeeper."
        );
        let names: Vec<String> = list_personas()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["Aria".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_refuses_to_clobber_an_existing_persona() {
        let dir = scratch("no-clobber");
        save_new_persona("Aria", "first").unwrap();
        assert!(save_new_persona("Aria", "second").is_err());
        assert_eq!(load_persona("Aria").unwrap(), "first");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_uses_the_source_filename_as_the_name() {
        let dir = scratch("import");
        let source = dir.join("Bandit Leader.md");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&source, "A gruff bandit leader.").unwrap();
        let summary = import_persona(&source).unwrap();
        assert_eq!(summary.name, "Bandit Leader");
        assert_eq!(
            load_persona("Bandit Leader").unwrap(),
            "A gruff bandit leader."
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_it_from_the_list() {
        let dir = scratch("delete");
        save_new_persona("Temp", "x").unwrap();
        delete_persona("Temp").unwrap();
        assert!(list_personas().unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_with_path_separators_cannot_escape_the_personas_dir() {
        let dir = scratch("traversal");
        let summary = save_new_persona("../../evil", "x").unwrap();
        let path = persona_path_for_test(&summary.name);
        assert_eq!(
            path.parent().unwrap(),
            personas_dir(),
            "the file must land inside the personas dir, not escape it: {path:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
