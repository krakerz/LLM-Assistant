//! Wire protocol between the file-ops agent and a small local model: tool
//! schemas (native mode), a forgiving text-block parser (text mode), the
//! static system prompts, result formatting and the agent-loop note texts.
//! The parser accepts every shape a 1-4B model tends to emit; it never
//! executes anything and reports precise errors for near-miss blocks.

use crate::llm::{split_thinking, ToolCall, ToolSpec};
use crate::tools::{Edit, Tool};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Native,
    Text,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TextParse {
    Tool {
        tool: Tool,
        /// Reply text outside every tool block.
        prose: String,
        /// Further blocks after the first; they are ignored, not run.
        extra_blocks: usize,
    },
    Answer(String),
    Malformed {
        error: String,
        prose: String,
    },
}

// --- Agent-loop notes (pushed into history by agent.rs) ---

pub const DENIED_NOTE: &str = "[the user DENIED this. It did NOT run and nothing changed. Do not \
retry it; reply to the user.]";

pub const STOPPED_NOTE: &str = "[the user stopped the task. The last tool call may not have \
finished -- check before assuming it did.]";

pub const STEP_CAP_NOTE: &str = "[the step limit for this task was reached, so no more tool calls \
will run.]";

pub const CUT_OFF_NOTE: &str = "[your reply was cut off; keep tool calls shorter -- write big \
files in parts: write the first part, then add the rest with edit_file]";

pub const MALFORMED_NOTE_PREFIX: &str = "[your tool call could not be read: ";
pub const MALFORMED_NOTE_SUFFIX: &str =
    ". Reply again with exactly one block in the documented format.]";

pub fn malformed_note(error: &str) -> String {
    format!("{MALFORMED_NOTE_PREFIX}{error}{MALFORMED_NOTE_SUFFIX}")
}

pub const REPEATED_CALL_NOTE: &str = "[you made the exact same tool call again immediately after \
it already ran, with nothing new to justify it -- it was not run again. You already have its \
result above.]";

pub const STUCK_LOOP_NOTE: &str = "[that exact tool call has come up too many times in this task \
without moving anything forward -- it was not run again. You already have its result from earlier \
-- work from that, or try something genuinely different.]";

/// Wrap-up turn after a hard stop, tools off the table. Every clause is
/// load-bearing (same lessons as `rules::FINAL_ANSWER_PROMPT`): don't
/// presuppose success, and don't let trimmed context read as permission to
/// fill gaps.
pub const FINAL_ANSWER_PROMPT: &str = "[don't call any more tools. Reply now in plain text, with \
no tool call and no code fence, describing the CURRENT state strictly from the tool results you \
actually received above. If calls were denied, never ran, or failed, say that plainly -- do not \
describe any file as moved, created, edited, or deleted unless a tool result above shows it \
actually happened. If part of this conversation was summarized or dropped to save context, don't \
reconstruct what was in it: say you no longer have it rather than describing file contents you \
cannot see.]";

// --- Tool schemas ---

const TOOL_NAMES: [&str; 11] = [
    "list_dir",
    "read_file",
    "search",
    "find_files",
    "write_file",
    "edit_file",
    "move",
    "delete",
    "make_dir",
    "run_command",
    "finish",
];

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required})
}

fn spec(name: &str, description: &str, parameters: Value) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
    }
}

pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        spec(
            "list_dir",
            "List the files and folders in a folder. Folders end with /.",
            schema(
                json!({
                    "path": {"type": "string", "description": "Folder, relative to the working folder. Default \".\"."},
                    "depth": {"type": "integer", "description": "Levels to show, 1-3. Default 1."}
                }),
                &[],
            ),
        ),
        spec(
            "read_file",
            "Read a text file. Lines are numbered. Long files: use offset and limit to read in parts.",
            schema(
                json!({
                    "path": {"type": "string", "description": "File to read."},
                    "offset": {"type": "integer", "description": "First line to read, starting at 1."},
                    "limit": {"type": "integer", "description": "Number of lines. Default 200, max 1000."}
                }),
                &["path"],
            ),
        ),
        spec(
            "search",
            "Search inside files for text. Returns matching lines with file and line number.",
            schema(
                json!({
                    "pattern": {"type": "string", "description": "Text to look for."},
                    "path": {"type": "string", "description": "Folder or file to search. Default \".\"."},
                    "glob": {"type": "string", "description": "Only search files matching this, e.g. \"*.rs\"."},
                    "regex": {"type": "boolean", "description": "Treat pattern as a regular expression."},
                    "case_sensitive": {"type": "boolean", "description": "Default false."}
                }),
                &["pattern"],
            ),
        ),
        spec(
            "find_files",
            "Find files by name pattern, e.g. \"*.txt\" or \"**/test_*.py\".",
            schema(
                json!({
                    "glob": {"type": "string", "description": "Name pattern."},
                    "path": {"type": "string", "description": "Folder to search in. Default \".\"."}
                }),
                &["glob"],
            ),
        ),
        spec(
            "write_file",
            "Create a file or replace it completely with the given content. Missing folders are created. To add to the end of a file, set append. To change part of an existing file, use edit_file instead.",
            schema(
                json!({
                    "path": {"type": "string", "description": "File to write."},
                    "content": {"type": "string", "description": "The full new content of the file (or the lines to add when append is true)."},
                    "append": {"type": "boolean", "description": "true adds content to the end of the file instead of replacing it."}
                }),
                &["path", "content"],
            ),
        ),
        spec(
            "edit_file",
            "Change part of an existing file. Each edit replaces old_text with new_text. old_text must match the file exactly and appear once (or set replace_all). Read the file first.",
            schema(
                json!({
                    "path": {"type": "string", "description": "File to edit."},
                    "edits": {
                        "type": "array",
                        "description": "Edits applied in order.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_text": {"type": "string", "description": "Exact text to replace."},
                                "new_text": {"type": "string", "description": "Replacement text."},
                                "replace_all": {"type": "boolean", "description": "Replace every occurrence. Default false."}
                            },
                            "required": ["old_text", "new_text"]
                        }
                    }
                }),
                &["path", "edits"],
            ),
        ),
        spec(
            "move",
            "Move or rename a file or folder.",
            schema(
                json!({
                    "from": {"type": "string", "description": "Current path."},
                    "to": {"type": "string", "description": "New path."}
                }),
                &["from", "to"],
            ),
        ),
        spec(
            "delete",
            "Delete a file or folder. It is moved to a recoverable backup, not destroyed.",
            schema(
                json!({"path": {"type": "string", "description": "File or folder to delete."}}),
                &["path"],
            ),
        ),
        spec(
            "make_dir",
            "Create a folder, including missing parent folders.",
            schema(
                json!({"path": {"type": "string", "description": "Folder to create."}}),
                &["path"],
            ),
        ),
        spec(
            "run_command",
            "Run a shell command (sh) in the working folder. No network, no sudo. Only for things the other tools cannot do.",
            schema(
                json!({"command": {"type": "string", "description": "The shell command."}}),
                &["command"],
            ),
        ),
        spec(
            "finish",
            "Say the task is done and give the user a short summary of what actually happened.",
            schema(
                json!({"summary": {"type": "string", "description": "What was done, based only on tool results you saw."}}),
                &["summary"],
            ),
        ),
    ]
}

