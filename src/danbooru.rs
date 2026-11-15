//! Danbooru tag data for the persona builder: a bundled SQLite list, an
//! optional folder of raw CSVs that replaces it, and an opt-in online lookup.
//!
//! Formats (comfyui-autocomplete-plus): `danbooru_tags.csv` is
//! `tag,category,count,alias` (alias is a quoted, comma separated list; about
//! 1.5 MB / 32,000 rows); `danbooru_tags_cooccurrence.csv` is
//! `tag_a,tag_b,count` (about 103 MB / 3.2 million rows, unsorted, each pair
//! once, counts written as floats). The bundled DB is built from them by
//! `scripts/build-danbooru-db.py` and keeps only character/general pairs.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::config::AppConfig;
use crate::{chat_director, paths};

pub const TAGS_FILE: &str = "danbooru_tags.csv";
pub const COOCCURRENCE_FILE: &str = "danbooru_tags_cooccurrence.csv";

/// The list shipped with the app (see `scripts/build-danbooru-db.py`).
const BUNDLED_DB: &[u8] = include_bytes!("../assets/danbooru-tags.sqlite");
/// The `meta.schema_version` this code reads.
const SCHEMA_VERSION: &str = "1";
/// Inputs beyond these are cut: a builder never sends more.
const MAX_TAGS_CHECKED: usize = 60;
const MAX_TAG_CHARS: usize = 80;
/// Co-occurring tags kept per tag by the folder layer.
const MAX_COOC_PER_TAG: usize = 400;
/// Folder co-occurrence results kept in memory (a scan takes about half a second).
const COOC_CACHE_ENTRIES: usize = 64;
const ONLINE_TIMEOUT: Duration = Duration::from_secs(8);
const ONLINE_BASE_URL: &str = "https://danbooru.donmai.us";
/// Candidates returned by a character search.
const CHARACTER_LIMIT: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    General,
    Artist,
    Copyright,
    Character,
    Meta,
    Other,
}

impl Category {
    fn from_id(id: i64) -> Self {
        match id {
            0 => Self::General,
            1 => Self::Artist,
            3 => Self::Copyright,
            4 => Self::Character,
            5 => Self::Meta,
            _ => Self::Other,
        }
    }
}

/// A tag as the data knows it: canonical name (underscores), category, post count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagInfo {
    pub name: String,
    pub category: Category,
    pub count: u64,
}

/// The local layer in use; `None` when neither opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagSource {
    Folder,
    Bundled,
    None,
}

/// Verified: exact local hit. Alias: local hit through an alias. Online: unknown
/// locally, found online. Unknown: every consulted source lacks it. Unverified:
/// no source could be consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagStatus {
    Verified,
    Alias,
    Online,
    Unknown,
    Unverified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagCheck {
    pub input: String,
    /// Underscore form, when found.
    pub canonical: Option<String>,
    /// Prompt form: spaces, parens escaped; not found = the cleaned input.
    pub display: String,
    pub status: TagStatus,
    pub category: Option<Category>,
    pub count: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagClass {
    Body,
    Outfit,
    Other,
}

// ---------------------------------------------------------------------------
// Name helpers
// ---------------------------------------------------------------------------

/// Trim, lowercase, unescape parens, whitespace runs -> `_`, drop a trailing `.`/`,`.
pub fn normalize(raw: &str) -> String {
    let lower = raw
        .trim()
        .trim_end_matches(['.', ','])
        .to_lowercase()
        .replace("\\(", "(")
        .replace("\\)", ")");
    lower.split_whitespace().collect::<Vec<_>>().join("_")
}

/// Prompt form of a canonical name: spaces, parens escaped. Idempotent after `normalize`.
pub fn display(name: &str) -> String {
    chat_director::escape_parens(&name.replace('_', " "))
}

/// Splits trailing `_(group)` suffixes off a name; groups come back last-first.
fn split_groups(name: &str) -> (&str, Vec<&str>) {
    let mut rest = name;
    let mut groups = Vec::new();
    while rest.ends_with(')') {
        match rest.rfind("_(") {
            Some(i) => {
                groups.push(&rest[i + 2..rest.len() - 1]);
                rest = &rest[..i];
            }
            None => break,
        }
    }
    (rest, groups)
}

/// `a_(x)_(fate)` -> `a_(fate)`. One or zero groups -> `None`.
pub fn general_character_name(name: &str) -> Option<String> {
    let (base, groups) = split_groups(name);
    (groups.len() >= 2).then(|| format!("{base}_({})", groups[0]))
}

const BODY_LAST: &[&str] = &[
    "hair", "eyes", "breasts", "skin", "ears", "tail", "horns", "wings", "fang", "freckles", "mole",
];
const BODY_EXACT: &[&str] = &[
    "flat_chest",
    "ahoge",
    "dark-skinned_female",
    "dark-skinned_male",
    "muscular",
    "abs",
    "tall",
    "petite",
    "heterochromia",
];
/// Hairstyles are here on purpose: the card keeps the hairstyle per outfit.
const OUTFIT_TOKENS: &[&str] = &[
    "bikini",
    "swimsuit",
    "shirt",
    "t-shirt",
    "skirt",
    "miniskirt",
    "dress",
    "kimono",
    "yukata",
    "hakama",
    "obi",
    "thighhighs",
    "pantyhose",
    "socks",
    "kneehighs",
    "stockings",
    "choker",
    "gloves",
    "boots",
    "shoes",
    "sneakers",
    "sandals",
    "geta",
    "hat",
    "cap",
    "ribbon",
    "bow",
    "bowtie",
    "jacket",
    "coat",
    "hoodie",
    "sweater",
    "cardigan",
    "vest",
    "shorts",
    "pants",
    "jeans",
    "uniform",
    "serafuku",
    "apron",
    "armor",
    "cape",
    "cloak",
    "scarf",
    "necktie",
    "shrug",
    "sleeves",
    "leotard",
    "bodysuit",
    "bra",
    "panties",
    "lingerie",
    "underwear",
    "swimwear",
    "glasses",
    "earrings",
    "necklace",
    "bracelet",
    "ring",
    "hairband",
    "hairclip",
    "headband",
    "headpiece",
    "headphones",
    "crown",
    "tiara",
    "mask",
    "belt",
    "sash",
    "veil",
    "hood",
    "katana",
    "sword",
    "weapon",
    "gown",
    "robe",
    "corset",
    "garter",
    "leggings",
    "tights",
    "collar",
    "pendant",
    "ornament",
    "bandages",
    "bandeau",
    "sarong",
    "pareo",
    "halo",
    "ponytail",
    "twintails",
    "braid",
    "braids",
    "bun",
    "pigtails",
];

/// Whole-token classification on the `_`-split name, parenthesised groups ignored.
pub fn classify(name: &str) -> TagClass {
    let (base, _) = split_groups(name);
    if BODY_EXACT.contains(&base) {
        return TagClass::Body;
    }
    let tokens: Vec<&str> = base.split('_').collect();
    if tokens.last().is_some_and(|t| BODY_LAST.contains(t)) {
        return TagClass::Body;
    }
    if tokens.iter().any(|t| OUTFIT_TOKENS.contains(t)) {
        return TagClass::Outfit;
    }
    TagClass::Other
}

/// Alphanumeric words of at least three chars, for series matching.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
        .collect()
}

