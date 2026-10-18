//! Chat recall: a local search index over chat history, so a long chat keeps
//! what `context::fit_to_budget` drops from the prompt. Before every turn the
//! app retrieves the few most relevant old messages that aren't in the prompt
//! and appends them, labelled, to that request's last user message. The model
//! decides nothing: small models are bad at choosing to call a search tool.
//!
//! Retrieval is hybrid. SQLite FTS5 (BM25) is always on; when an embedding
//! model is configured, cosine similarity joins it and the two ranks are fused
//! (RRF). Any failure degrades to keyword only, then to nothing; recall never
//! fails a turn.
//!
//! Everything SQLite runs in `spawn_blocking`, with the DB path resolved on the
//! calling thread (tests redirect it per thread through `chat_session`'s test
//! root, which a blocking-pool thread can't see).

use crate::chat_session;
use crate::config::AppConfig;
use crate::llm::{self, ChatMessage};
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

const CHUNK_CHARS: usize = 600;
const MAX_QUERY_TERMS: usize = 12;
/// Per retriever, before fusion.
const CANDIDATES: usize = 20;
const RRF_K: f32 = 60.0;
/// Embedding hits this far below the best one are dropped: a strong match
/// makes the merely-similar ones noise (measured: they pulled in chit-chat).
const RELATIVE_GAP: f32 = 0.08;
/// The user's own messages carry the facts people ask back about; the
/// character's replies are mostly reactions, so they rank a bit lower.
const ASSISTANT_WEIGHT: f32 = 0.75;
const SNIPPET_MAX_CHARS: usize = 240;
const MAX_BLOCK_CHARS: usize = 1200;
/// Un-embedded chunks embedded per recall, so legacy history is covered gradually.
const EMBED_BACKFILL: usize = 64;
const EMBED_BATCH: usize = 32;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS chunks(
  id INTEGER PRIMARY KEY,
  session_id TEXT NOT NULL,
  persona TEXT,
  message_id TEXT NOT NULL,
  role TEXT NOT NULL,
  ts INTEGER,
  text TEXT NOT NULL,
  embedding BLOB,
  embed_model TEXT
);
CREATE INDEX IF NOT EXISTS chunks_session ON chunks(session_id);
CREATE INDEX IF NOT EXISTS chunks_persona ON chunks(persona);
CREATE UNIQUE INDEX IF NOT EXISTS chunks_msg ON chunks(message_id, session_id, text);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
  text, content='chunks', content_rowid='id', tokenize='porter unicode61');
CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);
";

/// A snippet of old chat to show the model.
#[derive(Debug, Clone, PartialEq)]
pub struct Recalled {
    /// "user" or "assistant".
    pub role: String,
    pub text: String,
    /// Unix seconds the message was indexed; `None` for backfilled history.
    pub ts: Option<i64>,
}

/// Chunks newly inserted by `index_messages`, waiting for their embedding.
pub struct Fresh {
    path: PathBuf,
    chunks: Vec<(i64, String)>,
}

impl Fresh {
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

// --- Database ---

type Db = Arc<Mutex<Connection>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Next to `sessions/`, so it follows the session root in tests.
fn db_path() -> PathBuf {
    chat_session::sessions_root()
        .parent()
        .map(|p| p.join("recall.sqlite"))
        .unwrap_or_else(|| PathBuf::from("recall.sqlite"))
}

fn open_new(path: &Path) -> anyhow::Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

fn open_db(path: &Path) -> anyhow::Result<Db> {
    static DBS: OnceLock<Mutex<HashMap<PathBuf, Db>>> = OnceLock::new();
    let mut map = lock(DBS.get_or_init(Default::default));
    if let Some(db) = map.get(path) {
        return Ok(db.clone());
    }
    match open_new(path) {
        Ok(conn) => {
            let db = Arc::new(Mutex::new(conn));
            map.insert(path.to_path_buf(), db.clone());
            Ok(db)
        }
        Err(e) => {
            static WARN: std::sync::Once = std::sync::Once::new();
            WARN.call_once(|| log::warn!("chat recall is off: can't open its database: {e}"));
            Err(e)
        }
    }
}

fn with_db<T>(
    path: &Path,
    f: impl FnOnce(&mut Connection) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let db = open_db(path)?;
    let mut conn = lock(&db);
    f(&mut conn)
}

async fn blocking<T: Send + 'static>(
    path: &Path,
    f: impl FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || with_db(&path, f))
        .await
        .map_err(|e| anyhow::anyhow!("recall task failed: {e}"))?
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

// --- Text ---

/// What gets indexed: roleplay markers (`*`, `(`/`)`, and the legacy `//`,
/// `||`) become spaces, an attachment message keeps only its
/// `[attached: name]` line, whitespace is collapsed per line.
fn clean_text(raw: &str) -> String {
    let raw = raw.trim();
    let raw = if raw.starts_with("[attached:") {
        raw.lines().next().unwrap_or("")
    } else {
        raw
    };
    raw.replace("//", " ")
        .replace("||", " ")
        .replace(['*', '(', ')'], " ")
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let mut start = 0;
        let mut chars = line.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            let boundary = matches!(c, '.' | '!' | '?' | '…')
                && match chars.peek() {
                    None => true,
                    Some((_, next)) => next.is_whitespace(),
                };
            if boundary {
                let end = i + c.len_utf8();
                let s = line[start..end].trim();
                if !s.is_empty() {
                    out.push(s.to_string());
                }
                start = end;
            }
        }
        let tail = line[start..].trim();
        if !tail.is_empty() {
            out.push(tail.to_string());
        }
    }
    out
}