// --- JSON leniency ---

/// Fixes the mistakes small models make inside JSON: raw control characters
/// in strings, trailing commas, smart quotes used as string delimiters and
/// invalid backslash escapes. Only called after a strict parse failed.
fn repair_json(s: &str) -> String {
    #[derive(PartialEq)]
    enum St {
        Out,
        Plain,
        Smart,
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut st = St::Out;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match st {
            St::Out => match c {
                '"' => {
                    st = St::Plain;
                    out.push('"');
                }
                '\u{201c}' | '\u{201d}' => {
                    st = St::Smart;
                    out.push('"');
                }
                ',' => {
                    let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
                    if !matches!(next, Some('}') | Some(']')) {
                        out.push(',');
                    }
                }
                _ => out.push(c),
            },
            St::Plain | St::Smart => {
                let smart = st == St::Smart;
                match c {
                    '\\' => match chars.get(i + 1) {
                        Some(&n) if "\"\\/bfnrtu".contains(n) => {
                            out.push('\\');
                            out.push(n);
                            i += 1;
                        }
                        // Not a JSON escape (e.g. a Windows path): keep it literal.
                        _ => out.push_str("\\\\"),
                    },
                    '"' if smart => out.push_str("\\\""),
                    '"' => {
                        st = St::Out;
                        out.push('"');
                    }
                    '\u{201c}' | '\u{201d}' if smart => {
                        st = St::Out;
                        out.push('"');
                    }
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                    c => out.push(c),
                }
            }
        }
        i += 1;
    }
    out
}

fn parse_json_lenient(s: &str) -> Result<Value, String> {
    match serde_json::from_str::<Value>(s) {
        Ok(v) => Ok(v),
        Err(first) => serde_json::from_str::<Value>(&repair_json(s)).map_err(|_| first.to_string()),
    }
}

/// Index of the `}` closing the `{` at `start`, aware of JSON strings.
fn matching_brace(s: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for (off, b) in s.as_bytes()[start..].iter().enumerate() {
        if in_str {
            if escaped {
                escaped = false;
            } else if *b == b'\\' {
                escaped = true;
            } else if *b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + off);
                }
            }
            _ => {}
        }
    }
    None
}

// --- Name / argument normalization ---

fn canonical_tool_name(raw: &str) -> Option<&'static str> {
    let base = raw.trim().rsplit(['.', ':']).next().unwrap_or("");
    let n = base.to_ascii_lowercase().replace(['-', ' '], "_");
    Some(match n.as_str() {
        "list_dir" | "listdir" | "list_directory" | "list_files" | "ls" | "list" => "list_dir",
        "read_file" | "readfile" | "read" | "cat" | "open_file" => "read_file",
        "search" | "grep" | "search_files" | "search_text" => "search",
        "find_files" | "findfiles" | "find" | "glob" => "find_files",
        "write_file" | "writefile" | "write" | "create_file" | "append_file" | "append_to_file" => {
            "write_file"
        }
        "edit_file" | "editfile" | "edit" | "str_replace" => "edit_file",
        "move" | "mv" | "move_file" | "rename" => "move",
        "delete" | "rm" | "remove" | "delete_file" => "delete",
        "make_dir" | "makedir" | "mkdir" | "make_directory" | "create_dir" => "make_dir",
        "run_command" | "runcommand" | "run" | "bash" | "sh" | "shell" | "exec" | "command" => {
            "run_command"
        }
        "finish" | "done" | "final_answer" => "finish",
        _ => return None,
    })
}

fn rename_key(map: &mut Map<String, Value>, alias: &str, canon: &str) {
    if let Some(v) = map.remove(alias) {
        map.entry(canon.to_string()).or_insert(v);
    }
}

fn coerce_scalars(map: &mut Map<String, Value>) {
    for k in ["offset", "limit", "depth"] {
        let Some(v) = map.get_mut(k) else { continue };
        let n = match &*v {
            Value::String(s) => s.trim().parse::<u64>().ok(),
            Value::Number(n) if n.as_u64().is_none() => n
                .as_f64()
                .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                .map(|f| f as u64),
            _ => None,
        };
        if let Some(n) = n {
            *v = json!(n);
        }
    }
    for k in ["regex", "case_sensitive", "replace_all", "append"] {
        let Some(v) = map.get_mut(k) else { continue };
        if let Value::String(s) = &*v {
            match s.trim().to_ascii_lowercase().as_str() {
                "true" => *v = json!(true),
                "false" => *v = json!(false),
                _ => {}
            }
        }
    }
}

fn normalize_edit(v: Value) -> Value {
    let Value::Object(mut m) = v else { return v };
    rename_key(&mut m, "old", "old_text");
    rename_key(&mut m, "old_string", "old_text");
    rename_key(&mut m, "new", "new_text");
    rename_key(&mut m, "new_string", "new_text");
    coerce_scalars(&mut m);
    Value::Object(m)
}

/// `append_file` style names mean write_file with append on.
fn is_append_alias(raw: &str) -> bool {
    let base = raw.trim().rsplit(['.', ':']).next().unwrap_or("");
    matches!(
        base.to_ascii_lowercase().replace(['-', ' '], "_").as_str(),
        "append_file" | "append_to_file"
    )
}