/// Does a parenthesised group of `name` match the series (slug inside it, or a shared word)?
fn series_matches(name: &str, series_slug: &str, series_words: &[String]) -> bool {
    if series_slug.is_empty() {
        return false;
    }
    let (_, groups) = split_groups(name);
    groups
        .iter()
        .any(|g| g.contains(series_slug) || words(g).iter().any(|w| series_words.contains(w)))
}

/// De-duplicates, ranks (series match first, then count) and truncates.
fn rank_characters(mut list: Vec<TagInfo>, series: &str, limit: usize) -> Vec<TagInfo> {
    let slug = normalize(series);
    let series_words = words(series);
    let mut seen = std::collections::HashSet::new();
    list.retain(|t| seen.insert(t.name.clone()));
    list.sort_by(|a, b| {
        let ma = series_matches(&a.name, &slug, &series_words);
        let mb = series_matches(&b.name, &slug, &series_words);
        mb.cmp(&ma)
            .then(b.count.cmp(&a.count))
            .then(a.name.cmp(&b.name))
    });
    list.truncate(limit);
    list
}

/// The same name with two words swapped (`a_b` -> `b_a`), when it has exactly two.
fn reversed_words(slug: &str) -> Option<String> {
    let mut parts = slug.split('_');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(b), None) if !a.is_empty() && !b.is_empty() => Some(format!("{b}_{a}")),
        _ => None,
    }
}

fn is_character_match(name: &str, slug: &str) -> bool {
    name == slug || name.strip_prefix(slug).is_some_and(|r| r.starts_with("_("))
}

// ---------------------------------------------------------------------------
// CSV helpers (folder layer)
// ---------------------------------------------------------------------------

/// One CSV line, RFC 4180 quotes (`""` is a literal quote). No embedded newlines.
fn split_csv_line(line: &str) -> Vec<Cow<'_, str>> {
    let line = line.trim_end_matches(['\r', '\n']);
    if !line.contains('"') {
        return line.split(',').map(Cow::Borrowed).collect();
    }
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => fields.push(Cow::Owned(std::mem::take(&mut cur))),
            _ => cur.push(c),
        }
    }
    fields.push(Cow::Owned(cur));
    fields
}

/// `u64`, else an `f64` truncated (the co-occurrence file writes `12.0`).
fn parse_count(text: &str) -> Option<u64> {
    let text = text.trim();
    text.parse::<u64>().ok().or_else(|| {
        text.parse::<f64>()
            .ok()
            .filter(|f| *f >= 0.0)
            .map(|f| f as u64)
    })
}

struct FolderIndex {
    tags: HashMap<String, (Category, u64)>,
    aliases: HashMap<String, String>,
}

impl FolderIndex {
    fn parse(text: &str) -> Self {
        let mut tags: HashMap<String, (Category, u64)> = HashMap::new();
        let mut pending: Vec<(String, String, u64)> = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let fields = split_csv_line(line);
            if fields.len() < 3 {
                continue;
            }
            if i == 0 && matches!(fields[0].trim(), "tag" | "name") {
                continue;
            }
            let (Ok(cat), Some(count)) = (fields[1].trim().parse::<i64>(), parse_count(&fields[2]))
            else {
                continue;
            };
            let name = fields[0].trim().to_string();
            if name.is_empty() {
                continue;
            }
            if let Some(list) = fields.get(3) {
                for alias in list.split(',') {
                    let alias = normalize(alias);
                    if !alias.is_empty() {
                        pending.push((alias, name.clone(), count));
                    }
                }
            }
            tags.insert(name, (Category::from_id(cat), count));
        }
        let mut best: HashMap<String, (String, u64)> = HashMap::new();
        for (alias, canonical, count) in pending {
            if tags.contains_key(&alias) {
                continue;
            }
            match best.get(&alias) {
                Some((_, c)) if *c >= count => {}
                _ => {
                    best.insert(alias, (canonical, count));
                }
            }
        }
        let aliases = best.into_iter().map(|(a, (c, _))| (a, c)).collect();
        Self { tags, aliases }
    }

    fn info(&self, name: &str) -> Option<TagInfo> {
        self.tags.get(name).map(|(category, count)| TagInfo {
            name: name.to_string(),
            category: *category,
            count: *count,
        })
    }
}

type FileStamp = (PathBuf, Option<SystemTime>, u64);

fn stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((path.to_path_buf(), meta.modified().ok(), meta.len()))
}

type IndexCache = HashMap<PathBuf, (FileStamp, Arc<FolderIndex>)>;