/// A sentence longer than `max` chars, cut at whitespace where possible.
fn hard_split(s: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while rest.chars().count() > max {
        let cut = rest.char_indices().nth(max).map_or(rest.len(), |(i, _)| i);
        let split = rest[..cut]
            .rfind(char::is_whitespace)
            .filter(|&i| i > 0)
            .unwrap_or(cut);
        out.push(rest[..split].trim().to_string());
        rest = rest[split..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

/// About `CHUNK_CHARS` per chunk, on sentence or line boundaries.
fn chunk_text(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut cur, mut cur_len) = (String::new(), 0usize);
    for sentence in sentences(text) {
        for piece in hard_split(&sentence, CHUNK_CHARS) {
            let len = piece.chars().count();
            if cur_len > 0 && cur_len + 1 + len > CHUNK_CHARS {
                out.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            if cur_len > 0 {
                cur.push(' ');
                cur_len += 1;
            }
            cur.push_str(&piece);
            cur_len += len;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "your", "yours", "all", "any", "can", "had",
    "her", "was", "one", "our", "out", "has", "have", "him", "his", "how", "its", "who", "why",
    "did", "does", "doesn", "don", "didn", "isn", "wasn", "aren", "won", "that", "this", "with",
    "from", "they", "them", "then", "than", "there", "these", "those", "what", "when", "where",
    "which", "while", "will", "would", "could", "should", "about", "into", "over", "just", "like",
    "been", "being", "were", "very", "some", "also", "too", "yes", "got", "get", "let", "say",
    "said", "see", "she", "tell", "told", "remember", "again", "ever", "much", "more", "most",
    "hey", "hello", "okay", "yeah",
];

/// Words of 3+ letters or digits, minus stopwords, deduped, capped.
fn query_terms(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        let word = word.to_lowercase();
        if word.chars().count() < 3 || STOPWORDS.contains(&word.as_str()) || terms.contains(&word) {
            continue;
        }
        terms.push(word);
        if terms.len() == MAX_QUERY_TERMS {
            break;
        }
    }
    terms
}

/// Every term double-quoted (so FTS5 syntax in user text is inert), OR-joined.
fn fts_query(terms: &[String]) -> Option<String> {
    (!terms.is_empty()).then(|| {
        terms
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ")
    })
}

// --- Embeddings ---

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Option<Vec<f32>> {
    (b.len() % 4 == 0 && !b.is_empty()).then(|| {
        b.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    })
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// The configured embedding model, if any (empty = keyword only).
fn embedding_model(cfg: &AppConfig) -> Option<&str> {
    let m = cfg.chat_recall_embedding_model.trim();
    (!m.is_empty()).then_some(m)
}

#[derive(Clone, Copy)]
enum Kind {
    Document,
    Query,
}

/// nomic-embed-text is trained with task prefixes and is poor without them;
/// other models get the text as is.
fn with_prefix(model: &str, kind: Kind, text: &str) -> String {
    if !model.to_lowercase().contains("nomic") {
        return text.to_string();
    }
    match kind {
        Kind::Document => format!("search_document: {text}"),
        Kind::Query => format!("search_query: {text}"),
    }
}

async fn embed_texts(
    cfg: &AppConfig,
    model: &str,
    kind: Kind,
    texts: &[String],
) -> anyhow::Result<Vec<Vec<f32>>> {
    let mut out = Vec::with_capacity(texts.len());
    for batch in texts.chunks(EMBED_BATCH) {
        let inputs: Vec<String> = batch.iter().map(|t| with_prefix(model, kind, t)).collect();
        out.extend(llm::embed(&cfg.endpoint, &cfg.api_key, model, &inputs).await?);
    }
    Ok(out)
}

fn store_embeddings(
    conn: &mut Connection,
    model: &str,
    items: Vec<(i64, Vec<f32>)>,
) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut stmt =
            tx.prepare_cached("UPDATE chunks SET embedding = ?1, embed_model = ?2 WHERE id = ?3")?;
        for (id, vector) in items {
            stmt.execute(params![to_blob(&vector), model, id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Embeds chunks `index_messages` just inserted. Best effort: a failure is
/// logged and leaves them un-embedded for the next recall's backfill.
pub async fn embed_fresh(cfg: &AppConfig, fresh: Fresh) {
    let Some(model) = embedding_model(cfg) else {
        return;
    };
    if fresh.chunks.is_empty() {
        return;
    }
    let (ids, texts): (Vec<i64>, Vec<String>) = fresh.chunks.into_iter().unzip();
    let result = async {
        let vectors = embed_texts(cfg, model, Kind::Document, &texts).await?;
        let items: Vec<(i64, Vec<f32>)> = ids.into_iter().zip(vectors).collect();
        let model = model.to_string();
        blocking(&fresh.path, move |conn| {
            store_embeddings(conn, &model, items)
        })
        .await
    }
    .await;
    if let Err(e) = result {
        log::warn!("recall: embedding new messages failed: {e}");
    }
}

// --- Indexing ---

fn insert_message(
    conn: &Connection,
    session_id: &str,
    persona: Option<&str>,
    msg: &ChatMessage,
    ts: Option<i64>,
) -> anyhow::Result<Vec<(i64, String)>> {
    let mut fresh = Vec::new();
    if msg.id.is_empty() || (msg.role != "user" && msg.role != "assistant") {
        return Ok(fresh);
    }
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO chunks(session_id, persona, message_id, role, ts, text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for chunk in chunk_text(&clean_text(&msg.content)) {
        let inserted = stmt.execute(params![session_id, persona, msg.id, msg.role, ts, chunk])?;
        if inserted > 0 {
            fresh.push((conn.last_insert_rowid(), chunk));
        }
    }
    Ok(fresh)
}

/// Indexes user and assistant messages (idempotent per message id). Never
/// fails outward; the returned chunks are what `embed_fresh` should embed.
pub async fn index_messages(
    session_id: &str,
    persona: Option<&str>,
    msgs: &[ChatMessage],
) -> Fresh {
    let path = db_path();
    let (sid, persona) = (session_id.to_string(), persona.map(str::to_string));
    // Only id/role/content matter; don't carry image data URLs across threads.
    let msgs: Vec<ChatMessage> = msgs
        .iter()
        .map(|m| {
            let mut copy = ChatMessage::text(m.role.clone(), m.content.clone());
            copy.id = m.id.clone();
            copy
        })
        .collect();
    let chunks = blocking(&path, move |conn| {
        let tx = conn.transaction()?;
        let mut fresh = Vec::new();
        for msg in &msgs {
            fresh.extend(insert_message(
                &tx,
                &sid,
                persona.as_deref(),
                msg,
                Some(now_secs()),
            )?);
        }
        tx.commit()?;
        Ok(fresh)
    })
    .await
    .unwrap_or_else(|e| {
        log::warn!("recall: indexing failed: {e}");
        Vec::new()
    });
    Fresh { path, chunks }
}

/// Drops one message's rows (a turn that ended without a reply removes its
/// user message from the history).
pub async fn delete_message(session_id: &str, message_id: &str) {
    let path = db_path();
    if !path.exists() {
        return;
    }
    let (sid, mid) = (session_id.to_string(), message_id.to_string());
    let result = blocking(&path, move |conn| {
        conn.execute(
            "DELETE FROM chunks WHERE session_id = ?1 AND message_id = ?2",
            params![sid, mid],
        )?;
        Ok(())
    })
    .await;
    if let Err(e) = result {
        log::warn!("recall: removing a message failed: {e}");
    }
}

/// Removes a deleted chat's rows.
pub async fn delete_session(session_id: &str) {
    let path = db_path();
    if !path.exists() {
        return;
    }
    let sid = session_id.to_string();
    let result = blocking(&path, move |conn| {
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM chunks WHERE session_id = ?1", params![sid])?;
        tx.execute(
            "DELETE FROM meta WHERE key = ?1",
            params![backfill_key(&sid)],
        )?;
        tx.commit()?;
        Ok(())
    })
    .await;
    if let Err(e) = result {
        log::warn!("recall: removing a deleted chat's index failed: {e}");
    }
}

fn backfill_key(session_id: &str) -> String {
    format!("backfilled:{session_id}")
}

/// Indexes the existing history of every not-yet-backfilled session in scope
/// (ts NULL: their real times are unknown).
async fn ensure_backfilled(
    path: &Path,
    sessions: Vec<(String, Option<String>)>,
) -> anyhow::Result<()> {
    let ids: Vec<String> = sessions.iter().map(|(id, _)| id.clone()).collect();
    let pending: HashSet<String> = blocking(path, move |conn| {
        let mut stmt = conn.prepare_cached("SELECT 1 FROM meta WHERE key = ?1")?;
        let mut pending = HashSet::new();
        for id in ids {
            if !stmt.exists(params![backfill_key(&id)])? {
                pending.insert(id);
            }
        }
        Ok(pending)
    })
    .await?;
    if pending.is_empty() {
        return Ok(());
    }
    let mut loaded = Vec::new();
    for (id, persona) in sessions.into_iter().filter(|(id, _)| pending.contains(id)) {
        match chat_session::load_history(&id) {
            Ok(history) => loaded.push((id, persona, history)),
            Err(e) => log::warn!("recall: can't backfill {id}: {e}"),
        }
    }
    blocking(path, move |conn| {
        let tx = conn.transaction()?;
        for (id, persona, history) in &loaded {
            for msg in history {
                insert_message(&tx, id, persona.as_deref(), msg, None)?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, '1')",
                params![backfill_key(id)],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

// --- Query ---

#[derive(Clone)]
struct Scope {
    session_id: String,
    /// `Some` widens the scope to every chat with this persona name.
    persona: Option<String>,
}

const SCOPE_SQL: &str = "(session_id = ?1 OR (?2 IS NOT NULL AND persona = ?2))";

fn has_candidates(
    conn: &Connection,
    scope: &Scope,
    exclude: &HashSet<String>,
) -> anyhow::Result<bool> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT DISTINCT message_id FROM chunks WHERE {SCOPE_SQL}"
    ))?;
    let mut rows = stmt.query(params![scope.session_id, scope.persona])?;
    while let Some(row) = rows.next()? {
        if !exclude.contains(&row.get::<_, String>(0)?) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Chunk ids by BM25, best first.
fn keyword_search(
    conn: &Connection,
    scope: &Scope,
    match_query: &str,
    exclude: &HashSet<String>,
) -> anyhow::Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT c.id, c.message_id FROM chunks_fts
         JOIN chunks c ON c.id = chunks_fts.rowid
         WHERE chunks_fts MATCH ?3 AND (c.session_id = ?1 OR (?2 IS NOT NULL AND c.persona = ?2))
         ORDER BY bm25(chunks_fts) LIMIT ?4",
    )?;
    let limit = (CANDIDATES + exclude.len().min(500)) as i64;
    let mut rows = stmt.query(params![scope.session_id, scope.persona, match_query, limit])?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next()? {
        if !exclude.contains(&row.get::<_, String>(1)?) {
            ids.push(row.get(0)?);
            if ids.len() == CANDIDATES {
                break;
            }
        }
    }
    Ok(ids)
}

/// Newest first; chunks in the prompt are skipped (embedded once they drop out).
fn unembedded(
    conn: &Connection,
    scope: &Scope,
    model: &str,
    exclude: &HashSet<String>,
) -> anyhow::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT id, message_id, text FROM chunks
         WHERE {SCOPE_SQL} AND (embedding IS NULL OR embed_model IS NOT ?3)
         ORDER BY id DESC LIMIT ?4"
    ))?;
    let limit = (EMBED_BACKFILL + exclude.len().min(500)) as i64;
    let mut rows = stmt.query(params![scope.session_id, scope.persona, model, limit])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        if !exclude.contains(&row.get::<_, String>(1)?) {
            out.push((row.get(0)?, row.get(2)?));
            if out.len() == EMBED_BACKFILL {
                break;
            }
        }
    }
    Ok(out)
}

struct EmbeddingHits {
    /// At or above the threshold, best first.
    top: Vec<(i64, f32)>,
    /// Similarity of the requested keyword hits that have an embedding.
    scored: HashMap<i64, f32>,
}

fn embedding_search(
    conn: &Connection,
    scope: &Scope,
    model: &str,
    query: &[f32],
    exclude: &HashSet<String>,
    min_similarity: f32,
    keyword_ids: &HashSet<i64>,
) -> anyhow::Result<EmbeddingHits> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT id, message_id, embedding FROM chunks
         WHERE {SCOPE_SQL} AND embed_model = ?3 AND embedding IS NOT NULL"
    ))?;
    let mut rows = stmt.query(params![scope.session_id, scope.persona, model])?;
    let mut all: Vec<(i64, f32)> = Vec::new();
    let mut scored = HashMap::new();
    while let Some(row) = rows.next()? {
        let (id, message_id): (i64, String) = (row.get(0)?, row.get(1)?);
        if exclude.contains(&message_id) {
            continue;
        }
        let blob: Vec<u8> = row.get(2)?;
        let Some(vector) = from_blob(&blob).filter(|v| v.len() == query.len()) else {
            continue;
        };
        let sim = cosine(query, &vector);
        if keyword_ids.contains(&id) {
            scored.insert(id, sim);
        }
        if sim >= min_similarity {
            all.push((id, sim));
        }
    }
    all.sort_by(|a, b| b.1.total_cmp(&a.1));
    if let Some(&(_, best)) = all.first() {
        all.retain(|(_, sim)| *sim >= best - RELATIVE_GAP);
    }
    all.truncate(CANDIDATES);
    Ok(EmbeddingHits { top: all, scored })
}

/// Reciprocal rank fusion of two best-first lists; ties keep the keyword
/// side's order.
fn fuse(keyword: &[i64], embedding: &[i64], max: usize, weight: impl Fn(i64) -> f32) -> Vec<i64> {
    let mut score: HashMap<i64, f32> = HashMap::new();
    let mut order: Vec<i64> = Vec::new();
    for list in [keyword, embedding] {
        for (rank, id) in list.iter().enumerate() {
            let entry = score.entry(*id).or_insert_with(|| {
                order.push(*id);
                0.0
            });
            *entry += weight(*id) / (RRF_K + rank as f32 + 1.0);
        }
    }
    order.sort_by(|a, b| score[b].total_cmp(&score[a]));
    order.truncate(max);
    order
}

struct ChunkRow {
    id: i64,
    session_id: String,
    message_id: String,
    role: String,
    ts: Option<i64>,
    text: String,
}

fn load_chunks(conn: &Connection, ids: &[i64]) -> anyhow::Result<Vec<ChunkRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, session_id, message_id, role, ts, text FROM chunks WHERE id = ?1",
    )?;
    let mut out = Vec::new();
    for id in ids {
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            out.push(ChunkRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                message_id: row.get(2)?,
                role: row.get(3)?,
                ts: row.get(4)?,
                text: row.get(5)?,
            });
        }
    }
    Ok(out)
}