/// Turns loosely shaped arguments into the exact object `Tool` deserializes.
fn tool_from_name_args(raw_name: &str, args: Value) -> Result<Tool, String> {
    let name = canonical_tool_name(raw_name).ok_or_else(|| {
        format!(
            "unknown tool `{}`. Valid tools: {}",
            raw_name.trim(),
            TOOL_NAMES.join(", ")
        )
    })?;
    let mut map = into_args_map(args, name)?;
    // `{"arguments": {...}}` wrapped one level deeper.
    for _ in 0..2 {
        if map.len() != 1 {
            break;
        }
        let key = map.keys().next().cloned().unwrap_or_default();
        if !matches!(key.as_str(), "arguments" | "args" | "parameters") {
            break;
        }
        let inner = map.remove(&key).unwrap_or(Value::Null);
        map = into_args_map(inner, name)?;
    }
    map.retain(|_, v| !v.is_null());
    for a in [
        "file",
        "filename",
        "filepath",
        "file_path",
        "directory",
        "dir",
        "folder",
    ] {
        rename_key(&mut map, a, "path");
    }
    rename_key(&mut map, "cmd", "command");
    match name {
        "write_file" => {
            for a in ["contents", "text", "data"] {
                rename_key(&mut map, a, "content");
            }
            if is_append_alias(raw_name) {
                map.insert("append".to_string(), Value::Bool(true));
            }
        }
        "edit_file" => {
            rename_key(&mut map, "old", "old_text");
            rename_key(&mut map, "old_string", "old_text");
            rename_key(&mut map, "new", "new_text");
            rename_key(&mut map, "new_string", "new_text");
            coerce_scalars(&mut map);
            if !map.contains_key("edits") && map.contains_key("old_text") {
                let mut e = Map::new();
                for k in ["old_text", "new_text", "replace_all"] {
                    if let Some(v) = map.remove(k) {
                        e.insert(k.to_string(), v);
                    }
                }
                map.insert("edits".to_string(), Value::Array(vec![Value::Object(e)]));
            }
            match map.remove("edits") {
                Some(Value::Array(a)) => {
                    let a = a.into_iter().map(normalize_edit).collect();
                    map.insert("edits".to_string(), Value::Array(a));
                }
                Some(o @ Value::Object(_)) => {
                    map.insert("edits".to_string(), Value::Array(vec![normalize_edit(o)]));
                }
                Some(other) => {
                    map.insert("edits".to_string(), other);
                }
                None => {}
            }
        }
        "move" => {
            rename_key(&mut map, "src", "from");
            rename_key(&mut map, "source", "from");
            rename_key(&mut map, "dest", "to");
            rename_key(&mut map, "destination", "to");
            rename_key(&mut map, "dst", "to");
        }
        "search" => rename_key(&mut map, "query", "pattern"),
        "find_files" => rename_key(&mut map, "pattern", "glob"),
        _ => {}
    }
    coerce_scalars(&mut map);
    map.insert("tool".to_string(), Value::String(name.to_string()));
    serde_json::from_value::<Tool>(Value::Object(map)).map_err(|e| format!("{name}: {e}"))
}

fn into_args_map(args: Value, name: &str) -> Result<Map<String, Value>, String> {
    match args {
        Value::Object(m) => Ok(m),
        Value::Null => Ok(Map::new()),
        Value::String(s) => match parse_json_lenient(&s) {
            Ok(Value::Object(m)) => Ok(m),
            _ => Err(format!("{name}: arguments must be a JSON object")),
        },
        _ => Err(format!("{name}: arguments must be a JSON object")),
    }
}

/// Arguments of a call object: an explicit arguments/args/parameters value,
/// else every other key.
fn take_args(obj: &Map<String, Value>, skip: &[&str]) -> Value {
    for k in ["arguments", "args", "parameters"] {
        if let Some(v) = obj.get(k) {
            return v.clone();
        }
    }
    let flat: Map<String, Value> = obj
        .iter()
        .filter(|(k, _)| !skip.contains(&k.as_str()) && !matches!(k.as_str(), "type" | "id"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Value::Object(flat)
}

fn has_args_key(obj: &Map<String, Value>) -> bool {
    ["arguments", "args", "parameters"]
        .iter()
        .any(|k| obj.contains_key(*k))
}

/// `{"tool": ..}`, `{"name": .., "arguments": ..}`, `{"function": ..}` and
/// the OpenAI `{"type":"function","function":{..}}` shape.
fn call_from_value(v: &Value) -> Result<Tool, String> {
    let obj = v.as_object().ok_or("the tool call must be a JSON object")?;
    if let Some(Value::String(t)) = obj.get("tool") {
        return tool_from_name_args(t, take_args(obj, &["tool"]));
    }
    match obj.get("function") {
        Some(Value::String(t)) => return tool_from_name_args(t, take_args(obj, &["function"])),
        Some(f @ Value::Object(_)) => return call_from_value(f),
        _ => {}
    }
    if let Some(Value::String(t)) = obj.get("name") {
        return tool_from_name_args(t, take_args(obj, &["name"]));
    }
    Err("missing \"tool\" key; expected {\"tool\": \"<name>\", ...}".to_string())
}

/// A JSON object that is plausibly a tool call (vs. data in an answer).
fn is_call_shape(v: &Value, accept_known_name: bool) -> bool {
    let Some(obj) = v.as_object() else {
        return false;
    };
    if obj.contains_key("tool") || obj.contains_key("function") {
        return true;
    }
    match obj.get("name") {
        Some(Value::String(n)) => {
            has_args_key(obj) || (accept_known_name && canonical_tool_name(n).is_some())
        }
        _ => false,
    }
}

/// Drops a wrapping ``` fence (Hermes output sometimes nests one inside
/// `<tool_call>`).
fn json_payload(s: &str) -> &str {
    let mut t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        t = rest.split_once('\n').map_or("", |(_, b)| b);
        t = t.trim_end().strip_suffix("```").unwrap_or(t).trim();
    }
    t
}

fn parse_call_text(inner: &str) -> Result<Tool, String> {
    let mut payload = json_payload(inner);
    // Tolerate <tool_call> tags in a fence body.
    if let Some(p) = payload.to_ascii_lowercase().find("<tool_call>") {
        payload = json_payload(&payload[p + "<tool_call>".len()..]);
    }
    let payload = payload
        .rsplit_once("</tool_call>")
        .map_or(payload, |(a, _)| a);
    let v = match parse_json_lenient(payload) {
        Ok(v) => v,
        Err(e) => match crate::llm::extract_json_object(payload) {
            Some(v) => v,
            None => return Err(format!("invalid JSON ({e})")),
        },
    };
    call_from_value(&v)
}

pub fn from_native(call: &ToolCall) -> Result<Tool, String> {
    let name = call.name.trim();
    let raw = call.arguments.trim();
    let args = if raw.is_empty() {
        Value::Object(Map::new())
    } else {
        parse_json_lenient(raw)
            .map_err(|e| format!("{name}: the arguments are not valid JSON ({e})"))?
    };
    tool_from_name_args(name, args)
}

// --- Text-mode block scanner ---

struct Line<'a> {
    start: usize,
    next: usize,
    text: &'a str,
}

fn split_lines(s: &str) -> Vec<Line<'_>> {
    let mut v = Vec::new();
    let mut pos = 0;
    while pos < s.len() {
        let (end, next) = match s[pos..].find('\n') {
            Some(i) => (pos + i, pos + i + 1),
            None => (s.len(), s.len()),
        };
        v.push(Line {
            start: pos,
            next,
            text: s[pos..end].trim_end_matches('\r'),
        });
        pos = next;
    }
    v
}

/// `(backtick count, info string)` of an opening fence line.
fn fence_open(text: &str) -> Option<(usize, &str)> {
    let t = text.trim_start();
    let n = t.chars().take_while(|c| *c == '`').count();
    if n < 3 {
        return None;
    }
    let info = t[n..].trim();
    (!info.contains('`')).then_some((n, info))
}