fn index_cache() -> &'static Mutex<IndexCache> {
    static CACHE: OnceLock<Mutex<IndexCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The tag index of `dir/TAGS_FILE`, cached by (path, mtime, len).
fn load_folder_index(dir: &Path) -> anyhow::Result<Arc<FolderIndex>> {
    let path = dir.join(TAGS_FILE);
    let stamp = stamp(&path).with_context(|| format!("can't read {}", path.display()))?;
    {
        let cache = index_cache().lock().expect("lock poisoned");
        if let Some((s, index)) = cache.get(&path) {
            if *s == stamp {
                return Ok(Arc::clone(index));
            }
        }
    }
    let bytes = std::fs::read(&path).with_context(|| format!("can't read {}", path.display()))?;
    let index = Arc::new(FolderIndex::parse(&String::from_utf8_lossy(&bytes)));
    if index.tags.is_empty() {
        anyhow::bail!("{} has no tags", path.display());
    }
    index_cache()
        .lock()
        .expect("lock poisoned")
        .insert(path, (stamp, Arc::clone(&index)));
    Ok(index)
}

type CoocKey = (FileStamp, String);

type CoocCache = HashMap<CoocKey, Vec<(String, u64)>>;

fn cooc_cache() -> &'static Mutex<CoocCache> {
    static CACHE: OnceLock<Mutex<CoocCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Splits one co-occurrence line into (a, b, count); fast path without quotes.
fn parse_cooc_line(line: &str) -> Option<(Cow<'_, str>, Cow<'_, str>, u64)> {
    let line = line.trim_end_matches(['\r', '\n']);
    if !line.contains('"') {
        let first = line.find(',')?;
        let last = line.rfind(',')?;
        if first == last {
            return None;
        }
        let count = parse_count(&line[last + 1..])?;
        return Some((
            Cow::Borrowed(&line[..first]),
            Cow::Borrowed(&line[first + 1..last]),
            count,
        ));
    }
    let fields = split_csv_line(line);
    if fields.len() < 3 {
        return None;
    }
    let count = parse_count(&fields[2])?;
    let mut it = fields.into_iter();
    Some((it.next()?, it.next()?, count))
}

