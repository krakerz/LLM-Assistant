//! Chat mode's persistent conversations: `<app-config-dir>/chat/sessions/
//! session-<timestamp>/`, one directory per session, holding `meta.json`
//! (title, persona, timestamps), `history.json` (the conversation, every
//! message carrying a stable id), `state.json` (the character sheet as raw
//! JSON, the source of truth), `state.md` (compact prose derived from it, fed
//! to every in-character call) and `state.stamp` (the assistant message id
//! the current state derived from).
//!
//! The backend is the only owner of history. Every read-modify-write of those
//! files happens under the per-session `lock`; long LLM calls run outside it
//! and only their final write is locked. Not rotated or capped: the session
//! list is the user's to manage.

use crate::llm::ChatMessage;
use crate::paths::{app_config_dir, write_atomic};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

type LockMap = Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

const META_FILE: &str = "meta.json";
const HISTORY_FILE: &str = "history.json";
const STATE_FILE: &str = "state.md";
const RAW_STATE_FILE: &str = "state.json";
const STATE_STAMP_FILE: &str = "state.stamp";

/// Title a fresh session starts with; `save_history` only auto-titles while
/// it's still this, so a user's rename wins permanently.
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

// `XDG_CONFIG_HOME` is process-wide, so tests isolate through this
// thread-local instead (async tests must stay on the current-thread runtime).
#[cfg(test)]
thread_local! {
    static TEST_SESSIONS_ROOT: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

fn lock_for(map: &'static OnceLock<LockMap>, id: &str) -> Arc<tokio::sync::Mutex<()>> {
    let map = map.get_or_init(Default::default);
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(id.to_string()).or_default().clone()
}

static HISTORY_LOCKS: OnceLock<LockMap> = OnceLock::new();
static STATE_LOCKS: OnceLock<LockMap> = OnceLock::new();

/// Held across every read-modify-write of a session's files. Not reentrant:
/// the helpers below that say "locks internally" must not be called while
/// holding it.
pub async fn lock(id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    lock_for(&HISTORY_LOCKS, id).lock_owned().await
}

/// Serializes state updates (which span a long LLM call) so each one starts
/// from the previous one's result. Always taken before `lock`, never after.
pub async fn state_queue(id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    lock_for(&STATE_LOCKS, id).lock_owned().await
}

fn forget_locks(id: &str) {
    for map in [&HISTORY_LOCKS, &STATE_LOCKS] {
        if let Some(map) = map.get() {
            map.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        }
    }
}

/// Points this thread's session storage at `root` (tests only).
#[cfg(test)]
pub(crate) fn use_test_root(root: PathBuf) {
    TEST_SESSIONS_ROOT.with(|d| *d.borrow_mut() = Some(root));
}

pub(crate) fn sessions_root() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = TEST_SESSIONS_ROOT.with(|d| d.borrow().clone()) {
        return dir;
    }
    app_config_dir().join("chat").join("sessions")
}

/// Exactly the shape `new_session_id` produces:
/// `session-YYYYMMDD-HHMMSS` plus an optional `-<n>` collision suffix. Ids
/// arrive from HTTP clients, and `join` would let an absolute path or `..`
/// escape `sessions_root()`, so this is the one gate every path goes through.
fn is_valid_session_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("session-") else {
        return false;
    };
    let mut parts = rest.split('-');
    let digits = |p: Option<&str>, n: usize| {
        p.is_some_and(|p| p.len() == n && p.bytes().all(|b| b.is_ascii_digit()))
    };
    if !digits(parts.next(), 8) || !digits(parts.next(), 6) {
        return false;
    }
    match parts.next() {
        None => true,
        Some(n) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && parts.next().is_none(),
    }
}

fn session_dir(id: &str) -> anyhow::Result<PathBuf> {
    if !is_valid_session_id(id) {
        anyhow::bail!("invalid session id");
    }
    Ok(sessions_root().join(id))
}

fn is_session_dir(path: &std::path::Path) -> bool {
    path.is_dir()
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_valid_session_id)
}

fn read_session_file(id: &str, name: &str) -> Option<String> {
    fs::read_to_string(session_dir(id).ok()?.join(name)).ok()
}

fn write_session_file(id: &str, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    write_atomic(&session_dir(id)?.join(name), bytes, None)?;
    Ok(())
}

fn read_meta(id: &str) -> anyhow::Result<SessionMeta> {
    let text = fs::read_to_string(session_dir(id)?.join(META_FILE))?;
    Ok(serde_json::from_str(&text)?)
}