/// Rank-ordered chunks -> snippets: chunks of one message merge (a gap in the
/// chunk run shows as an ellipsis) and sit at the message's best rank.
fn assemble(rows: Vec<ChunkRow>) -> Vec<Recalled> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut groups: HashMap<(String, String), Vec<ChunkRow>> = HashMap::new();
    for row in rows {
        let key = (row.session_id.clone(), row.message_id.clone());
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(row);
    }
    order
        .into_iter()
        .filter_map(|key| {
            let mut chunks = groups.remove(&key)?;
            chunks.sort_by_key(|c| c.id);
            let mut text = String::new();
            let mut prev: Option<i64> = None;
            for c in &chunks {
                if let Some(p) = prev {
                    text.push_str(if c.id == p + 1 { " " } else { " … " });
                }
                text.push_str(&c.text);
                prev = Some(c.id);
            }
            Some(Recalled {
                role: chunks[0].role.clone(),
                text,
                ts: chunks[0].ts,
            })
        })
        .collect()
}

/// Lowest-ranked snippets go first; a lone oversized one is cut.
fn fit_chars(items: Vec<Recalled>, max: usize) -> Vec<Recalled> {
    let mut out: Vec<Recalled> = Vec::new();
    let mut used = 0;
    for mut item in items {
        let len = item.text.chars().count();
        if used + len > max {
            if !out.is_empty() {
                continue;
            }
            item.text = item
                .text
                .chars()
                .take(max.saturating_sub(1))
                .collect::<String>()
                + "…";
            used = max;
        } else {
            used += len;
        }
        out.push(item);
    }
    out
}

