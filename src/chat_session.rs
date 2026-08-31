//! Chat mode's persistent conversations: `<app-config-dir>/chat/sessions/
//! session-<timestamp>/`, one directory per session, holding `meta.json`
//! (title, persona, timestamps), `history.json` (the conversation), and
//! `state.md` (the persona's session-scoped, model-writable persistent
//! state -- see `rules::CHAT_PROTOCOL_PROMPT`).
//!
//! Deliberately **not** rotated or capped the way `chat_log`/`memory`
//! sessions are -- the whole point is a session list the user manages
//! themselves (rename, delete), not one the app prunes for them.
//!
//! Unlike `memory.rs`'s intent/original-state/progress/completed ledger,
//! this isn't a task record: chat mode has no tasks and no commands that
//! ran, so there's nothing to archive on a boundary. A session is just one
//! continuous conversation until the user deletes it.

use crate::llm::ChatMessage;
use crate::paths::app_config_dir;
use std::fs;
use std::path::PathBuf;

const META_FILE: &str = "meta.json";
const HISTORY_FILE: &str = "history.json";
const STATE_FILE: &str = "state.md";

/// Title a freshly created session starts with, and the signal
/// `save_history` uses to decide whether it's still safe to auto-title from
/// the first message -- once a user renames a session away from this, their
/// title wins permanently.
pub const DEFAULT_TITLE: &str = "New chat";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionMeta {
    pub title: String,
    pub persona: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub persona: Option<String>,
    pub updated_at: String,
}

// See the identical fix/note in `memory.rs` and `persona.rs`: `XDG_CONFIG_HOME`
// is process-wide, not thread-local, so it can't isolate parallel tests.
#[cfg(test)]
thread_local! {
    static TEST_SESSIONS_ROOT: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

fn sessions_root() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = TEST_SESSIONS_ROOT.with(|d| d.borrow().clone()) {
        return dir;
    }
    app_config_dir().join("chat").join("sessions")
}

fn session_dir(id: &str) -> PathBuf {
    sessions_root().join(id)
}

fn is_session_dir(path: &std::path::Path) -> bool {
    path.is_dir()
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("session-"))
            .unwrap_or(false)
}

fn read_meta(id: &str) -> anyhow::Result<SessionMeta> {
    let text = fs::read_to_string(session_dir(id).join(META_FILE))?;
    Ok(serde_json::from_str(&text)?)
}

fn write_meta(id: &str, meta: &SessionMeta) -> anyhow::Result<()> {
    fs::write(
        session_dir(id).join(META_FILE),
        serde_json::to_string_pretty(meta)?,
    )?;
    Ok(())
}

pub fn list_sessions() -> anyhow::Result<Vec<SessionSummary>> {
    let root = sessions_root();
    fs::create_dir_all(&root)?;
    let mut sessions: Vec<SessionSummary> = fs::read_dir(&root)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_session_dir(p))
        .filter_map(|p| {
            let id = p.file_name()?.to_str()?.to_string();
            let meta = read_meta(&id).ok()?;
            Some(SessionSummary {
                id,
                title: meta.title,
                persona: meta.persona,
                updated_at: meta.updated_at,
            })
        })
        .collect();
    // Most recently active first -- RFC3339 timestamps sort lexically.
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}

/// Timestamp-suffixed like every other session id in this app, with a
/// numeric fallback if two are created within the same second (plausible
/// here -- unlike `memory`/`chat_log` sessions, one per process launch,
/// this is a button the user can click twice quickly).
fn new_session_id() -> String {
    let base = format!("session-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    if !session_dir(&base).exists() {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !session_dir(&candidate).exists() {
            return candidate;
        }
    }
    unreachable!()
}

pub fn create_session(persona: Option<&str>) -> anyhow::Result<SessionSummary> {
    let id = new_session_id();
    fs::create_dir_all(session_dir(&id))?;
    let now = chrono::Local::now().to_rfc3339();
    let meta = SessionMeta {
        title: DEFAULT_TITLE.to_string(),
        persona: persona.map(|p| p.to_string()),
        created_at: now.clone(),
        updated_at: now,
    };
    write_meta(&id, &meta)?;
    fs::write(session_dir(&id).join(HISTORY_FILE), "[]")?;
    fs::write(session_dir(&id).join(STATE_FILE), "")?;
    Ok(SessionSummary {
        id,
        title: meta.title,
        persona: meta.persona,
        updated_at: meta.updated_at,
    })
}