/// A line of only backticks, at least `n` of them.
fn is_closing(text: &str, n: usize) -> bool {
    let t = text.trim();
    t.len() >= n && t.bytes().all(|b| b == b'`')
}

fn first_close(lines: &[Line], from: usize, n: usize) -> Option<usize> {
    (from..lines.len()).find(|&i| is_closing(lines[i].text, n))
}

/// `write` and `edit` content may hold its own fences, so it ends at the
/// last bare fence of the whole reply.
fn last_close(lines: &[Line], from: usize, n: usize) -> Option<usize> {
    (from..lines.len())
        .rev()
        .find(|&i| is_closing(lines[i].text, n))
}

fn line_at_or_after(lines: &[Line], pos: usize) -> usize {
    lines
        .iter()
        .position(|l| l.start >= pos)
        .unwrap_or(lines.len())
}

struct Block {
    start: usize,
    end: usize,
    call: Result<Tool, String>,
}

#[derive(PartialEq)]
enum Kind {
    ToolJson,
    MaybeJson,
    Shell,
    Console,
    Write,
    Append,
    Edit,
    Other,
}

fn fence_kind(tag: &str) -> Kind {
    match tag {
        "tool" | "tool_call" | "toolcall" | "tool-call" => Kind::ToolJson,
        "json" => Kind::MaybeJson,
        "sh" | "bash" | "shell" | "zsh" => Kind::Shell,
        "console" => Kind::Console,
        "write" => Kind::Write,
        "append" => Kind::Append,
        "edit" => Kind::Edit,
        _ => Kind::Other,
    }
}

fn clean_path(s: &str) -> &str {
    s.trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`'))
        .trim()
}

fn shell_command(lines: &[&str], console: bool) -> String {
    let prompt = |l: &&str| l.trim_start().strip_prefix("$ ").map(str::to_string);
    let picked: Vec<String> = if console && lines.iter().any(|l| prompt(l).is_some()) {
        // A console transcript mixes prompts and output; keep only commands.
        lines.iter().filter_map(prompt).collect()
    } else {
        lines
            .iter()
            .map(|l| prompt(l).unwrap_or_else(|| l.to_string()))
            .collect()
    };
    picked.join("\n").trim().to_string()
}

fn is_marker(t: &str, ch: char, word: &str) -> bool {
    let n = t.chars().take_while(|c| *c == ch).count();
    (5..=9).contains(&n) && t[n * ch.len_utf8()..].trim().eq_ignore_ascii_case(word)
}

fn is_separator(t: &str) -> bool {
    (5..=9).contains(&t.len()) && t.bytes().all(|b| b == b'=')
}

/// Small models sometimes put the path on the first line inside a
/// ```write/```edit block instead of on the fence line. Accepted when that
/// line is a single path-like token; `write` is stricter (needs a `.` or `/`)
/// because its first content line could be anything.
fn path_on_first_line<'a>(inner: &'a [&'a str], strict: bool) -> Option<(&'a str, &'a [&'a str])> {
    let first = clean_path(inner.first()?);
    let plausible = !first.is_empty()
        && first.len() <= 200
        && !first.contains(char::is_whitespace)
        && !first.starts_with(['<', '=', '>'])
        && (!strict || first.contains(['.', '/']));
    plausible.then(|| (first, &inner[1..]))
}

fn parse_edit_block(path: &str, lines: &[&str]) -> Result<Tool, String> {
    if path.is_empty() {
        return Err("the ```edit line needs a file path, e.g. ```edit notes.txt".to_string());
    }
    enum S {
        Out,
        Old,
        New,
    }
    let mut state = S::Out;
    let (mut old, mut new): (Vec<&str>, Vec<&str>) = (Vec::new(), Vec::new());
    let mut edits = Vec::new();
    for l in lines {
        let t = l.trim();
        match state {
            S::Out => {
                if is_marker(t, '<', "search") {
                    state = S::Old;
                    old.clear();
                    new.clear();
                }
            }
            S::Old => {
                if is_separator(t) {
                    state = S::New;
                } else {
                    old.push(l);
                }
            }
            S::New => {
                if is_marker(t, '>', "replace") {
                    let old_text = old.join("\n");
                    if old_text.is_empty() {
                        return Err(
                            "a SEARCH section is empty; to create a file use ```write".to_string()
                        );
                    }
                    edits.push(Edit {
                        old_text,
                        new_text: new.join("\n"),
                        replace_all: false,
                    });
                    state = S::Out;
                } else {
                    new.push(l);
                }
            }
        }
    }
    match state {
        S::Out => {}
        S::Old => return Err("an edit block has no ======= line after the old text".to_string()),
        S::New => {
            return Err("an edit block has no >>>>>>> REPLACE line after the new text".to_string())
        }
    }
    if edits.is_empty() {
        return Err(
            "the ```edit block has no <<<<<<< SEARCH / ======= / >>>>>>> REPLACE section"
                .to_string(),
        );
    }
    Ok(Tool::EditFile {
        path: path.to_string(),
        edits,
    })
}