/// Where this turn's candidates come from: this chat, or every chat sharing
/// the persona name (the current one always included).
fn scope_sessions(
    cfg: &AppConfig,
    session_id: &str,
    persona: Option<&str>,
) -> (Scope, Vec<(String, Option<String>)>) {
    let persona_scope = (cfg.chat_recall_scope == "persona")
        .then_some(persona)
        .flatten();
    let mut sessions = vec![(session_id.to_string(), persona.map(str::to_string))];
    if let Some(name) = persona_scope {
        for s in chat_session::list_sessions().unwrap_or_default() {
            if s.persona.as_deref() == Some(name) && s.id != session_id {
                sessions.push((s.id, s.persona));
            }
        }
    }
    let scope = Scope {
        session_id: session_id.to_string(),
        persona: persona_scope.map(str::to_string),
    };
    (scope, sessions)
}

/// The few most relevant old messages that aren't already in the prompt
/// (`exclude_message_ids`; this includes the current user message). Any
/// failure degrades to keyword only, then to nothing.
pub async fn recall(
    cfg: &AppConfig,
    session_id: &str,
    persona: Option<&str>,
    query_text: &str,
    exclude_message_ids: &HashSet<String>,
) -> Vec<Recalled> {
    if !cfg.chat_recall || cfg.chat_recall_max == 0 {
        return Vec::new();
    }
    match recall_inner(cfg, session_id, persona, query_text, exclude_message_ids).await {
        Ok(items) => items,
        Err(e) => {
            log::warn!("recall failed, continuing without it: {e}");
            Vec::new()
        }
    }
}

async fn recall_inner(
    cfg: &AppConfig,
    session_id: &str,
    persona: Option<&str>,
    query_text: &str,
    exclude: &HashSet<String>,
) -> anyhow::Result<Vec<Recalled>> {
    let path = db_path();
    let (scope, sessions) = scope_sessions(cfg, session_id, persona);
    ensure_backfilled(&path, sessions).await?;

    let candidates = {
        let (scope, exclude) = (scope.clone(), exclude.clone());
        blocking(&path, move |conn| has_candidates(conn, &scope, &exclude)).await?
    };
    if !candidates {
        log::debug!("recall: nothing outside the prompt to search");
        return Ok(Vec::new());
    }

    let query = clean_text(query_text);
    let match_query = fts_query(&query_terms(&query));
    let keyword: Vec<i64> = match match_query {
        Some(q) => {
            let (scope, exclude) = (scope.clone(), exclude.clone());
            blocking(&path, move |conn| {
                keyword_search(conn, &scope, &q, &exclude)
            })
            .await
            .unwrap_or_else(|e| {
                log::warn!("recall: keyword search failed: {e}");
                Vec::new()
            })
        }
        None => Vec::new(),
    };

    let mut embedded: Option<EmbeddingHits> = None;
    if let Some(model) = embedding_model(cfg) {
        match embedding_phase(cfg, &path, &scope, model, &query, exclude, &keyword).await {
            Ok(hits) => embedded = Some(hits),
            Err(e) => log::warn!("recall: embeddings unavailable, using keywords only: {e}"),
        }
    }

    // Drop a keyword hit the embedding scored below (an un-embedded chunk has
    // no score and stays) the threshold: the
    // embedding is the stronger relevance signal, a weak hit on a common word
    // is noise.
    let keyword: Vec<i64> = match &embedded {
        Some(hits) => keyword
            .into_iter()
            .filter(|id| {
                hits.scored
                    .get(id)
                    .map_or(true, |s| *s >= cfg.chat_recall_min_similarity)
            })
            .collect(),
        None => keyword,
    };
    let embedding_ids: Vec<i64> = embedded
        .as_ref()
        .map(|h| h.top.iter().map(|(id, _)| *id).collect())
        .unwrap_or_default();

    let candidates: Vec<i64> = keyword
        .iter()
        .chain(embedding_ids.iter())
        .copied()
        .collect::<HashSet<i64>>()
        .into_iter()
        .collect();
    let assistant_ids: HashSet<i64> = if candidates.is_empty() {
        HashSet::new()
    } else {
        blocking(&path, move |conn| load_chunks(conn, &candidates))
            .await?
            .into_iter()
            .filter(|r| r.role == "assistant")
            .map(|r| r.id)
            .collect()
    };
    let selected = fuse(
        &keyword,
        &embedding_ids,
        cfg.chat_recall_max as usize,
        |id| {
            if assistant_ids.contains(&id) {
                ASSISTANT_WEIGHT
            } else {
                1.0
            }
        },
    );
    if selected.is_empty() {
        log::debug!("recall: no relevant earlier messages");
        return Ok(Vec::new());
    }
    let rows = {
        let selected = selected.clone();
        blocking(&path, move |conn| load_chunks(conn, &selected)).await?
    };
    let kw: HashSet<i64> = keyword.iter().copied().collect();
    let emb: HashSet<i64> = embedding_ids.iter().copied().collect();
    let used_keyword = selected.iter().any(|id| kw.contains(id));
    let used_embedding = selected.iter().any(|id| emb.contains(id));
    let mode = match (used_keyword, used_embedding) {
        (true, true) => "hybrid",
        (false, true) => "embedding",
        _ => "keyword",
    };
    let top_similarity = embedded.as_ref().and_then(|h| {
        h.top
            .iter()
            .filter(|(id, _)| selected.contains(id))
            .map(|(_, s)| *s)
            .reduce(f32::max)
    });
    let items = fit_chars(assemble(rows), MAX_BLOCK_CHARS);
    log::info!(
        "recall: {} earlier snippet(s) ({mode}{})",
        items.len(),
        top_similarity.map_or(String::new(), |s| format!(", top similarity {s:.2}"))
    );
    Ok(items)
}