fn write_meta(id: &str, meta: &SessionMeta) -> anyhow::Result<()> {
    write_session_file(
        id,
        META_FILE,
        serde_json::to_string_pretty(meta)?.as_bytes(),
    )
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
    free_session_id(base)
}

#[cfg(not(test))]
fn free_session_id(base: String) -> String {
    let taken = |id: &str| sessions_root().join(id).exists();
    if !taken(&base) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// Parallel tests have separate roots but share the process-wide lock and
/// abort maps, so their ids must be unique across roots too.
#[cfg(test)]
fn free_session_id(base: String) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1000);
    format!(
        "{base}-{}",
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

pub fn create_session(persona: Option<&str>) -> anyhow::Result<SessionSummary> {
    let id = new_session_id();
    fs::create_dir_all(session_dir(&id)?)?;
    let now = chrono::Local::now().to_rfc3339();
    let meta = SessionMeta {
        title: DEFAULT_TITLE.to_string(),
        persona: persona.map(|p| p.to_string()),
        created_at: now.clone(),
        updated_at: now,
    };
    write_meta(&id, &meta)?;
    write_session_file(&id, HISTORY_FILE, b"[]")?;
    write_session_file(&id, STATE_FILE, b"")?;
    write_session_file(&id, RAW_STATE_FILE, b"{}")?;
    Ok(SessionSummary {
        id,
        title: meta.title,
        persona: meta.persona,
        updated_at: meta.updated_at,
    })
}

/// Legacy messages (written before ids existed) get a deterministic
/// `legacy-<index>` id, so repeated loads agree until the next save persists it.
fn read_history(id: &str) -> anyhow::Result<Vec<ChatMessage>> {
    let text = fs::read_to_string(session_dir(id)?.join(HISTORY_FILE))?;
    let mut history: Vec<ChatMessage> = serde_json::from_str(&text)?;
    for (i, m) in history.iter_mut().enumerate() {
        if m.id.is_empty() {
            m.id = format!("legacy-{i}");
        }
    }
    Ok(history)
}

pub fn load_session(id: &str) -> anyhow::Result<(SessionMeta, Vec<ChatMessage>)> {
    let meta = read_meta(id)?;
    Ok((meta, read_history(id)?))
}

pub fn load_history(id: &str) -> anyhow::Result<Vec<ChatMessage>> {
    read_history(id)
}

/// The persona name this session was created with (not the card text).
pub fn persona_name(id: &str) -> Option<String> {
    read_meta(id).ok()?.persona
}

/// The persona card text for this session, if it has one that still exists.
pub fn persona_content(id: &str) -> Option<String> {
    let name = read_meta(id).ok()?.persona?;
    crate::persona::load_persona(&name).ok()
}

/// Persists the full history and bumps `updated_at`. The caller holds `lock`.
/// `title_hint`, while the title is still `DEFAULT_TITLE`, becomes the new
/// title; returns it when it did.
pub fn save_history(
    id: &str,
    history: &[ChatMessage],
    title_hint: Option<&str>,
) -> anyhow::Result<Option<String>> {
    write_session_file(id, HISTORY_FILE, serde_json::to_string(history)?.as_bytes())?;
    let mut meta = read_meta(id)?;
    let mut new_title = None;
    if meta.title == DEFAULT_TITLE {
        if let Some(hint) = title_hint.filter(|h| !h.is_empty()) {
            meta.title = hint.to_string();
            new_title = Some(meta.title.clone());
        }
    }
    meta.updated_at = chrono::Local::now().to_rfc3339();
    write_meta(id, &meta)?;
    Ok(new_title)
}

/// Locks internally.
pub async fn append_message(id: &str, msg: ChatMessage) -> anyhow::Result<()> {
    let _guard = lock(id).await;
    let mut history = read_history(id)?;
    history.push(msg.with_id());
    save_history(id, &history, None)?;
    Ok(())
}

/// Attaches a generated image to that exact message; errors if it is gone
/// (e.g. the session was deleted meanwhile). Locks internally.
pub async fn attach_generated_image(
    id: &str,
    message_id: &str,
    image_path: &str,
) -> anyhow::Result<()> {
    let _guard = lock(id).await;
    let mut history = read_history(id)?;
    let target = history
        .iter_mut()
        .find(|m| m.id == message_id)
        .ok_or_else(|| anyhow::anyhow!("message \"{message_id}\" no longer exists"))?;
    target.generated_images.push(image_path.to_string());
    save_history(id, &history, None)?;
    Ok(())
}

/// The compact prose summary every in-character call reads as context.
pub fn read_state(id: &str) -> String {
    read_session_file(id, STATE_FILE).unwrap_or_default()
}

/// The full character sheet as raw JSON, the source of truth; empty if missing.
pub fn read_raw_state(id: &str) -> String {
    read_session_file(id, RAW_STATE_FILE).unwrap_or_default()
}

fn position_of(history: &[ChatMessage], message_id: &str) -> Option<usize> {
    history.iter().position(|m| m.id == message_id)
}

/// Writes `state.json` and its stamp unless a later message already stamped
/// the state (an out-of-order update). `Ok(false)` means skipped. Locks
/// internally.
pub async fn write_raw_state_stamped(
    id: &str,
    source_message_id: &str,
    json: &str,
) -> anyhow::Result<bool> {
    let _guard = lock(id).await;
    let history = read_history(id)?;
    let Some(pos) = position_of(&history, source_message_id) else {
        return Ok(false);
    };
    let stamped_pos = read_session_file(id, STATE_STAMP_FILE)
        .and_then(|stamp| position_of(&history, stamp.trim()));
    if stamped_pos.is_some_and(|p| p > pos) {
        return Ok(false);
    }
    write_session_file(id, RAW_STATE_FILE, json.as_bytes())?;
    write_session_file(id, STATE_STAMP_FILE, source_message_id.as_bytes())?;
    Ok(true)
}

/// Writes `state.md` only while `source_message_id` is still the stamp, so a
/// slow summary never overwrites a newer state's. Locks internally.
pub async fn write_state_md_stamped(
    id: &str,
    source_message_id: &str,
    markdown: &str,
) -> anyhow::Result<bool> {
    let _guard = lock(id).await;
    let stamp = read_session_file(id, STATE_STAMP_FILE).unwrap_or_default();
    if stamp.trim() != source_message_id {
        return Ok(false);
    }
    write_session_file(id, STATE_FILE, markdown.as_bytes())?;
    Ok(true)
}

/// Locks internally.
pub async fn rename_session(id: &str, title: &str) -> anyhow::Result<()> {
    let _guard = lock(id).await;
    let mut meta = read_meta(id)?;
    meta.title = title.to_string();
    write_meta(id, &meta)
}

/// Locks internally.
pub async fn delete_session(id: &str) -> anyhow::Result<()> {
    let dir = session_dir(id)?;
    let guard = lock(id).await;
    fs::remove_dir_all(dir)?;
    drop(guard);
    forget_locks(id);
    crate::recall::delete_session(id).await;
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
        ChatMessage::text(role, content).with_id()
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
    fn history_round_trips_and_auto_titles() {
        let dir = scratch("history");
        let session = create_session(None).unwrap();
        let history = vec![msg("user", "hello"), msg("assistant", "hi there")];
        let title = save_history(&session.id, &history, Some("hello")).unwrap();
        assert_eq!(title.as_deref(), Some("hello"));
        let (meta, loaded) = load_session(&session.id).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[1].content, "hi there");
        assert_eq!(loaded[1].id, history[1].id);
        assert_eq!(meta.title, "hello");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_renamed_session_is_never_auto_retitled_again() {
        let dir = scratch("no-retitle");
        let session = create_session(None).unwrap();
        rename_session(&session.id, "My real title").await.unwrap();
        let title = save_history(&session.id, &[msg("user", "hi")], Some("hi")).unwrap();
        assert_eq!(title, None);
        let (meta, _) = load_session(&session.id).unwrap();
        assert_eq!(meta.title, "My real title");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_messages_without_ids_get_stable_ids_that_persist_on_save() {
        let dir = scratch("legacy-ids");
        let session = create_session(None).unwrap();
        let legacy = r#"[{"role":"user","content":"a"},{"role":"assistant","content":"b"}]"#;
        write_session_file(&session.id, HISTORY_FILE, legacy.as_bytes()).unwrap();
        let first = load_history(&session.id).unwrap();
        let second = load_history(&session.id).unwrap();
        assert!(first.iter().all(|m| !m.id.is_empty()));
        assert_eq!(first[0].id, second[0].id, "ids must agree across loads");
        assert_ne!(first[0].id, first[1].id);
        save_history(&session.id, &first, None).unwrap();
        let raw = read_session_file(&session.id, HISTORY_FILE).unwrap();
        assert!(raw.contains(&first[0].id) && raw.contains(&first[1].id));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stale_state_updates_are_skipped_by_stamp() {
        let dir = scratch("state-stamp");
        let session = create_session(None).unwrap();
        let (m1, m2) = (msg("assistant", "one"), msg("assistant", "two"));
        save_history(&session.id, &[m1.clone(), m2.clone()], None).unwrap();
        assert_eq!(read_state(&session.id), "");
        assert_eq!(read_raw_state(&session.id), "{}");

        assert!(
            write_raw_state_stamped(&session.id, &m2.id, r#"{"HP":"80"}"#)
                .await
                .unwrap()
        );
        // An update derived from the older message arrives late.
        assert!(
            !write_raw_state_stamped(&session.id, &m1.id, r#"{"HP":"100"}"#)
                .await
                .unwrap()
        );
        assert_eq!(read_raw_state(&session.id), r#"{"HP":"80"}"#);

        // A summary for the older state must not overwrite the newer one.
        assert!(!write_state_md_stamped(&session.id, &m1.id, "old")
            .await
            .unwrap());
        assert!(write_state_md_stamped(&session.id, &m2.id, "HP: 80")
            .await
            .unwrap());
        assert_eq!(read_state(&session.id), "HP: 80");

        // A source message that no longer exists is stale.
        assert!(!write_raw_state_stamped(&session.id, "gone", "{}")
            .await
            .unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn attach_lands_on_the_right_message_while_others_are_appended() {
        let dir = scratch("attach-concurrent");
        let session = create_session(None).unwrap();
        let target = msg("assistant", "here you go");
        save_history(&session.id, &[msg("user", "pic?"), target.clone()], None).unwrap();

        let id = session.id.clone();
        let appenders: Vec<_> = (0..5)
            .map(|i| {
                let id = id.clone();
                tokio::spawn(
                    async move { append_message(&id, msg("user", &format!("next {i}"))).await },
                )
            })
            .collect();
        let attacher = {
            let (id, target_id) = (id.clone(), target.id.clone());
            tokio::spawn(async move { attach_generated_image(&id, &target_id, "/x/a.png").await })
        };
        for h in appenders {
            h.await.unwrap().unwrap();
        }
        attacher.await.unwrap().unwrap();

        let history = load_history(&session.id).unwrap();
        assert_eq!(history.len(), 7, "nothing may be lost");
        let with_image: Vec<_> = history
            .iter()
            .filter(|m| !m.generated_images.is_empty())
            .collect();
        assert_eq!(with_image.len(), 1);
        assert_eq!(with_image[0].id, target.id);
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn attach_errors_when_the_message_is_gone() {
        let dir = scratch("attach-missing");
        let session = create_session(None).unwrap();
        save_history(&session.id, &[msg("user", "hi")], None).unwrap();
        assert!(attach_generated_image(&session.id, "nope", "/x/a.png")
            .await
            .is_err());
        delete_session(&session.id).await.unwrap();
        assert!(attach_generated_image(&session.id, "nope", "/x/a.png")
            .await
            .is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn append_message_adds_at_the_end_with_an_id() {
        let dir = scratch("append");
        let session = create_session(None).unwrap();
        save_history(&session.id, &[msg("user", "hi")], None).unwrap();
        append_message(&session.id, ChatMessage::text("assistant", "reaction"))
            .await
            .unwrap();
        let history = load_history(&session.id).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].role, "assistant");
        assert_eq!(history[1].content, "reaction");
        assert!(!history[1].id.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_id_validation_accepts_only_generated_shapes() {
        for bad in [
            "/home/x",
            "../..",
            "session-20260101-120000/../x",
            "",
            "session-",
            "session-20260101-120000-",
            "session-20260101-120000-1-2",
            "session-2026010-120000",
            "session-20260101-12000a",
            "session-20260101-120000/",
            "x/session-20260101-120000",
        ] {
            assert!(session_dir(bad).is_err(), "{bad:?} should be rejected");
        }
        for good in ["session-20260101-120000", "session-20260101-120000-2"] {
            assert!(session_dir(good).is_ok(), "{good:?} should be accepted");
        }
    }

    #[test]
    fn generated_ids_always_validate() {
        let dir = scratch("generated-ids");
        let session = create_session(None).unwrap();
        assert!(is_valid_session_id(&session.id));
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_with_a_bad_id_errors_and_touches_nothing() {
        let dir = scratch("delete-bad");
        let _session = create_session(None).unwrap();
        let victim = dir.join("victim");
        fs::create_dir_all(&victim).unwrap();
        for bad in [victim.to_str().unwrap(), "../..", ""] {
            assert!(delete_session(bad).await.is_err());
        }
        assert!(victim.exists());
        assert_eq!(list_sessions().unwrap().len(), 1);
        assert_eq!(read_state("../.."), "");
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_removes_it_from_the_list() {
        let dir = scratch("delete");
        let session = create_session(None).unwrap();
        delete_session(&session.id).await.unwrap();
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