fn scan_blocks(body: &str) -> Vec<Block> {
    let lines = split_lines(body);
    let lower = body.to_ascii_lowercase();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        if let Some((n, info)) = fence_open(line.text) {
            let first = info.split_whitespace().next().unwrap_or("");
            let tag = first.to_ascii_lowercase();
            let rest = info[first.len()..].trim();
            let kind = fence_kind(&tag);
            let close = if matches!(kind, Kind::Write | Kind::Append | Kind::Edit) {
                last_close(&lines, i + 1, n)
            } else {
                first_close(&lines, i + 1, n)
            };
            let (inner_end, end) = match close {
                Some(c) => (c, lines[c].next),
                None => (lines.len(), body.len()),
            };
            let closed = close.is_some();
            let inner: Vec<&str> = lines[i + 1..inner_end].iter().map(|l| l.text).collect();
            let unclosed = || {
                format!(
                    "the ```{tag} block was never closed with ``` (the reply may have been cut off)"
                )
            };
            let call = match kind {
                Kind::ToolJson => Some(parse_call_text(&inner.join("\n")).map_err(|e| {
                    if closed {
                        e
                    } else {
                        format!("{e}; {}", unclosed())
                    }
                })),
                Kind::MaybeJson => {
                    let text = inner.join("\n");
                    let attempt = match parse_json_lenient(json_payload(&text)) {
                        Ok(v) => is_call_shape(&v, true),
                        Err(_) => {
                            text.contains("\"tool\"")
                                || (text.contains("\"name\"")
                                    && (text.contains("\"arguments\"")
                                        || text.contains("\"args\"")))
                        }
                    };
                    attempt.then(|| parse_call_text(&text))
                }
                Kind::Shell | Kind::Console => Some(if !closed {
                    Err(unclosed())
                } else {
                    let command = shell_command(&inner, kind == Kind::Console);
                    if command.is_empty() {
                        Err("the shell block is empty".to_string())
                    } else {
                        Ok(Tool::RunCommand { command })
                    }
                }),
                Kind::Write | Kind::Append => Some(if !closed {
                    Err(unclosed())
                } else {
                    let mut path = clean_path(rest);
                    let mut body: &[&str] = &inner;
                    if path.is_empty() {
                        if let Some((p, b)) = path_on_first_line(&inner, true) {
                            (path, body) = (p, b);
                        }
                    }
                    if path.is_empty() {
                        Err(format!(
                            "the ```{tag} line needs a file path, e.g. ```{tag} notes.txt"
                        ))
                    } else {
                        let mut content = body.join("\n");
                        if !body.is_empty() {
                            content.push('\n');
                        }
                        Ok(Tool::WriteFile {
                            path: path.to_string(),
                            content,
                            append: kind == Kind::Append,
                        })
                    }
                }),
                Kind::Edit => Some(if !closed {
                    Err(unclosed())
                } else {
                    match path_on_first_line(&inner, false) {
                        Some((p, b)) if rest.is_empty() => parse_edit_block(p, b),
                        _ => parse_edit_block(clean_path(rest), &inner),
                    }
                }),
                Kind::Other => {
                    // A <tool_call> wrapped in an ordinary fence (e.g. ```xml).
                    let text = inner.join("\n");
                    text.trim_start()
                        .to_ascii_lowercase()
                        .starts_with("<tool_call>")
                        .then(|| parse_call_text(&text))
                }
            };
            if let Some(call) = call {
                blocks.push(Block {
                    start: line.start,
                    end,
                    call,
                });
            }
            i = line_at_or_after(&lines, end).max(i + 1);
            continue;
        }

        if let Some(col) = lower[line.start..line.start + line.text.len()].find("<tool_call>") {
            let start = line.start + col;
            let after = start + "<tool_call>".len();
            let (inner, end) = match lower[after..].find("</tool_call>") {
                Some(j) => (&body[after..after + j], after + j + "</tool_call>".len()),
                None => (&body[after..], body.len()),
            };
            blocks.push(Block {
                start,
                end,
                call: parse_call_text(inner),
            });
            i = line_at_or_after(&lines, end).max(i + 1);
            continue;
        }

        let t = line.text.trim_start();
        if t.starts_with('{') {
            let start = line.start + (line.text.len() - t.len());
            let found = match matching_brace(body, start) {
                Some(e) => {
                    let text = &body[start..=e];
                    match parse_json_lenient(text) {
                        Ok(v) if is_call_shape(&v, false) => Some((e + 1, call_from_value(&v))),
                        Ok(_) => None,
                        Err(err) => text
                            .contains("\"tool\"")
                            .then(|| (e + 1, Err(format!("invalid JSON ({err})")))),
                    }
                }
                None => body[start..].contains("\"tool\"").then(|| {
                    (
                        body.len(),
                        Err(
                            "the JSON object is not closed (the reply may have been cut off)"
                                .to_string(),
                        ),
                    )
                }),
            };
            if let Some((end, call)) = found {
                blocks.push(Block { start, end, call });
                i = line_at_or_after(&lines, end).max(i + 1);
                continue;
            }
        }
        i += 1;
    }
    blocks
}