/// Embeds a slice of un-embedded history, then the query, then scores. An
/// error anywhere means "no embedding signal this turn".
async fn embedding_phase(
    cfg: &AppConfig,
    path: &Path,
    scope: &Scope,
    model: &str,
    query: &str,
    exclude: &HashSet<String>,
    keyword: &[i64],
) -> anyhow::Result<EmbeddingHits> {
    let pending = {
        let (scope, exclude, model) = (scope.clone(), exclude.clone(), model.to_string());
        blocking(path, move |conn| unembedded(conn, &scope, &model, &exclude)).await?
    };
    if !pending.is_empty() {
        let (ids, texts): (Vec<i64>, Vec<String>) = pending.into_iter().unzip();
        let vectors = embed_texts(cfg, model, Kind::Document, &texts).await?;
        let items: Vec<(i64, Vec<f32>)> = ids.into_iter().zip(vectors).collect();
        let model = model.to_string();
        blocking(path, move |conn| store_embeddings(conn, &model, items)).await?;
    }
    let query_vec = embed_texts(cfg, model, Kind::Query, &[query.to_string()])
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no query embedding returned"))?;
    let (scope, exclude, model) = (scope.clone(), exclude.clone(), model.to_string());
    let keyword_ids: HashSet<i64> = keyword.iter().copied().collect();
    let min_similarity = cfg.chat_recall_min_similarity;
    blocking(path, move |conn| {
        embedding_search(
            conn,
            &scope,
            &model,
            &query_vec,
            &exclude,
            min_similarity,
            &keyword_ids,
        )
    })
    .await
}

// --- Prompt block ---