pub fn load_session(id: &str) -> anyhow::Result<(SessionMeta, Vec<ChatMessage>)> {
    let meta = read_meta(id)?;
    let history_text = fs::read_to_string(session_dir(id).join(HISTORY_FILE))?;
    let history: Vec<ChatMessage> = serde_json::from_str(&history_text)?;
    Ok((meta, history))
}

/// Persists the full history and bumps `updated_at`. `title_hint`, when the
/// session's title is still `DEFAULT_TITLE`, becomes the new title -- once a
/// user renames it away from that, this never overwrites their choice again.
pub fn save_history(
    id: &str,
    history: &[ChatMessage],
    title_hint: Option<&str>,
) -> anyhow::Result<()> {
    fs::write(
        session_dir(id).join(HISTORY_FILE),
        serde_json::to_string(history)?,
    )?;
    let mut meta = read_meta(id)?;
    if meta.title == DEFAULT_TITLE {
        if let Some(hint) = title_hint {
            meta.title = hint.to_string();
        }
    }
    meta.updated_at = chrono::Local::now().to_rfc3339();
    write_meta(id, &meta)?;
    Ok(())
}

pub fn read_state(id: &str) -> String {
    fs::read_to_string(session_dir(id).join(STATE_FILE)).unwrap_or_default()
}

pub fn update_state(id: &str, content: &str) -> anyhow::Result<()> {
    fs::write(session_dir(id).join(STATE_FILE), content)?;
    Ok(())
}

pub fn rename_session(id: &str, title: &str) -> anyhow::Result<()> {
    let mut meta = read_meta(id)?;
    meta.title = title.to_string();
    write_meta(id, &meta)
}

pub fn delete_session(id: &str) -> anyhow::Result<()> {
    fs::remove_dir_all(session_dir(id))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread::sleep, time::Duration};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-assistant-chat-session-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        TEST_SESSIONS_ROOT.with(|d| *d.borrow_mut() = Some(dir.join("chat/sessions")));
        dir
    }

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage::text(role, content)
    }

    #[test]
    fn create_then_list_finds_it() {
        let dir = scratch("create-list");
        let created = create_session(Some("Aria")).unwrap();
        let listed = list_sessions().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].title, DEFAULT_TITLE);
        assert_eq!(listed[0].persona.as_deref(), Some("Aria"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_round_trips() {
        let dir = scratch("history");
        let session = create_session(None).unwrap();
        let history = vec![msg("user", "hello"), msg("assistant", "hi there")];
        save_history(&session.id, &history, Some("hello")).unwrap();
        let (meta, loaded) = load_session(&session.id).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].content, "hi there");
        assert_eq!(meta.title, "hello", "empty-title session should auto-title");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_renamed_session_is_never_auto_retitled_again() {
        let dir = scratch("no-retitle");
        let session = create_session(None).unwrap();
        rename_session(&session.id, "My real title").unwrap();
        save_history(&session.id, &[msg("user", "hi")], Some("hi")).unwrap();
        let (meta, _) = load_session(&session.id).unwrap();
        assert_eq!(meta.title, "My real title");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_round_trips_and_overwrites() {
        let dir = scratch("state");
        let session = create_session(None).unwrap();
        assert_eq!(read_state(&session.id), "");
        update_state(&session.id, "HP: 100").unwrap();
        assert_eq!(read_state(&session.id), "HP: 100");
        update_state(&session.id, "HP: 80").unwrap();
        assert_eq!(
            read_state(&session.id),
            "HP: 80",
            "state.md holds the current snapshot, not a growing log"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_it_from_the_list() {
        let dir = scratch("delete");
        let session = create_session(None).unwrap();
        delete_session(&session.id).unwrap();
        assert!(list_sessions().unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_is_most_recently_updated_first() {
        let dir = scratch("ordering");
        let first = create_session(None).unwrap();
        sleep(Duration::from_millis(5));
        let second = create_session(None).unwrap();
        sleep(Duration::from_millis(5));
        // Touch the first session again -- it should jump back to the top.
        save_history(&first.id, &[msg("user", "hi")], None).unwrap();

        let listed = list_sessions().unwrap();
        assert_eq!(listed[0].id, first.id);
        assert_eq!(listed[1].id, second.id);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn creating_two_sessions_back_to_back_never_collides() {
        let dir = scratch("collision");
        let a = create_session(None).unwrap();
        let b = create_session(None).unwrap();
        assert_ne!(a.id, b.id);
        assert_eq!(list_sessions().unwrap().len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