/// The reply with every block cut out, pieces joined by newlines.
fn outside_text(body: &str, blocks: &[Block]) -> String {
    let mut pieces: Vec<&str> = Vec::new();
    let mut pos = 0;
    for b in blocks {
        pieces.push(body[pos..b.start].trim());
        pos = b.end;
    }
    pieces.push(body[pos..].trim());
    pieces
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn parse_text_reply(raw: &str) -> TextParse {
    let (_, body) = split_thinking(raw);
    let mut blocks = scan_blocks(&body);
    if blocks.is_empty() {
        return TextParse::Answer(body.trim().to_string());
    }
    let prose = outside_text(&body, &blocks);
    let extra_blocks = blocks.len() - 1;
    match blocks.swap_remove(0).call {
        Ok(tool) => TextParse::Tool {
            tool,
            prose,
            extra_blocks,
        },
        Err(error) => TextParse::Malformed { error, prose },
    }
}

/// Reply text for display: thinking and every tool block removed.
pub fn strip_tool_blocks(raw: &str) -> String {
    let (_, body) = split_thinking(raw);
    let blocks = scan_blocks(&body);
    outside_text(&body, &blocks)
}

// --- Results ---

const RESULT_CAP: usize = 16_000;
const RESULT_HEAD: usize = 10_000;
const RESULT_TAIL: usize = 5_000;

pub fn format_tool_result(tool: &Tool, ok: bool, body: &str) -> String {
    let total = body.chars().count();
    let body = if total <= RESULT_CAP {
        body.to_string()
    } else {
        let head: String = body.chars().take(RESULT_HEAD).collect();
        let tail: String = body.chars().skip(total - RESULT_TAIL).collect();
        format!(
            "{head}\n[... {} characters omitted ...]\n{tail}",
            total - RESULT_HEAD - RESULT_TAIL
        )
    };
    format!(
        "[tool result: {} — {}]\n{body}",
        tool.name(),
        if ok { "ok" } else { "error" }
    )
}

// --- System prompts ---

const COMMON_RULES: &str = "\
You are a careful file assistant. You work only inside the folder the user opened; every path is \
relative to it.

# How to work
- One tool call per reply. Wait for its result before the next step.
- To add to a file, append; to change part of it, edit; write replaces the whole file.
- Read a file before you edit it. Prefer the dedicated tools over shell commands.
- run_command is for what the tools cannot do. It runs `sh` with no network and no sudo.
- Changes are backed up automatically, so deleting or overwriting is recoverable.
- When the task is done, reply in plain text with no tool call, or call finish.
- Never claim something happened until you have seen its tool result. A call you are about to make \
has NOT happened yet, so describe it in the future tense, never as done. A call the user denied \
did NOT run.
- Never repeat a call just to show a result you already have; quote it.
- Keep replies short.";

const NATIVE_FORMAT: &str = "\
# Tool format
Call tools through the tool-call interface, one per reply. Don't write the call as text.";

const TEXT_FORMAT: &str = r#"# Tool format
Write one short sentence, then exactly ONE block. Only the first block in a reply runs.

Call a tool with JSON:
```tool
{"tool": "read_file", "path": "notes.txt"}
```

Write a whole file (raw content, no JSON, no escaping):
```write notes.txt
first line
second line
```

Add lines to the end of a file:
```append notes.txt
last line
```

Change part of a file (old text copied exactly from read_file):
```edit notes.txt
<<<<<<< SEARCH
old line
=======
new line
>>>>>>> REPLACE
```

Run a shell command:
```sh
ls -F
```

Tools (JSON keys):
- list_dir: path, depth
- read_file: path, offset, limit
- search: pattern, path, glob, regex, case_sensitive
- find_files: glob, path
- move: from, to
- delete: path
- make_dir: path
- finish: summary
Writing, appending and editing files uses the ```write, ```append and ```edit blocks above; shell commands use ```sh."#;

/// Static content only: nothing here varies per step, so the server's
/// prefix cache stays warm across a whole task.
pub fn system_prompt(
    mode: Transport,
    root_note: &str,
    rules: &str,
    user_system_prompt: &str,
) -> String {
    let mut out = String::from(COMMON_RULES);
    let root_note = root_note.trim();
    if !root_note.is_empty() {
        out.push_str("\n\n");
        out.push_str(root_note);
    }
    out.push_str("\n\n");
    out.push_str(match mode {
        Transport::Native => NATIVE_FORMAT,
        Transport::Text => TEXT_FORMAT,
    });
    for extra in [rules, user_system_prompt] {
        let extra = extra.trim();
        if !extra.is_empty() {
            out.push_str("\n\n");
            out.push_str(extra);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(path: &str) -> Tool {
        Tool::ReadFile {
            path: path.to_string(),
            offset: None,
            limit: None,
        }
    }

    fn cmd(c: &str) -> Tool {
        Tool::RunCommand {
            command: c.to_string(),
        }
    }

    fn write(path: &str, content: &str) -> Tool {
        Tool::WriteFile {
            path: path.to_string(),
            content: content.to_string(),
            append: false,
        }
    }

    fn edit(old: &str, new: &str) -> Edit {
        Edit {
            old_text: old.to_string(),
            new_text: new.to_string(),
            replace_all: false,
        }
    }

    enum Want {
        T(Tool),
        Answer,
        Bad(&'static str),
    }
    use Want::*;

    #[test]
    fn text_reply_table() {
        let cases: Vec<(&str, String, Want)> = vec![
            (
                "tool fence",
                "Let me look.\n```tool\n{\"tool\": \"read_file\", \"path\": \"a.txt\"}\n```".into(),
                T(read("a.txt")),
            ),
            (
                "json fence",
                "```json\n{\"tool\":\"read_file\",\"path\":\"a.txt\"}\n```".into(),
                T(read("a.txt")),
            ),
            (
                "crlf",
                "```tool\r\n{\"tool\": \"read_file\", \"path\": \"a.txt\"}\r\n```\r\n".into(),
                T(read("a.txt")),
            ),
            (
                "upper-case tag and trailing spaces",
                "```TOOL  \n{\"tool\": \"read_file\", \"path\": \"a.txt\"}\n```  ".into(),
                T(read("a.txt")),
            ),
            (
                "trailing comma",
                "```tool\n{\"tool\": \"read_file\", \"path\": \"a.txt\",}\n```".into(),
                T(read("a.txt")),
            ),
            (
                "raw newline inside string",
                "```tool\n{\"tool\": \"write_file\", \"path\": \"a.txt\", \"content\": \"x\ny\"}\n```"
                    .into(),
                T(write("a.txt", "x\ny")),
            ),
            (
                "smart quotes",
                "```tool\n{\u{201c}tool\u{201d}: \u{201c}read_file\u{201d}, \u{201c}path\u{201d}: \u{201c}a.txt\u{201d}}\n```"
                    .into(),
                T(read("a.txt")),
            ),
            (
                "name/arguments shape",
                "```json\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}\n```".into(),
                T(read("a.txt")),
            ),
            (
                "tool/args shape",
                "```tool\n{\"tool\": \"read_file\", \"args\": {\"path\": \"a.txt\"}}\n```".into(),
                T(read("a.txt")),
            ),
            (
                "openai function shape",
                "```json\n{\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}\n```"
                    .into(),
                T(read("a.txt")),
            ),
            (
                "function string key",
                "```tool\n{\"function\": \"read_file\", \"path\": \"a.txt\"}\n```".into(),
                T(read("a.txt")),
            ),
            (
                "hermes inline",
                "<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}\n</tool_call>"
                    .into(),
                T(read("a.txt")),
            ),
            (
                "hermes in xml fence",
                "```xml\n<tool_call>{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}</tool_call>\n```"
                    .into(),
                T(read("a.txt")),
            ),
            (
                "hermes with json fence inside",
                "<tool_call>\n```json\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}\n```\n</tool_call>"
                    .into(),
                T(read("a.txt")),
            ),
            (
                "hermes unclosed but valid",
                "<tool_call>{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}".into(),
                T(read("a.txt")),
            ),
            ("sh fence", "```sh\nls -F\n```".into(), T(cmd("ls -F"))),
            (
                "bash with prompt prefix",
                "```bash\n$ ls -F\n$ pwd\n```".into(),
                T(cmd("ls -F\npwd")),
            ),
            (
                "console keeps only prompt lines",
                "```console\n$ ls\na.txt b.txt\n```".into(),
                T(cmd("ls")),
            ),
            (
                "write block",
                "```write notes.txt\nhello\nworld\n```".into(),
                T(write("notes.txt", "hello\nworld\n")),
            ),
            (
                "write with nested fences",
                "```write README.md\n# T\n```sh\nls\n```\ndone\n```\nAll set.".into(),
                T(write("README.md", "# T\n```sh\nls\n```\ndone\n")),
            ),
            (
                "write crlf",
                "```write a.txt\r\nx\r\ny\r\n```\r\n".into(),
                T(write("a.txt", "x\ny\n")),
            ),
            (
                "write in a four-backtick fence",
                "````write a.md\n```\ncode\n```\n````".into(),
                T(write("a.md", "```\ncode\n```\n")),
            ),
            (
                "append block",
                "```append notes.txt\nlast\n```".into(),
                T(Tool::WriteFile {
                    path: "notes.txt".into(),
                    content: "last\n".into(),
                    append: true,
                }),
            ),
            (
                "append path on next line",
                "```append\nnotes.txt\nlast\n```".into(),
                T(Tool::WriteFile {
                    path: "notes.txt".into(),
                    content: "last\n".into(),
                    append: true,
                }),
            ),
            (
                "edit block",
                "```edit a.txt\n<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE\n```".into(),
                T(Tool::EditFile {
                    path: "a.txt".into(),
                    edits: vec![edit("old", "new")],
                }),
            ),
            (
                "edit two sections",
                "```edit a.txt\n<<<<<<< SEARCH\na\nb\n=======\nA\n>>>>>>> REPLACE\n\n<<<<<<< SEARCH\nc\n=======\nC\n>>>>>>> REPLACE\n```"
                    .into(),
                T(Tool::EditFile {
                    path: "a.txt".into(),
                    edits: vec![edit("a\nb", "A"), edit("c", "C")],
                }),
            ),
            (
                "edit with odd marker widths and trailing spaces",
                "```EDIT a.txt\n<<<<<<<<< search  \nold\n=========  \nnew\n>>>>>> replace \n```".into(),
                T(Tool::EditFile {
                    path: "a.txt".into(),
                    edits: vec![edit("old", "new")],
                }),
            ),
            (
                "edit with empty replacement",
                "```edit a.txt\n<<<<<<< SEARCH\nold\n=======\n>>>>>>> REPLACE\n```".into(),
                T(Tool::EditFile {
                    path: "a.txt".into(),
                    edits: vec![edit("old", "")],
                }),
            ),
            (
                "bare json",
                "{\"tool\": \"read_file\", \"path\": \"a.txt\"}".into(),
                T(read("a.txt")),
            ),
            (
                "bare json after prose",
                "Reading it now.\n{\"tool\": \"read_file\", \"path\": \"a.txt\"}\nThanks.".into(),
                T(read("a.txt")),
            ),
            (
                "flat edit_file becomes edits",
                "```tool\n{\"tool\":\"edit_file\",\"path\":\"a.txt\",\"old_text\":\"x\",\"new_text\":\"y\"}\n```"
                    .into(),
                T(Tool::EditFile {
                    path: "a.txt".into(),
                    edits: vec![edit("x", "y")],
                }),
            ),
            (
                "aliases",
                "```tool\n{\"tool\":\"run_command\",\"cmd\":\"ls\"}\n```".into(),
                T(cmd("ls")),
            ),
            (
                "file alias and numeric strings",
                "```tool\n{\"tool\":\"read_file\",\"file\":\"a.txt\",\"offset\":\"5\",\"limit\":10.0}\n```"
                    .into(),
                T(Tool::ReadFile {
                    path: "a.txt".into(),
                    offset: Some(5),
                    limit: Some(10),
                }),
            ),
            (
                "thinking-wrapped call",
                "<think>maybe ```sh\nrm -rf x\n``` no</think>\n```tool\n{\"tool\":\"read_file\",\"path\":\"a.txt\"}\n```"
                    .into(),
                T(read("a.txt")),
            ),
            ("plain answer", "All done, nothing to change.".into(), Answer),
            (
                "answer with python fence",
                "Here is the code:\n```python\nprint('hi')\n```\nThat is it.".into(),
                Answer,
            ),
            (
                "answer with json data fence",
                "The config is:\n```json\n{\"name\": \"Bob\", \"age\": 3}\n```".into(),
                Answer,
            ),
            (
                "answer with tool call only inside thinking",
                "<think>```sh\nls\n```</think>Nothing more to do.".into(),
                Answer,
            ),
            (
                "answer with ordinary fence hiding a tool-looking line",
                "```text\n{\"x\": 1}\n```".into(),
                Answer,
            ),
            (
                "broken json",
                "```tool\n{\"tool\": \"read_file\", path: }\n```".into(),
                Bad("invalid JSON"),
            ),
            (
                "unknown tool",
                "```tool\n{\"tool\": \"fly_away\"}\n```".into(),
                Bad("unknown tool `fly_away`"),
            ),
            (
                "missing required field",
                "```tool\n{\"tool\": \"read_file\"}\n```".into(),
                Bad("read_file"),
            ),
            (
                "json fence with name but no tool shape and broken",
                "```json\n{\"name\": \"read_file\", \"arguments\": {\"path\": }}\n```".into(),
                Bad("invalid JSON"),
            ),
            (
                "write unclosed",
                "```write a.txt\nhalf a fi".into(),
                Bad("never closed"),
            ),
            (
                "write without path",
                "```write\ncontent\n```".into(),
                Bad("file path"),
            ),
            ("sh unclosed", "```sh\nrm -r".into(), Bad("never closed")),
            ("sh empty", "```sh\n```".into(), Bad("empty")),
            (
                "edit missing separator",
                "```edit a.txt\n<<<<<<< SEARCH\nold\n```".into(),
                Bad("======="),
            ),
            (
                "edit empty search",
                "```edit a.txt\n<<<<<<< SEARCH\n=======\nnew\n>>>>>>> REPLACE\n```".into(),
                Bad("SEARCH section is empty"),
            ),
            (
                "edit with no sections",
                "```edit a.txt\njust words\n```".into(),
                Bad("no <<<<<<< SEARCH"),
            ),
            (
                "hermes with bad json",
                "<tool_call>not json</tool_call>".into(),
                Bad("invalid JSON"),
            ),
            (
                "truncated bare json",
                "{\"tool\": \"write_file\", \"path\": \"a.txt\", \"content\": \"abc".into(),
                Bad("not closed"),
            ),
        ];
        assert!(cases.len() >= 30);
        for (name, input, want) in cases {
            let got = parse_text_reply(&input);
            match (want, got) {
                (T(t), TextParse::Tool { tool, .. }) => assert_eq!(tool, t, "case: {name}"),
                (Answer, TextParse::Answer(_)) => {}
                (Bad(frag), TextParse::Malformed { error, .. }) => {
                    assert!(error.contains(frag), "case: {name}: error was `{error}`")
                }
                (_, got) => panic!("case: {name}: unexpected {got:?}"),
            }
        }
    }

    #[test]
    fn prose_and_extra_blocks() {
        let raw = "First I list.\n```sh\nls\n```\nThen more.\n```sh\npwd\n```";
        match parse_text_reply(raw) {
            TextParse::Tool {
                tool,
                prose,
                extra_blocks,
            } => {
                assert_eq!(tool, cmd("ls"));
                assert_eq!(extra_blocks, 1);
                assert_eq!(prose, "First I list.\nThen more.");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn earliest_block_wins_even_when_malformed() {
        let raw = "```tool\n{\"tool\": \"nope\"}\n```\n```sh\nls\n```";
        assert!(matches!(parse_text_reply(raw), TextParse::Malformed { .. }));
    }

    // Seen live from qwen2.5:3b: the path on the line after the fence.
    #[test]
    fn edit_path_on_the_first_line_inside_the_block() {
        let raw =
            "```edit\nshopping.txt\n<<<<<<< SEARCH\neggs\n=======\nmilk\n>>>>>>> REPLACE\n```";
        match parse_text_reply(raw) {
            TextParse::Tool { tool, .. } => assert_eq!(
                tool,
                Tool::EditFile {
                    path: "shopping.txt".into(),
                    edits: vec![edit("eggs", "milk")],
                }
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn write_path_on_the_first_line_only_when_it_looks_like_a_path() {
        match parse_text_reply("```write\nnotes/a.txt\nhello\n```") {
            TextParse::Tool { tool, .. } => assert_eq!(tool, write("notes/a.txt", "hello\n")),
            other => panic!("{other:?}"),
        }
        // No `.` or `/`: that is content, so the path is still missing.
        assert!(matches!(
            parse_text_reply("```write\nhello\n```"),
            TextParse::Malformed { .. }
        ));
        // A path on the fence line is never second-guessed.
        match parse_text_reply("```write a.txt\nb.txt\n```") {
            TextParse::Tool { tool, .. } => assert_eq!(tool, write("a.txt", "b.txt\n")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn answer_keeps_ordinary_fences() {
        match parse_text_reply("Use:\n```python\nprint(1)\n```") {
            TextParse::Answer(a) => assert!(a.contains("```python")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn strip_blocks_for_display() {
        let raw = "<think>hmm</think>Checking.\n```tool\n{\"tool\":\"list_dir\"}\n```\nDone.\n```python\nx=1\n```";
        let shown = strip_tool_blocks(raw);
        assert_eq!(shown, "Checking.\nDone.\n```python\nx=1\n```");
        assert_eq!(strip_tool_blocks("just text"), "just text");
    }

    #[test]
    fn list_dir_defaults() {
        match parse_text_reply("```tool\n{\"tool\":\"list_dir\"}\n```") {
            TextParse::Tool {
                tool: Tool::ListDir { path, depth },
                ..
            } => {
                assert_eq!(path, ".");
                assert_eq!(depth, None);
            }
            other => panic!("{other:?}"),
        }
    }

    fn native(name: &str, args: &str) -> Result<Tool, String> {
        from_native(&ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: args.into(),
        })
    }

    #[test]
    fn native_append_forms() {
        let want = |c: &str| Tool::WriteFile {
            path: "a.txt".into(),
            content: c.into(),
            append: true,
        };
        for args in [
            r#"{"path":"a.txt","content":"x","append":true}"#,
            r#"{"path":"a.txt","content":"x","append":"true"}"#,
        ] {
            assert_eq!(native("write_file", args).unwrap(), want("x"));
        }
        assert_eq!(
            native(
                "write_file",
                r#"{"path":"a.txt","content":"x","append":"false"}"#
            )
            .unwrap(),
            write("a.txt", "x")
        );
        for name in ["append_file", "append_to_file"] {
            assert_eq!(
                native(name, r#"{"path":"a.txt","content":"x"}"#).unwrap(),
                want("x")
            );
        }
    }

    #[test]
    fn native_parsing() {
        assert_eq!(
            native("read_file", r#"{"path":"a.txt"}"#).unwrap(),
            read("a.txt")
        );
        assert_eq!(
            native("read_file", r#"{"arguments":{"path":"a.txt"}}"#).unwrap(),
            read("a.txt")
        );
        // Double-encoded arguments.
        assert_eq!(
            native("read_file", r#""{\"path\":\"a.txt\"}""#).unwrap(),
            read("a.txt")
        );
        assert_eq!(native("run_command", r#"{"cmd":"ls"}"#).unwrap(), cmd("ls"));
        assert_eq!(
            native("edit_file", r#"{"filename":"a","old":"x","new":"y"}"#).unwrap(),
            Tool::EditFile {
                path: "a".into(),
                edits: vec![edit("x", "y")]
            }
        );
        assert_eq!(
            native(
                "edit_file",
                r#"{"path":"a","edits":[{"old":"x","new_text":"y","replace_all":"true"}]}"#
            )
            .unwrap(),
            Tool::EditFile {
                path: "a".into(),
                edits: vec![Edit {
                    old_text: "x".into(),
                    new_text: "y".into(),
                    replace_all: true
                }]
            }
        );
        assert!(matches!(
            native("list_dir", "").unwrap(),
            Tool::ListDir { .. }
        ));
        assert!(matches!(
            native("search", r#"{"pattern":"x","regex":"true"}"#).unwrap(),
            Tool::Search { regex: true, .. }
        ));
        assert!(native("nope", "{}").unwrap_err().contains("unknown tool"));
        assert!(native("read_file", "{")
            .unwrap_err()
            .contains("not valid JSON"));
        assert!(native("read_file", "{}").unwrap_err().contains("path"));
    }

    #[test]
    fn specs_cover_every_tool_and_deserialize() {
        let specs = tool_specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, TOOL_NAMES);
        for s in &specs {
            let props = s.parameters["properties"].as_object().unwrap();
            let mut args = Map::new();
            for r in s.parameters["required"].as_array().unwrap() {
                let k = r.as_str().unwrap();
                let v = match props[k]["type"].as_str().unwrap() {
                    "string" => json!("x"),
                    "array" => json!([{"old_text": "a", "new_text": "b"}]),
                    "integer" => json!(1),
                    _ => json!(true),
                };
                args.insert(k.to_string(), v);
            }
            let call = ToolCall {
                id: "1".into(),
                name: s.name.clone(),
                arguments: Value::Object(args).to_string(),
            };
            let tool = from_native(&call).unwrap_or_else(|e| panic!("{}: {e}", s.name));
            assert_eq!(tool.name(), s.name);
        }
    }

    #[test]
    fn result_formatting_and_cap() {
        let t = read("a.txt");
        assert_eq!(
            format_tool_result(&t, true, "hi"),
            "[tool result: read_file — ok]\nhi"
        );
        assert!(
            format_tool_result(&t, false, "boom").starts_with("[tool result: read_file — error]")
        );
        let big = "é".repeat(20_000);
        let out = format_tool_result(&t, true, &big);
        assert!(out.contains("5000 characters omitted"));
        assert!(out.chars().count() < 16_200);
    }

    #[test]
    fn system_prompts() {
        let text = system_prompt(Transport::Text, "Root: /x", "# Rules\n- a", "Be nice.");
        assert!(text.contains("```write notes.txt"));
        assert!(text.contains("<<<<<<< SEARCH"));
        assert!(text.contains("Root: /x"));
        assert!(text.ends_with("Be nice."));
        let native = system_prompt(Transport::Native, "", "", "");
        assert!(!native.contains("```write"));
        assert!(native.contains("tool-call interface"));
        // Static: same inputs, same bytes.
        assert_eq!(
            text,
            system_prompt(Transport::Text, "Root: /x", "# Rules\n- a", "Be nice.")
        );
    }

    #[test]
    fn malformed_note_wraps_error() {
        let n = malformed_note("bad");
        assert!(n.starts_with(MALFORMED_NOTE_PREFIX));
        assert!(n.contains("bad"));
        assert!(n.ends_with("format.]"));
    }

    #[test]
    fn repair_handles_windows_paths_and_control_chars() {
        let v = parse_json_lenient("{\"path\": \"C:\\dir\\pics\", \"c\": \"a\tb\"}").unwrap();
        assert_eq!(v["path"], "C:\\dir\\pics");
        assert_eq!(v["c"], "a\tb");
    }
}