/// One pass over the file; the OTHER side of every pair that touches a wanted tag.
fn scan_cooccurrence(
    path: &Path,
    wanted: &[String],
) -> anyhow::Result<HashMap<String, Vec<(String, u64)>>> {
    let file =
        std::fs::File::open(path).with_context(|| format!("can't open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut buf = Vec::new();
    let mut found: HashMap<String, Vec<(String, u64)>> =
        wanted.iter().map(|t| (t.clone(), Vec::new())).collect();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let Some((a, b, count)) = parse_cooc_line(&line) else {
            continue;
        };
        if let Some(list) = found.get_mut(a.as_ref()) {
            list.push((b.to_string(), count));
        }
        if let Some(list) = found.get_mut(b.as_ref()) {
            list.push((a.to_string(), count));
        }
    }
    for list in found.values_mut() {
        list.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
        list.truncate(MAX_COOC_PER_TAG);
    }
    Ok(found)
}

// ---------------------------------------------------------------------------
// Bundled layer
// ---------------------------------------------------------------------------

/// Writes the embedded DB into `config_dir/cache` when missing or of another size,
/// drops older copies, returns its path.
fn extract_bundled(config_dir: &Path) -> anyhow::Result<PathBuf> {
    let dir = config_dir.join("cache");
    let target = dir.join(format!(
        "danbooru-tags-{}-{}.sqlite",
        env!("CARGO_PKG_VERSION"),
        BUNDLED_DB.len()
    ));
    let up_to_date = std::fs::metadata(&target)
        .map(|m| m.len() == BUNDLED_DB.len() as u64)
        .unwrap_or(false);
    if !up_to_date {
        std::fs::create_dir_all(&dir)?;
        paths::write_atomic(&target, BUNDLED_DB, Some(0o600))?;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                let old = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("danbooru-tags-") && n.ends_with(".sqlite"));
                if old && p != target {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }
    Ok(target)
}

fn open_bundled_db(config_dir: &Path) -> anyhow::Result<Connection> {
    let path = extract_bundled(config_dir)?;
    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if version.as_deref() != Some(SCHEMA_VERSION) {
        anyhow::bail!("unexpected tag database schema {version:?}");
    }
    Ok(conn)
}

fn row_info(row: &rusqlite::Row<'_>) -> rusqlite::Result<TagInfo> {
    Ok(TagInfo {
        name: row.get(0)?,
        category: Category::from_id(row.get(1)?),
        count: row.get::<_, i64>(2)?.max(0) as u64,
    })
}

fn bundled_cache() -> &'static Mutex<HashMap<PathBuf, Arc<TagData>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<TagData>>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn open_bundled(config_dir: &Path) -> Option<Arc<TagData>> {
    let mut cache = bundled_cache().lock().expect("lock poisoned");
    if let Some(data) = cache.get(config_dir) {
        return Some(Arc::clone(data));
    }
    match open_bundled_db(config_dir) {
        Ok(conn) => {
            let data = Arc::new(TagData {
                inner: Inner::Bundled(Mutex::new(conn)),
            });
            cache.insert(config_dir.to_path_buf(), Arc::clone(&data));
            Some(data)
        }
        Err(e) => {
            log::warn!("danbooru: the built-in tag list is unavailable: {e:#}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// TagData
// ---------------------------------------------------------------------------

enum Inner {
    Folder {
        index: Arc<FolderIndex>,
        dir: PathBuf,
        /// Used for co-occurrence when the folder has no such file.
        fallback: Option<Arc<TagData>>,
    },
    Bundled(Mutex<Connection>),
}

/// The local layer: the folder override when it loads, else the bundled DB.
pub struct TagData {
    inner: Inner,
}

impl TagData {
    /// Cached; `None` when neither layer opens.
    pub fn open(cfg: &AppConfig) -> Option<Arc<TagData>> {
        Self::open_in(cfg, &paths::app_config_dir())
    }

    pub(crate) fn open_in(cfg: &AppConfig, config_dir: &Path) -> Option<Arc<TagData>> {
        let dir = cfg.danbooru_tags_dir.trim();
        if !dir.is_empty() {
            match load_folder_index(Path::new(dir)) {
                Ok(index) => {
                    return Some(Arc::new(TagData {
                        inner: Inner::Folder {
                            index,
                            dir: PathBuf::from(dir),
                            fallback: open_bundled(config_dir),
                        },
                    }));
                }
                Err(e) => log::warn!("danbooru: tag folder not used: {e:#}"),
            }
        }
        open_bundled(config_dir)
    }

    pub fn source(&self) -> TagSource {
        match self.inner {
            Inner::Folder { .. } => TagSource::Folder,
            Inner::Bundled(_) => TagSource::Bundled,
        }
    }

    /// Exact lookup by canonical name.
    fn lookup(&self, name: &str) -> Option<TagInfo> {
        match &self.inner {
            Inner::Folder { index, .. } => index.info(name),
            Inner::Bundled(conn) => {
                let conn = conn.lock().expect("lock poisoned");
                conn.query_row(
                    "SELECT name, category, count FROM tags WHERE name = ?1",
                    [name],
                    row_info,
                )
                .optional()
                .unwrap_or_else(|e| {
                    log::warn!("danbooru: lookup failed: {e}");
                    None
                })
            }
        }
    }

    /// A tag by name or alias; the flag says it came through an alias.
    pub fn resolve(&self, raw: &str) -> Option<(TagInfo, bool)> {
        let name = normalize(raw);
        if name.is_empty() {
            return None;
        }
        if let Some(info) = self.lookup(&name) {
            return Some((info, false));
        }
        let canonical = match &self.inner {
            Inner::Folder { index, .. } => index.aliases.get(&name).cloned(),
            Inner::Bundled(conn) => {
                let conn = conn.lock().expect("lock poisoned");
                conn.query_row(
                    "SELECT t.name FROM aliases a JOIN tags t ON t.id = a.tag_id WHERE a.alias = ?1",
                    [&name],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .unwrap_or_else(|e| {
                    log::warn!("danbooru: alias lookup failed: {e}");
                    None
                })
            }
        }?;
        self.lookup(&canonical).map(|info| (info, true))
    }

    /// For a character tag: its general tag when that exists with at least the
    /// variant's count, else the tag itself. Non-character -> `None`.
    pub fn general_character(&self, name: &str) -> Option<TagInfo> {
        let info = self.lookup(name)?;
        if info.category != Category::Character {
            return None;
        }
        if let Some(general) = general_character_name(name).and_then(|g| self.lookup(&g)) {
            if general.category == Category::Character && general.count >= info.count {
                return Some(general);
            }
        }
        Some(info)
    }

    /// Character tags whose general name is `general`, most used first.
    pub fn variants_of(&self, general: &str, limit: usize) -> Vec<TagInfo> {
        let (base, groups) = split_groups(general);
        if groups.len() != 1 {
            return Vec::new();
        }
        let mut out: Vec<TagInfo> = self
            .characters_with_prefix(&format!("{base}_("))
            .into_iter()
            .filter(|t| general_character_name(&t.name).as_deref() == Some(general))
            .collect();
        out.sort_by(|a, b| b.count.cmp(&a.count).then(a.name.cmp(&b.name)));
        out.truncate(limit);
        out
    }

    /// Character tags starting with `prefix`.
    fn characters_with_prefix(&self, prefix: &str) -> Vec<TagInfo> {
        match &self.inner {
            Inner::Folder { index, .. } => index
                .tags
                .iter()
                .filter(|(name, (cat, _))| *cat == Category::Character && name.starts_with(prefix))
                .map(|(name, (category, count))| TagInfo {
                    name: name.clone(),
                    category: *category,
                    count: *count,
                })
                .collect(),
            Inner::Bundled(conn) => {
                let conn = conn.lock().expect("lock poisoned");
                let run = || -> rusqlite::Result<Vec<TagInfo>> {
                    let mut stmt = conn.prepare_cached(
                        "SELECT name, category, count FROM tags \
                         WHERE name >= ?1 AND name < ?1 || '~' AND category = 4 LIMIT 2000",
                    )?;
                    let rows = stmt.query_map([prefix], row_info)?;
                    rows.collect()
                };
                run().unwrap_or_else(|e| {
                    log::warn!("danbooru: prefix search failed: {e}");
                    Vec::new()
                })
            }
        }
    }

    /// Character candidates for a name (either word order, or an alias), collapsed
    /// to their general tags and ranked by series match, then count.
    pub fn find_characters(&self, name: &str, series: &str, limit: usize) -> Vec<TagInfo> {
        let slug = normalize(name);
        if slug.is_empty() {
            return Vec::new();
        }
        let mut slugs = vec![slug.clone()];
        slugs.extend(reversed_words(&slug));
        let mut found = Vec::new();
        for s in &slugs {
            if let Some(info) = self.lookup(s).filter(|t| t.category == Category::Character) {
                found.push(info);
            }
            found.extend(
                self.characters_with_prefix(&format!("{s}_("))
                    .into_iter()
                    .filter(|t| is_character_match(&t.name, s)),
            );
        }
        if let Some((info, _)) = self.resolve(name) {
            if info.category == Category::Character {
                found.push(info);
            }
        }
        let collapsed = found
            .into_iter()
            .map(|t| self.general_character(&t.name).unwrap_or(t))
            .collect();
        rank_characters(collapsed, series, limit)
    }

    /// Tags that co-occur with each of `tags`, most frequent first.
    ///
    /// Blocking (the folder layer scans 100 MB): call inside `spawn_blocking`.
    pub fn cooccurring(
        &self,
        tags: &[String],
    ) -> anyhow::Result<HashMap<String, Vec<(String, u64)>>> {
        match &self.inner {
            Inner::Bundled(conn) => {
                let conn = conn.lock().expect("lock poisoned");
                let mut stmt = conn.prepare_cached(
                    "SELECT o.name, c.count FROM tags t \
                     JOIN cooccurrence c ON c.tag_id = t.id \
                     JOIN tags o ON o.id = c.other_id \
                     WHERE t.name = ?1 ORDER BY c.count DESC, o.name LIMIT ?2",
                )?;
                let mut out = HashMap::new();
                for tag in tags {
                    let rows = stmt
                        .query_map(rusqlite::params![tag, MAX_COOC_PER_TAG as i64], |r| {
                            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
                        })?;
                    out.insert(tag.clone(), rows.collect::<rusqlite::Result<Vec<_>>>()?);
                }
                Ok(out)
            }
            Inner::Folder { dir, fallback, .. } => {
                let path = dir.join(COOCCURRENCE_FILE);
                let Some(stamp) = stamp(&path) else {
                    return match fallback {
                        Some(data) => data.cooccurring(tags),
                        None => Ok(HashMap::new()),
                    };
                };
                let mut out = HashMap::new();
                let mut missing = Vec::new();
                {
                    let cache = cooc_cache().lock().expect("lock poisoned");
                    for tag in tags {
                        match cache.get(&(stamp.clone(), tag.clone())) {
                            Some(list) => {
                                out.insert(tag.clone(), list.clone());
                            }
                            None => missing.push(tag.clone()),
                        }
                    }
                }
                if !missing.is_empty() {
                    let scanned = scan_cooccurrence(&path, &missing)?;
                    let mut cache = cooc_cache().lock().expect("lock poisoned");
                    if cache.len() + scanned.len() > COOC_CACHE_ENTRIES {
                        cache.clear();
                    }
                    for (tag, list) in scanned {
                        cache.insert((stamp.clone(), tag.clone()), list.clone());
                        out.insert(tag, list);
                    }
                }
                Ok(out)
            }
        }
    }
}

pub fn tag_source(cfg: &AppConfig) -> TagSource {
    TagData::open(cfg).map_or(TagSource::None, |d| d.source())
}

// ---------------------------------------------------------------------------
// Online layer
// ---------------------------------------------------------------------------

fn online_base_url() -> &'static str {
    ONLINE_BASE_URL
}

/// Tags out of a `tags.json` response; deprecated or unused tags count as not found.
pub fn parse_online_tags(body: &serde_json::Value) -> Vec<TagInfo> {
    let Some(items) = body.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let name = item.get("name")?.as_str()?;
            let count = item.get("post_count")?.as_u64()?;
            let deprecated = item
                .get("is_deprecated")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if deprecated || count == 0 || name.is_empty() {
                return None;
            }
            Some(TagInfo {
                name: name.to_string(),
                category: Category::from_id(item.get("category")?.as_i64()?),
                count,
            })
        })
        .collect()
}

const ONLINE_FIELDS: &str = "name,category,post_count,is_deprecated";

async fn online_tags(base_url: &str, params: &[(&str, &str)]) -> anyhow::Result<Vec<TagInfo>> {
    let client = reqwest::Client::builder()
        .timeout(ONLINE_TIMEOUT)
        .user_agent(concat!("llm-assistant/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let body: serde_json::Value = client
        .get(format!("{base_url}/tags.json"))
        .query(params)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(parse_online_tags(&body))
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// `(x:1.2)` weights and `lora:`/`embedding:` references are not tags.
fn is_passthrough(tag: &str) -> bool {
    let lower = tag.to_lowercase();
    lower.starts_with("lora:")
        || lower.starts_with("embedding:")
        || lower.starts_with("<lora:")
        || (lower.starts_with('(') && lower.contains(':'))
}

fn local_check(data: &TagData, input: &str) -> TagCheck {
    let cleaned = normalize(input);
    match data.resolve(&cleaned) {
        Some((info, via_alias)) => TagCheck {
            input: input.to_string(),
            display: display(&info.name),
            canonical: Some(info.name),
            status: if via_alias {
                TagStatus::Alias
            } else {
                TagStatus::Verified
            },
            category: Some(info.category),
            count: Some(info.count),
        },
        None => TagCheck {
            input: input.to_string(),
            canonical: None,
            display: display(&cleaned),
            status: TagStatus::Unknown,
            category: None,
            count: None,
        },
    }
}

async fn check_tags_at(
    cfg: &AppConfig,
    tags: &[String],
    base_url: &str,
) -> (TagSource, Vec<TagCheck>) {
    let inputs: Vec<String> = tags
        .iter()
        .map(|t| t.trim().chars().take(MAX_TAG_CHARS).collect::<String>())
        .filter(|t| !t.is_empty())
        .take(MAX_TAGS_CHECKED)
        .collect();
    let cfg_local = cfg.clone();
    let local_inputs = inputs.clone();
    let local = tokio::task::spawn_blocking(move || {
        let data = TagData::open(&cfg_local)?;
        Some((
            data.source(),
            local_inputs
                .iter()
                .map(|t| (!is_passthrough(t)).then(|| local_check(&data, t)))
                .collect::<Vec<_>>(),
        ))
    })
    .await
    .unwrap_or_else(|e| {
        log::warn!("danbooru: tag check task failed: {e}");
        None
    });
    let (source, mut checks): (TagSource, Vec<Option<TagCheck>>) = match local {
        Some((source, checks)) => (source, checks),
        None => (TagSource::None, inputs.iter().map(|_| None).collect()),
    };
    // Entries still None are pass-through; fill them in order.
    let no_local = source == TagSource::None;
    let mut out: Vec<TagCheck> = inputs
        .iter()
        .zip(checks.drain(..))
        .map(|(input, check)| {
            check.unwrap_or_else(|| TagCheck {
                input: input.clone(),
                canonical: None,
                display: input.clone(),
                status: TagStatus::Unverified,
                category: None,
                count: None,
            })
        })
        .collect();
    // Misses: tags that were actually looked up (not pass-through).
    let miss_idx: Vec<usize> = out
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            !is_passthrough(&inputs[*i]) && (no_local || c.status == TagStatus::Unknown)
        })
        .map(|(i, _)| i)
        .collect();
    if no_local {
        for &i in &miss_idx {
            let cleaned = normalize(&inputs[i]);
            out[i].display = display(&cleaned);
            out[i].status = TagStatus::Unverified;
        }
    }
    if cfg.danbooru_online_lookup && !miss_idx.is_empty() {
        let names: Vec<String> = miss_idx.iter().map(|&i| normalize(&inputs[i])).collect();
        let joined = names.join(",");
        let params = [
            ("search[name_comma]", joined.as_str()),
            ("limit", "100"),
            ("only", ONLINE_FIELDS),
        ];
        match online_tags(base_url, &params).await {
            Ok(found) => {
                let by_name: HashMap<&str, &TagInfo> =
                    found.iter().map(|t| (t.name.as_str(), t)).collect();
                for (&i, name) in miss_idx.iter().zip(&names) {
                    match by_name.get(name.as_str()) {
                        Some(info) => {
                            out[i] = TagCheck {
                                input: inputs[i].clone(),
                                canonical: Some(info.name.clone()),
                                display: display(&info.name),
                                status: TagStatus::Online,
                                category: Some(info.category),
                                count: Some(info.count),
                            };
                        }
                        None => out[i].status = TagStatus::Unknown,
                    }
                }
            }
            Err(e) => {
                log::warn!("danbooru: online tag lookup failed: {e:#}");
                for &i in &miss_idx {
                    out[i].status = TagStatus::Unknown;
                }
            }
        }
    }
    (source, out)
}

/// Checks tags against the local layer, then (opt-in) online. Input is capped
/// at 60 tags of 80 chars, empty entries dropped, order kept.
pub async fn check_tags(cfg: &AppConfig, tags: &[String]) -> (TagSource, Vec<TagCheck>) {
    check_tags_at(cfg, tags, online_base_url()).await
}

async fn find_characters_at(
    cfg: &AppConfig,
    name: &str,
    series: &str,
    base_url: &str,
) -> (TagSource, Vec<TagInfo>) {
    let cfg_local = cfg.clone();
    let (n, s) = (name.to_string(), series.to_string());
    let local = tokio::task::spawn_blocking(move || {
        TagData::open(&cfg_local).map(|d| (d.source(), d.find_characters(&n, &s, CHARACTER_LIMIT)))
    })
    .await
    .unwrap_or_else(|e| {
        log::warn!("danbooru: character search task failed: {e}");
        None
    });
    let (source, found) = local.unwrap_or((TagSource::None, Vec::new()));
    if !found.is_empty() || !cfg.danbooru_online_lookup {
        return (source, found);
    }
    let slug = normalize(name);
    if slug.is_empty() {
        return (source, found);
    }
    let pattern = format!("{slug}*");
    let params = [
        ("search[name_matches]", pattern.as_str()),
        ("search[category]", "4"),
        ("search[order]", "count"),
        ("limit", "20"),
        ("only", ONLINE_FIELDS),
    ];
    match online_tags(base_url, &params).await {
        Ok(tags) => {
            // No local data to check a general tag against: prefer one that is in the answer.
            let names: std::collections::HashSet<String> =
                tags.iter().map(|t| t.name.clone()).collect();
            let collapsed = tags
                .into_iter()
                .map(|t| match general_character_name(&t.name) {
                    Some(g) if names.contains(&g) => TagInfo { name: g, ..t },
                    _ => t,
                })
                .collect();
            (source, rank_characters(collapsed, series, CHARACTER_LIMIT))
        }
        Err(e) => {
            log::warn!("danbooru: online character search failed: {e:#}");
            (source, found)
        }
    }
}

/// Character candidates: the local layer first, online when it finds nothing and
/// the online lookup is on.
pub async fn find_characters(
    cfg: &AppConfig,
    name: &str,
    series: &str,
) -> (TagSource, Vec<TagInfo>) {
    find_characters_at(cfg, name, series, online_base_url()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_dir(label: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "llm-assistant-danbooru-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const TAGS_CSV: &str = "tag,category,count,alias\n\
1girl,0,4974288,\"女の子,girl,girls\"\n\
long_hair,0,3000000,\"longhair,long hair\"\n\
pink_hair,0,900000,\n\
swimsuit,0,500000,\n\
miyamoto_musashi_(fate),4,4097,\"musashi,miyamoto musashi\"\n\
miyamoto_musashi_(swimsuit_berserker)_(fate),4,1436,\n\
miyamoto_musashi_(swimsuit_berserker)_(second_ascension)_(fate),4,1064,\n\
musashi_miyamoto_(other),4,50,\n\
miyamoto_musashi_(gintama),4,70,\n\
orphan_(swimsuit_berserker)_(fate),4,30,\n\
fate/grand_order,3,200000,\n";

    const COOC_CSV: &str = "tag_a,tag_b,count\n\
miyamoto_musashi_(fate),pink_hair,2561.0\n\
long_hair,miyamoto_musashi_(fate),2503.0\n\
\"miyamoto_musashi_(swimsuit_berserker)_(fate)\",swimsuit,1000.0\n\
1girl,long_hair,2801554.0\n";

    fn folder(label: &str, tags: &str, cooc: Option<&str>) -> PathBuf {
        let dir = tmp_dir(label);
        std::fs::write(dir.join(TAGS_FILE), tags).unwrap();
        if let Some(c) = cooc {
            std::fs::write(dir.join(COOCCURRENCE_FILE), c).unwrap();
        }
        dir
    }

    fn folder_data(dir: &Path) -> Arc<TagData> {
        let cfg = AppConfig {
            danbooru_tags_dir: dir.display().to_string(),
            ..AppConfig::default()
        };
        let data = TagData::open_in(&cfg, &tmp_dir("cfg")).unwrap();
        assert_eq!(data.source(), TagSource::Folder);
        data
    }

    fn bundled_data() -> Arc<TagData> {
        let data = TagData::open_in(&AppConfig::default(), &tmp_dir("bundled")).unwrap();
        assert_eq!(data.source(), TagSource::Bundled);
        data
    }

    #[test]
    fn csv_header_skipped_and_quoted_aliases_parsed() {
        let index = FolderIndex::parse(TAGS_CSV);
        assert!(!index.tags.contains_key("tag"));
        assert_eq!(index.tags.len(), 11);
        assert_eq!(index.aliases.get("girl").map(String::as_str), Some("1girl"));
        assert_eq!(
            index.aliases.get("女の子").map(String::as_str),
            Some("1girl")
        );
        assert_eq!(split_csv_line("a,\"b,c\",\"d\"\"e\"").len(), 3);
        assert_eq!(split_csv_line("a,\"b,c\",\"d\"\"e\"")[2], "d\"e");
        // rows with too few fields or a bad count are skipped
        let bad = FolderIndex::parse("x,0\ny,0,notanumber\nz,0,5.0\n");
        assert_eq!(bad.tags.len(), 1);
        assert_eq!(bad.tags["z"].1, 5);
    }

    #[test]
    fn resolve_spelling_and_alias() {
        let data = folder_data(&folder("resolve", TAGS_CSV, None));
        for raw in ["long_hair", "Long Hair", "  long   hair, "] {
            assert_eq!(
                data.resolve(raw).unwrap().0.name,
                "long_hair",
                "input {raw:?}"
            );
        }
        let (info, via) = data.resolve("miyamoto musashi \\(fate\\)").unwrap();
        assert_eq!(
            (info.name.as_str(), via),
            ("miyamoto_musashi_(fate)", false)
        );
        let (info, via) = data.resolve("musashi").unwrap();
        assert_eq!((info.name.as_str(), via), ("miyamoto_musashi_(fate)", true));
        let (info, via) = data.resolve("longhair").unwrap();
        assert_eq!((info.name.as_str(), via), ("long_hair", true));
        assert!(data.resolve("not_a_tag").is_none());
    }

    #[test]
    fn general_character_name_cases() {
        assert_eq!(
            general_character_name("miyamoto_musashi_(swimsuit_berserker)_(fate)").as_deref(),
            Some("miyamoto_musashi_(fate)")
        );
        assert_eq!(
            general_character_name(
                "miyamoto_musashi_(swimsuit_berserker)_(second_ascension)_(fate)"
            )
            .as_deref(),
            Some("miyamoto_musashi_(fate)")
        );
        assert_eq!(general_character_name("miyamoto_musashi_(fate)"), None);
        assert_eq!(general_character_name("musashi"), None);
    }

    #[test]
    fn general_character_prefers_general_and_keeps_orphans() {
        let data = folder_data(&folder("general", TAGS_CSV, None));
        let g = data
            .general_character("miyamoto_musashi_(swimsuit_berserker)_(fate)")
            .unwrap();
        assert_eq!(g.name, "miyamoto_musashi_(fate)");
        let orphan = data
            .general_character("orphan_(swimsuit_berserker)_(fate)")
            .unwrap();
        assert_eq!(orphan.name, "orphan_(swimsuit_berserker)_(fate)");
        assert!(data.general_character("long_hair").is_none());
    }

    #[test]
    fn variants_of_sorted_by_count() {
        let data = folder_data(&folder("variants", TAGS_CSV, None));
        let v = data.variants_of("miyamoto_musashi_(fate)", 5);
        let names: Vec<_> = v.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "miyamoto_musashi_(swimsuit_berserker)_(fate)",
                "miyamoto_musashi_(swimsuit_berserker)_(second_ascension)_(fate)"
            ]
        );
        assert_eq!(data.variants_of("miyamoto_musashi_(fate)", 1).len(), 1);
    }

    #[test]
    fn find_characters_ranks_series_and_reversed_order() {
        let data = folder_data(&folder("find", TAGS_CSV, None));
        let found = data.find_characters("Miyamoto Musashi", "Fate/Grand Order", 8);
        assert_eq!(found[0].name, "miyamoto_musashi_(fate)");
        assert!(found.iter().any(|t| t.name == "miyamoto_musashi_(gintama)"));
        // variants collapse into the general tag, no duplicates
        assert_eq!(
            found
                .iter()
                .filter(|t| t.name == "miyamoto_musashi_(fate)")
                .count(),
            1
        );
        // gintama ranks first when the series says so
        let found = data.find_characters("miyamoto musashi", "gintama", 8);
        assert_eq!(found[0].name, "miyamoto_musashi_(gintama)");
        // reversed word order finds the tag
        let found = data.find_characters("miyamoto musashi", "other", 8);
        assert!(found.iter().any(|t| t.name == "musashi_miyamoto_(other)"));
        // an alias hit
        let found = data.find_characters("musashi", "", 8);
        assert!(found.iter().any(|t| t.name == "miyamoto_musashi_(fate)"));
    }

    #[test]
    fn folder_cooccurrence_both_directions_floats_quotes_and_missing_file() {
        let data = folder_data(&folder("cooc", TAGS_CSV, Some(COOC_CSV)));
        let wanted = vec![
            "miyamoto_musashi_(fate)".to_string(),
            "miyamoto_musashi_(swimsuit_berserker)_(fate)".to_string(),
        ];
        let got = data.cooccurring(&wanted).unwrap();
        assert_eq!(
            got["miyamoto_musashi_(fate)"],
            vec![
                ("pink_hair".to_string(), 2561),
                ("long_hair".to_string(), 2503)
            ]
        );
        assert_eq!(
            got["miyamoto_musashi_(swimsuit_berserker)_(fate)"],
            vec![("swimsuit".to_string(), 1000)]
        );
        // second call is served from the cache with the same answer
        assert_eq!(data.cooccurring(&wanted).unwrap(), got);
        // a missing file: the bundled layer answers (no panic, no error)
        let data = folder_data(&folder("nocooc", TAGS_CSV, None));
        assert!(data
            .cooccurring(&["miyamoto_musashi_(fate)".to_string()])
            .is_ok());
    }

    #[test]
    fn classify_table() {
        use TagClass::*;
        for (name, want) in [
            ("pink_hair", Body),
            ("blue_eyes", Body),
            ("large_breasts", Body),
            ("flat_chest", Body),
            ("dark-skinned_female", Body),
            ("fox_ears", Body),
            ("bikini", Outfit),
            ("american_flag_bikini", Outfit),
            ("white_thighhighs", Outfit),
            ("side_ponytail", Outfit),
            ("bun_cover", Outfit),
            ("hair_bun", Outfit),
            ("hair_ribbon", Outfit),
            ("smile", Other),
            ("looking_at_viewer", Other),
            ("solo", Other),
            ("long_hair_(meme)", Body),
        ] {
            assert_eq!(classify(name), want, "tag {name}");
        }
    }

    #[test]
    fn display_escapes_and_is_idempotent() {
        assert_eq!(
            display("miyamoto_musashi_(fate)"),
            "miyamoto musashi \\(fate\\)"
        );
        for raw in ["Miyamoto Musashi \\(Fate\\)", "long_hair", "a_(b)_(c)", "x"] {
            let once = display(&normalize(raw));
            assert_eq!(display(&normalize(&once)), once, "input {raw:?}");
        }
    }

    #[test]
    fn parse_online_tags_skips_deprecated_and_empty() {
        let body = serde_json::json!([
            {"name": "long_hair", "category": 0, "post_count": 10, "is_deprecated": false},
            {"name": "old_tag", "category": 0, "post_count": 10, "is_deprecated": true},
            {"name": "unused", "category": 0, "post_count": 0, "is_deprecated": false},
            {"name": "someone_(series)", "category": 4, "post_count": 5}
        ]);
        let tags = parse_online_tags(&body);
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[1].category, Category::Character);
        assert!(parse_online_tags(&serde_json::json!({"error": "x"})).is_empty());
    }

    #[tokio::test]
    async fn check_tags_leaves_weighted_tags_untouched() {
        let dir = folder("check", TAGS_CSV, None);
        let cfg = AppConfig {
            danbooru_tags_dir: dir.display().to_string(),
            ..AppConfig::default()
        };
        let tags = vec![
            "Long Hair".to_string(),
            "  ".to_string(),
            "(pink hair:1.2)".to_string(),
            "<lora:foo:0.8>".to_string(),
            "girls".to_string(),
            "zzz_nothing".to_string(),
        ];
        let (source, checks) = check_tags(&cfg, &tags).await;
        assert_eq!(source, TagSource::Folder);
        let status: Vec<_> = checks.iter().map(|c| c.status).collect();
        assert_eq!(
            status,
            [
                TagStatus::Verified,
                TagStatus::Unverified,
                TagStatus::Unverified,
                TagStatus::Alias,
                TagStatus::Unknown
            ]
        );
        assert_eq!(checks[1].display, "(pink hair:1.2)");
        assert_eq!(checks[2].display, "<lora:foo:0.8>");
        assert_eq!(checks[3].canonical.as_deref(), Some("1girl"));
        assert_eq!(checks[4].display, "zzz nothing");
    }

    #[test]
    fn bundled_db_content() {
        assert!(
            BUNDLED_DB.len() <= 6 * 1024 * 1024,
            "{} bytes",
            BUNDLED_DB.len()
        );
        let data = bundled_data();
        let (info, via) = data.resolve("1girl").unwrap();
        assert_eq!((info.category, via), (Category::General, false));
        let g = data
            .general_character("miyamoto_musashi_(swimsuit_berserker)_(fate)")
            .unwrap();
        assert_eq!(g.name, "miyamoto_musashi_(fate)");
        let variants = data.variants_of("miyamoto_musashi_(fate)", 3);
        assert_eq!(
            variants[0].name,
            "miyamoto_musashi_(swimsuit_berserker)_(fate)"
        );
        let cooc = data.cooccurring(std::slice::from_ref(&g.name)).unwrap();
        let list = &cooc[&g.name];
        assert!(!list.is_empty());
        for (name, _) in list {
            assert_eq!(
                data.resolve(name).unwrap().0.category,
                Category::General,
                "{name}"
            );
        }
        let found = data.find_characters("miyamoto musashi", "fate", 8);
        assert_eq!(found[0].name, "miyamoto_musashi_(fate)");
        // non-character tag: no co-occurrence in this layer
        assert!(data.cooccurring(&["1girl".to_string()]).unwrap()["1girl"].is_empty());
    }

    #[test]
    fn bundled_db_reports_schema_version() {
        let dir = tmp_dir("schema");
        let conn = open_bundled_db(&dir).unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "1");
    }

    #[test]
    fn folder_wins_and_empty_folder_falls_back() {
        let dir = folder("layer", "tag,category,count,alias\nonly_tag,0,5,\n", None);
        let data = folder_data(&dir);
        assert!(data.resolve("only_tag").is_some());
        assert!(data.resolve("1girl").is_none());
        let config_dir = tmp_dir("layer-cfg");
        for tags_dir in [tmp_dir("layer-empty").display().to_string(), String::new()] {
            let cfg = AppConfig {
                danbooru_tags_dir: tags_dir,
                ..AppConfig::default()
            };
            let data = TagData::open_in(&cfg, &config_dir).unwrap();
            assert_eq!(data.source(), TagSource::Bundled);
            assert!(data.resolve("1girl").is_some());
        }
    }
}