fn plural(n: i64, unit: &str) -> String {
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

fn relative_age(ts: Option<i64>, now: i64) -> String {
    let Some(ts) = ts else {
        return "earlier".to_string();
    };
    let secs = (now - ts).max(0);
    match secs {
        0..=59 => "moments ago".to_string(),
        60..=3599 => plural(secs / 60, "minute"),
        3600..=86399 => plural(secs / 3600, "hour"),
        86400..=2_591_999 => plural(secs / 86400, "day"),
        2_592_000..=31_103_999 => plural(secs / 2_592_000, "month"),
        _ => plural(secs / 31_104_000, "year"),
    }
}

/// The text appended to the turn's last user message.
pub fn format_block(items: &[Recalled]) -> String {
    let now = now_secs();
    let lines: Vec<String> = items
        .iter()
        .map(|r| {
            let who = if r.role == "user" {
                "the user said"
            } else {
                "you said"
            };
            let text = if r.text.chars().count() > SNIPPET_MAX_CHARS {
                format!(
                    "{}…",
                    r.text.chars().take(SNIPPET_MAX_CHARS).collect::<String>()
                )
            } else {
                r.text.clone()
            };
            format!("- ({}) {who}: {text}", relative_age(r.ts, now))
        })
        .collect();
    format!(
        "[From earlier in this chat. These are past moments and may be outdated; your current \
         state is what's true now:\n{}]",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::tests::fake_server;
    use serde_json::{json, Value};

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "llm-assistant-recall-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        chat_session::use_test_root(dir.join("chat/sessions"));
        dir
    }

    fn m(role: &str, text: &str) -> ChatMessage {
        ChatMessage::text(role, text).with_id()
    }

    fn ids(msgs: &[&ChatMessage]) -> HashSet<String> {
        msgs.iter().map(|m| m.id.clone()).collect()
    }

    fn row_count(path: &Path) -> i64 {
        with_db(path, |c| {
            Ok(c.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?)
        })
        .unwrap()
    }

    fn fts_count(path: &Path, word: &str) -> i64 {
        with_db(path, |c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
                [word],
                |r| r.get(0),
            )?)
        })
        .unwrap()
    }

    #[test]
    fn fts5_is_compiled_in() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(x, tokenize='porter unicode61');")
            .unwrap();
        conn.execute("INSERT INTO t(x) VALUES ('running cats')", [])
            .unwrap();
        let hits: i64 = conn
            .query_row("SELECT COUNT(*) FROM t WHERE t MATCH 'run'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(hits, 1, "porter stemming works");
    }

    #[test]
    fn cleaning_strips_markers_and_attachment_bodies() {
        assert_eq!(
            clean_text("// *waves* //  || Hello   there! ||"),
            "waves Hello there!"
        );
        assert_eq!(
            clean_text("*waves* \"Hello there!\" (I hope he stays)"),
            "waves \"Hello there!\" I hope he stays"
        );
        assert_eq!(
            clean_text("[attached: notes.txt]\nsecret body\n\nwhat is this"),
            "[attached: notes.txt]"
        );
        assert_eq!(clean_text("// || //"), "");
    }

    #[test]
    fn chunking_splits_long_text_on_sentences() {
        assert_eq!(chunk_text("short one."), vec!["short one."]);
        let sentence = "This is a fairly ordinary sentence about nothing in particular.";
        let text = vec![sentence; 40].join(" ");
        let chunks = chunk_text(&text);
        assert!(chunks.len() > 2);
        for c in &chunks {
            assert!(c.chars().count() <= CHUNK_CHARS, "{}", c.chars().count());
            assert!(c.ends_with('.'), "cut mid-sentence: {c}");
        }
        let words = |s: &str| s.split_whitespace().count();
        assert_eq!(chunks.iter().map(|c| words(c)).sum::<usize>(), words(&text));
    }

    #[test]
    fn a_single_huge_run_is_hard_split_without_panicking() {
        let text = "é".repeat(1500);
        let chunks = chunk_text(&text);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= CHUNK_CHARS));
    }

    #[test]
    fn query_terms_drop_stopwords_dedupe_and_cap() {
        assert_eq!(
            query_terms("What was the name of my Cat? The cat!"),
            vec!["name", "cat"]
        );
        let many = (0..30)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(query_terms(&many).len(), MAX_QUERY_TERMS);
        assert!(fts_query(&query_terms("it is to be")).is_none());
        assert_eq!(
            fts_query(&["a\"b".into(), "cat".into()]).as_deref(),
            Some("\"a\"\"b\" OR \"cat\"")
        );
    }

    #[tokio::test]
    async fn indexing_is_idempotent_and_syncs_the_fts_table() {
        let dir = test_dir("idempotent");
        let path = db_path();
        let msg = m("user", "My cat Biscuit likes tuna. She also likes salmon.");
        let first = index_messages("session-a", None, std::slice::from_ref(&msg)).await;
        assert!(!first.is_empty());
        let again = index_messages("session-a", None, std::slice::from_ref(&msg)).await;
        assert!(again.is_empty(), "re-indexing the same id is a no-op");
        assert_eq!(row_count(&path), first.chunks.len() as i64);
        assert_eq!(fts_count(&path, "biscuit"), 1);
        // Tool/system messages and id-less ones are never indexed.
        let skipped = [
            ChatMessage::text("system", "x y z").with_id(),
            ChatMessage::text("user", "no id here"),
        ];
        assert!(index_messages("session-a", None, &skipped).await.is_empty());
        delete_message("session-a", &msg.id).await;
        assert_eq!(row_count(&path), 0);
        assert_eq!(fts_count(&path, "biscuit"), 0, "delete trigger syncs FTS");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fts_syntax_in_user_text_never_errors() {
        let dir = test_dir("fts-syntax");
        let path = db_path();
        index_messages("session-a", None, &[m("user", "near the cat and dog")]).await;
        let nasty = "\" OR * AND NEAR( ^ : -x NOT col:foo {a b} 'quote' \"cat\"";
        let q = fts_query(&query_terms(nasty)).unwrap();
        let scope = Scope {
            session_id: "session-a".into(),
            persona: None,
        };
        let hits = with_db(&path, |c| keyword_search(c, &scope, &q, &HashSet::new())).unwrap();
        assert!(!hits.is_empty());
        let quoted = fts_query(&["a\"b".into()]).unwrap();
        with_db(&path, |c| {
            keyword_search(c, &scope, &quoted, &HashSet::new())
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn keyword_cfg() -> AppConfig {
        AppConfig::default()
    }

    #[tokio::test]
    async fn keyword_recall_finds_a_planted_message_and_skips_prompt_ids() {
        let dir = test_dir("keyword");
        let session = chat_session::create_session(None).unwrap();
        let history = vec![
            m("user", "The name of my cat is Biscuit"),
            m("assistant", "|| What a lovely name for a cat! ||"),
            m("user", "what's the weather like"),
            m("assistant", "|| sunny ||"),
        ];
        chat_session::save_history(&session.id, &history, None).unwrap();
        let cfg = keyword_cfg();
        let in_prompt = ids(&[&history[2], &history[3]]);

        let found = recall(
            &cfg,
            &session.id,
            None,
            "what was my cat called?",
            &in_prompt,
        )
        .await;
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found
            .iter()
            .any(|r| r.role == "user" && r.text.contains("Biscuit")));
        assert!(found.iter().all(|r| r.ts.is_none()), "backfilled: no time");

        let in_prompt = ids(&[&history[0], &history[1], &history[2], &history[3]]);
        assert!(recall(&cfg, &session.id, None, "my cat", &in_prompt)
            .await
            .is_empty());
        let excluded = ids(&[&history[0], &history[1]]);
        assert!(recall(&cfg, &session.id, None, "my cat", &excluded)
            .await
            .is_empty());

        let off = AppConfig {
            chat_recall: false,
            ..keyword_cfg()
        };
        assert!(recall(&off, &session.id, None, "my cat", &HashSet::new())
            .await
            .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_persona_scope_crosses_chats_and_the_session_scope_does_not() {
        let dir = test_dir("scope");
        let a = chat_session::create_session(Some("Aria")).unwrap();
        let b = chat_session::create_session(Some("Aria")).unwrap();
        let other = chat_session::create_session(Some("Zed")).unwrap();
        let planted = m("user", "My cat is called Biscuit");
        chat_session::save_history(&b.id, std::slice::from_ref(&planted), None).unwrap();
        chat_session::save_history(&a.id, &[m("user", "hello there")], None).unwrap();
        chat_session::save_history(&other.id, &[m("user", "My cat is called Mittens")], None)
            .unwrap();

        let session_scope = keyword_cfg();
        let found = recall(
            &session_scope,
            &a.id,
            Some("Aria"),
            "my cat",
            &HashSet::new(),
        )
        .await;
        assert!(found.is_empty(), "{found:?}");

        let persona_scope = AppConfig {
            chat_recall_scope: "persona".into(),
            ..keyword_cfg()
        };
        let found = recall(
            &persona_scope,
            &a.id,
            Some("Aria"),
            "my cat",
            &HashSet::new(),
        )
        .await;
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].text.contains("Biscuit"), "other personas stay out");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deleting_a_session_removes_its_rows() {
        let dir = test_dir("delete");
        let path = db_path();
        let a = chat_session::create_session(None).unwrap();
        index_messages(&a.id, None, &[m("user", "remember the lighthouse")]).await;
        index_messages(
            "session-other",
            None,
            &[m("user", "something else entirely")],
        )
        .await;
        let cfg = keyword_cfg();
        chat_session::save_history(&a.id, &[m("user", "remember the lighthouse")], None).unwrap();
        recall(&cfg, &a.id, None, "lighthouse", &HashSet::new()).await;
        chat_session::delete_session(&a.id).await.unwrap();
        assert_eq!(
            row_count(&path),
            1,
            "only the other session's chunk is left"
        );
        assert_eq!(fts_count(&path, "lighthouse"), 0);
        let marked: i64 = with_db(&path, |c| {
            Ok(c.query_row("SELECT COUNT(*) FROM meta", [], |r| r.get(0))?)
        })
        .unwrap();
        assert_eq!(marked, 0, "the backfill marker goes too");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fusion_ranks_agreement_first_and_honours_the_cap() {
        // 2 is in both lists; 1 and 3 each top one list.
        assert_eq!(fuse(&[1, 2], &[3, 2], 10, |_| 1.0), vec![2, 1, 3]);
        assert_eq!(fuse(&[1, 2, 3], &[], 2, |_| 1.0), vec![1, 2]);
        assert_eq!(fuse(&[], &[], 4, |_| 1.0), Vec::<i64>::new());
        assert_eq!(
            fuse(&[5], &[6], 4, |_| 1.0),
            vec![5, 6],
            "ties keep keyword first"
        );
        // The user's own words outrank an equally-ranked character reply.
        assert_eq!(
            fuse(&[7], &[8], 4, |id| if id == 7 {
                ASSISTANT_WEIGHT
            } else {
                1.0
            }),
            vec![8, 7]
        );
    }

    #[test]
    fn snippets_merge_consecutive_chunks_and_fit_the_budget() {
        let row = |id, msg: &str, text: &str| ChunkRow {
            id,
            session_id: "s".into(),
            message_id: msg.into(),
            role: "user".into(),
            ts: Some(1),
            text: text.into(),
        };
        let items = assemble(vec![
            row(11, "b", "second"),
            row(5, "a", "one"),
            row(6, "a", "two"),
            row(9, "a", "four"),
        ]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "second", "rank order of first appearance");
        assert_eq!(items[1].text, "one two … four");

        let big = |t: &str| Recalled {
            role: "user".into(),
            text: t.repeat(700),
            ts: None,
        };
        let kept = fit_chars(vec![big("a"), big("b")], 1200);
        assert_eq!(kept.len(), 1, "lowest-ranked trimmed first");
        let lone = fit_chars(vec![big("a"), big("b")][..1].to_vec(), 600);
        assert_eq!(lone[0].text.chars().count(), 600);
        assert!(lone[0].text.ends_with('…'));
    }

    #[test]
    fn embedding_blobs_round_trip_and_cosine_is_sane() {
        let v = vec![0.5f32, -1.25, 3.0, 0.0];
        assert_eq!(from_blob(&to_blob(&v)).unwrap(), v);
        assert!(from_blob(&[1, 2, 3]).is_none());
        assert!(from_blob(&[]).is_none());
        assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn the_similarity_threshold_filters_embedding_hits() {
        let dir = test_dir("threshold");
        let path = db_path();
        let scope = Scope {
            session_id: "s".into(),
            persona: None,
        };
        with_db(&path, |conn| {
            let tx = conn.transaction()?;
            for (msg, text) in [("a", "close"), ("b", "far"), ("c", "mid")] {
                insert_message(&tx, "s", None, &m_with("user", text, msg), None)?;
            }
            tx.commit()?;
            store_embeddings(
                conn,
                "mod",
                vec![
                    (1, vec![1.0, 0.0]),
                    (2, vec![0.0, 1.0]),
                    (3, vec![1.0, 1.0]),
                ],
            )
        })
        .unwrap();
        let hits = with_db(&path, |c| {
            embedding_search(
                c,
                &scope,
                "mod",
                &[1.0, 0.0],
                &HashSet::new(),
                0.6,
                &HashSet::from([2]),
            )
        })
        .unwrap();
        // 3 (0.71) clears the 0.6 threshold but is too far below the best
        // match (1.0) to keep.
        assert_eq!(hits.top.iter().map(|h| h.0).collect::<Vec<_>>(), vec![1]);
        assert!(
            hits.scored[&2] < 0.6,
            "keyword hit 2 is scored, below threshold"
        );
        // A different model's vectors are ignored.
        let none = with_db(&path, |c| {
            embedding_search(
                c,
                &scope,
                "other",
                &[1.0, 0.0],
                &HashSet::new(),
                0.0,
                &HashSet::new(),
            )
        })
        .unwrap();
        assert!(none.top.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn m_with(role: &str, text: &str, id: &str) -> ChatMessage {
        let mut msg = ChatMessage::text(role, text);
        msg.id = id.into();
        msg
    }

    #[test]
    fn only_nomic_models_get_task_prefixes() {
        assert_eq!(
            with_prefix("text-embedding-nomic-embed-text-v1.5", Kind::Document, "hi"),
            "search_document: hi"
        );
        assert_eq!(
            with_prefix("nomic-embed-text", Kind::Query, "hi"),
            "search_query: hi"
        );
        assert_eq!(with_prefix("bge-m3", Kind::Query, "hi"), "hi");
    }

    fn embeddings_body(body: &str, vector_for: impl Fn(&str) -> Vec<f32>) -> String {
        let req: Value = serde_json::from_str(body).unwrap();
        let data: Vec<Value> = req["input"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, t)| json!({"index": i, "embedding": vector_for(t.as_str().unwrap())}))
            .collect();
        json!({"data": data}).to_string()
    }

    fn embedding_cfg(host: &str, model: &str) -> AppConfig {
        AppConfig {
            endpoint: format!("{host}/v1/chat/completions"),
            api_key: "secret".into(),
            chat_recall_embedding_model: model.into(),
            ..AppConfig::default()
        }
    }

    #[tokio::test]
    async fn embed_requests_carry_model_batch_prefix_and_bearer_key() {
        let (host, seen) =
            fake_server(|_, _, body| (200, embeddings_body(body, |_| vec![1.0, 0.0])));
        let cfg = embedding_cfg(&host, "text-embedding-nomic-embed-text-v1.5");
        let texts: Vec<String> = (0..40).map(|i| format!("text {i}")).collect();
        let out = embed_texts(
            &cfg,
            "text-embedding-nomic-embed-text-v1.5",
            Kind::Document,
            &texts,
        )
        .await
        .unwrap();
        assert_eq!(out.len(), 40);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "batched by {EMBED_BATCH}");
        assert_eq!(seen[0].0, "POST");
        assert_eq!(seen[0].1, "/v1/embeddings");
        let body: Value = serde_json::from_str(&seen[0].2).unwrap();
        assert_eq!(body["model"], "text-embedding-nomic-embed-text-v1.5");
        assert_eq!(body["input"].as_array().unwrap().len(), EMBED_BATCH);
        assert_eq!(body["input"][0], "search_document: text 0");
    }

    #[tokio::test]
    async fn llm_embed_sends_the_bearer_key_and_rejects_bad_bodies() {
        let (host, _) = fake_server(|_, _, _| (200, r#"{"data":[]}"#.into()));
        let err = llm::embed(
            &format!("{host}/v1/chat/completions"),
            "",
            "m",
            &["a".into()],
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("0 usable vectors"), "{err}");
        let (host, _) = fake_server(|_, _, _| (404, r#"{"error":"no such model"}"#.into()));
        let err = llm::embed(
            &format!("{host}/v1/chat/completions"),
            "",
            "m",
            &["a".into()],
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:?}").contains("404") || err.to_string().contains("404"),
            "{err}"
        );
        assert!(llm::embed("not a url", "", "m", &["a".into()])
            .await
            .is_err());
        assert!(llm::embed("http://127.0.0.1:1", "", "m", &[])
            .await
            .unwrap()
            .is_empty());
    }

    // The fake server can't see headers through `fake_server`, so the bearer
    // key is checked on a raw one-shot listener.
    #[tokio::test]
    async fn the_embedding_request_has_an_authorization_header() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let head = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).unwrap();
            let body = r#"{"data":[{"index":0,"embedding":[0.5]}]}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).to_lowercase()
        });
        let out = llm::embed(
            &format!("http://{addr}/v1/chat/completions"),
            "secret",
            "m",
            &["a".into()],
        )
        .await
        .unwrap();
        assert_eq!(out, vec![vec![0.5]]);
        assert!(head
            .join()
            .unwrap()
            .contains("authorization: bearer secret"));
    }

    /// Vector by content: "mentioned" -> [0,1]; "kitten"/"pet" -> [1,0.1]; else [1,0].
    fn topic_vectors(text: &str) -> Vec<f32> {
        if text.contains("mentioned") {
            vec![0.0, 1.0]
        } else if text.contains("kitten") || text.contains("pet") {
            vec![1.0, 0.1]
        } else {
            vec![1.0, 0.0]
        }
    }

    #[tokio::test]
    async fn embeddings_find_what_keywords_cannot() {
        let dir = test_dir("embed-recall");
        let (host, seen_all) =
            fake_server(|_, _, body| (200, embeddings_body(body, topic_vectors)));
        let cfg = embedding_cfg(&host, "nomic-embed-text");
        let session = chat_session::create_session(None).unwrap();
        let history = vec![
            m("user", "My kitten Biscuit loves tuna"),
            m("user", "The market was crowded yesterday mentioned nothing"),
        ];
        chat_session::save_history(&session.id, &history, None).unwrap();

        let found = recall(
            &cfg,
            &session.id,
            None,
            "what pet do I have",
            &HashSet::new(),
        )
        .await;
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].text.contains("kitten"));
        {
            let seen = seen_all.lock().unwrap();
            let queries = seen
                .iter()
                .filter(|(_, _, b)| b.contains("search_query: "))
                .count();
            assert_eq!(queries, 1);
            assert!(seen
                .iter()
                .any(|(_, _, b)| b.contains("search_document: My kitten")));
        }

        // Everything is embedded now: a second recall sends only the query.
        let documents = |seen: &crate::gpu::tests::Seen| {
            seen.lock()
                .unwrap()
                .iter()
                .filter(|(_, _, b)| b.contains("search_document: "))
                .count()
        };
        let before = documents(&seen_all);
        let again = recall(
            &cfg,
            &session.id,
            None,
            "what pet do I have",
            &HashSet::new(),
        )
        .await;
        assert_eq!(again.len(), 1);
        assert_eq!(documents(&seen_all), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_weak_keyword_hit_is_dropped_when_the_embedding_disagrees() {
        let dir = test_dir("embed-drop");
        let (host, _) = fake_server(|_, _, body| (200, embeddings_body(body, topic_vectors)));
        let session = chat_session::create_session(None).unwrap();
        let history = vec![
            m("user", "alpha is mentioned here in passing"),
            m("user", "beta unrelated words entirely"),
        ];
        chat_session::save_history(&session.id, &history, None).unwrap();

        let keyword_only = recall(
            &keyword_cfg(),
            &session.id,
            None,
            "alpha beta",
            &HashSet::new(),
        )
        .await;
        assert_eq!(keyword_only.len(), 2);

        let cfg = embedding_cfg(&host, "plain-model");
        let hybrid = recall(&cfg, &session.id, None, "alpha beta", &HashSet::new()).await;
        assert_eq!(hybrid.len(), 1, "{hybrid:?}");
        assert!(hybrid[0].text.starts_with("beta"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_dead_embedding_server_degrades_to_keyword_search() {
        let dir = test_dir("embed-dead");
        let cfg = embedding_cfg("http://127.0.0.1:1", "nomic-embed-text");
        let session = chat_session::create_session(None).unwrap();
        let history = vec![m("user", "My cat Biscuit loves tuna")];
        chat_session::save_history(&session.id, &history, None).unwrap();
        let found = recall(&cfg, &session.id, None, "which cat", &HashSet::new()).await;
        assert_eq!(found.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ages_read_naturally() {
        let now = 1_000_000_000;
        assert_eq!(relative_age(None, now), "earlier");
        assert_eq!(relative_age(Some(now - 10), now), "moments ago");
        assert_eq!(relative_age(Some(now - 60), now), "1 minute ago");
        assert_eq!(relative_age(Some(now - 3 * 3600), now), "3 hours ago");
        assert_eq!(relative_age(Some(now - 2 * 86400), now), "2 days ago");
        assert_eq!(relative_age(Some(now - 90 * 86400), now), "3 months ago");
        assert_eq!(relative_age(Some(now + 5), now), "moments ago");
        let block = format_block(&[
            Recalled {
                role: "user".into(),
                text: "a".into(),
                ts: None,
            },
            Recalled {
                role: "assistant".into(),
                text: "b".into(),
                ts: Some(now_secs() - 2 * 86400),
            },
        ]);
        assert!(block.starts_with("[From earlier in this chat."));
        assert!(block.contains("- (earlier) the user said: a"));
        assert!(block.contains("- (2 days ago) you said: b]"));
    }
}
