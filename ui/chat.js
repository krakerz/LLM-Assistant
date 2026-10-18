// Chat mode: sessions, personas, rulesets, attachments, send/stream/follow-up handlers.

// Quick per-turn toggle for `chat_hide_narration`, mirroring the same
// checkbox in Settings -- lets you flip it from the chat header itself
// without opening a dialog, and re-draws whatever's already on screen
// (`renderChatHistoryLog`) so it takes effect immediately.
const toggleNarrationBtn = document.getElementById("toggleNarrationBtn");

function updateNarrationToggleBtn() {
  const hidden = !!currentConfig?.chat_hide_narration;
  toggleNarrationBtn.classList.toggle("active", hidden);
  toggleNarrationBtn.title = hidden
    ? "Narration is hidden — click to show it"
    : "Narration is shown — click to hide it";
}

toggleNarrationBtn.addEventListener("click", async () => {
  if (!currentConfig) return;
  currentConfig.chat_hide_narration = !currentConfig.chat_hide_narration;
  updateNarrationToggleBtn();
  renderChatHistoryLog();
  try {
    await invoke("save_config", { cfg: currentConfig });
  } catch (err) {
    console.error("save_config (narration toggle) failed:", err);
  }
});


// ============================================================
// Chat mode: a second, persona-driven UI. Purely conversational -- no
// sandbox, no folder, no auto-continue chain. See
// plans/2026-09-01-chat-mode.md.
// ============================================================

const fileOpsView = document.getElementById("fileOpsView");
const chatModeView = document.getElementById("chatModeView");
const fileOpsModeBtn = document.getElementById("fileOpsModeBtn");
const chatModeBtn = document.getElementById("chatModeBtn");

// File-ops mode needs a real sandboxed folder (`bubblewrap`, picked through
// a native dialog) -- neither exists for a browser talking to
// `llm-assistant --server`. `fileOpsModeBtn` is the only way `switchMode`
// ever gets called with "file-ops" (see below), so hiding it is enough on
// its own; `fileOpsView` already ships `hidden` in the markup, and chat mode
// is the only view that ever gets shown from then on.
if (!window.__TAURI__) {
  fileOpsModeBtn.hidden = true;
  // No native "browse for a folder" dialog outside Tauri either -- the text
  // field next to it already works fine typed into directly.
  document.getElementById("cfgComfyOutputDirBrowseBtn").hidden = true;
  // Settings' "File Operations" tab configures the sandbox/granted paths --
  // meaningless with no folder-picking mode to use them from at all.
  document.querySelector('.tab-btn[data-tab="fileops"]').hidden = true;
}

let chatModeInitialized = false;

function switchMode(mode) {
  const toChat = mode === "chat";
  fileOpsView.hidden = toChat;
  chatModeView.hidden = !toChat;
  fileOpsModeBtn.classList.toggle("active", !toChat);
  chatModeBtn.classList.toggle("active", toChat);
  // Loaded lazily on first visit, not at startup, so a user who never opens
  // chat mode never pays for the persona/session list calls.
  if (toChat && !chatModeInitialized) {
    chatModeInitialized = true;
    Promise.all([loadPersonaList(), loadSessionList(), refreshImageCapability()])
      .then(() => {
        // Default the persona picker to whichever persona was used most
        // recently (sessions are already most-recent-first), rather than
        // always resetting to "(no persona)" -- a new chat, more often
        // than not, continues with the same persona as the last one. Only
        // on this first visit, and only when nothing's open/chosen yet.
        if (!currentSessionId && !personaSelect.value && lastLoadedSessions[0]?.persona) {
          const persona = lastLoadedSessions[0].persona;
          if ([...personaSelect.options].some((o) => o.value === persona)) {
            personaSelect.value = persona;
          }
        }
      })
      .catch((err) => console.error("chat mode init failed:", err));
  }
}

fileOpsModeBtn.addEventListener("click", () => switchMode("file-ops"));
chatModeBtn.addEventListener("click", () => switchMode("chat"));

// --- State ---

let chatHistory = [];
let currentSessionId = null;

const chatModeLog = document.getElementById("chatModeLog");
const chatModeForm = document.getElementById("chatModeForm");
const chatModeInput = document.getElementById("chatModeInput");
const chatModeSendBtn = document.getElementById("chatModeSendBtn");
const personaSelect = document.getElementById("personaSelect");
const chatSessionList = document.getElementById("chatSessionList");

// --- Personas ---

async function loadPersonaList() {
  let personas = [];
  try {
    personas = await invoke("list_personas");
  } catch (err) {
    console.error("list_personas failed:", err);
  }
  const previous = personaSelect.value;
  personaSelect.innerHTML = "";
  const noneOpt = document.createElement("option");
  noneOpt.value = "";
  noneOpt.textContent = "(no persona)";
  personaSelect.appendChild(noneOpt);
  for (const p of personas) {
    const opt = document.createElement("option");
    opt.value = p.name;
    opt.textContent = p.name;
    personaSelect.appendChild(opt);
  }
  if (personas.some((p) => p.name === previous)) {
    personaSelect.value = previous;
  }
}

// Tauri has a native file dialog (`pick_persona_file`/`import_persona`,
// reading the .md straight off disk); a browser has neither, so it drives a
// hidden `<input type=file>` instead and reads the file's text itself --
// `save_new_persona` already takes raw content directly, same as pasting it
// into the "New persona" dialog by hand.
const importPersonaFileInput = document.getElementById("importPersonaFileInput");

document.getElementById("importPersonaBtn").addEventListener("click", async () => {
  if (!window.__TAURI__) {
    importPersonaFileInput.click();
    return;
  }
  try {
    const path = await invoke("pick_persona_file");
    if (!path) return;
    const summary = await invoke("import_persona", { path });
    await loadPersonaList();
    personaSelect.value = summary.name;
  } catch (err) {
    alert(`Import failed: ${err}`);
  }
});

importPersonaFileInput.addEventListener("change", async () => {
  const file = importPersonaFileInput.files[0];
  importPersonaFileInput.value = ""; // allow re-selecting the same file later
  if (!file) return;
  try {
    const content = await readFileAs(file, "readAsText");
    const name = file.name.replace(/\.md$/i, "");
    const summary = await invoke("save_new_persona", { name, content });
    await loadPersonaList();
    personaSelect.value = summary.name;
  } catch (err) {
    alert(`Import failed: ${err}`);
  }
});

// One dialog serves both "New persona" and "Edit persona" -- editing can't
// rename (the backend keys a persona by filename, and a rename is really a
// separate delete+create), so the name field is just locked while editing
// rather than building a second, near-identical dialog.
const newPersonaDialog = document.getElementById("newPersonaDialog");
const newPersonaDialogTitle = document.getElementById("newPersonaDialogTitle");
const newPersonaNameInput = document.getElementById("newPersonaName");
const newPersonaContentInput = document.getElementById("newPersonaContent");
let personaDialogMode = "new"; // "new" | "edit"

document.getElementById("newPersonaBtn").addEventListener("click", () => {
  personaDialogMode = "new";
  newPersonaDialogTitle.textContent = "New persona";
  newPersonaNameInput.value = "";
  newPersonaNameInput.disabled = false;
  newPersonaContentInput.value = "";
  focusMainWindow();
  newPersonaDialog.hidden = false;
});

document.getElementById("editPersonaBtn").addEventListener("click", async () => {
  const name = personaSelect.value;
  if (!name) {
    alert("Select a persona to edit first.");
    return;
  }
  try {
    const content = await invoke("get_persona_content", { name });
    personaDialogMode = "edit";
    newPersonaDialogTitle.textContent = `Edit persona: ${name}`;
    newPersonaNameInput.value = name;
    newPersonaNameInput.disabled = true;
    newPersonaContentInput.value = content;
    focusMainWindow();
    newPersonaDialog.hidden = false;
  } catch (err) {
    alert(`Could not load that persona: ${err}`);
  }
});

document.getElementById("newPersonaCancelBtn").addEventListener("click", () => {
  newPersonaDialog.hidden = true;
});
document.getElementById("newPersonaSaveBtn").addEventListener("click", async () => {
  const name = newPersonaNameInput.value.trim();
  const content = newPersonaContentInput.value;
  if (!name) return;
  try {
    if (personaDialogMode === "edit") {
      await invoke("update_persona", { name, content });
      // Persona content is read fresh every turn; nothing to refresh here.
    } else {
      const summary = await invoke("save_new_persona", { name, content });
      personaSelect.value = summary.name;
    }
    await loadPersonaList();
    newPersonaDialog.hidden = true;
  } catch (err) {
    alert(`Save failed: ${err}`);
  }
});

document.getElementById("deletePersonaBtn").addEventListener("click", async () => {
  const name = personaSelect.value;
  if (!name) return;
  if (!confirm(`Delete persona "${name}"? It's moved to personas/.trash, not permanently removed.`))
    return;
  try {
    await invoke("delete_persona", { name });
    await loadPersonaList();
    alert(`"${name}" moved to personas/.trash -- not permanently deleted.`);
  } catch (err) {
    alert(`Delete failed: ${err}`);
  }
});

// --- Rulesets ---
//
// Two always-present preference texts (seeded by `ruleset::list_rulesets`):
// edit-only, no new/delete. Read fresh on every use, so edits apply next turn.

const rulesetDialog = document.getElementById("rulesetDialog");
const rulesetSelect = document.getElementById("rulesetSelect");
const rulesetContentInput = document.getElementById("rulesetContent");

const RULESET_LABELS = {
  "image-generation-prompt": "Image style rules",
  "web-search": "Search answer policy",
};

async function loadRulesetContent(name) {
  try {
    rulesetContentInput.value = await invoke("get_ruleset_content", { name });
  } catch (err) {
    alert(`Could not load that ruleset: ${err}`);
  }
}

document.getElementById("editRulesetsBtn").addEventListener("click", async () => {
  try {
    const rulesets = await invoke("list_rulesets");
    rulesetSelect.innerHTML = "";
    for (const r of rulesets) {
      const opt = document.createElement("option");
      opt.value = r.name;
      opt.textContent = RULESET_LABELS[r.name] ?? r.name;
      rulesetSelect.appendChild(opt);
    }
    if (rulesets.length > 0) {
      await loadRulesetContent(rulesets[0].name);
    }
    focusMainWindow();
    rulesetDialog.hidden = false;
  } catch (err) {
    alert(`Could not load rulesets: ${err}`);
  }
});

rulesetSelect.addEventListener("change", () => loadRulesetContent(rulesetSelect.value));

document.getElementById("rulesetCloseBtn").addEventListener("click", () => {
  rulesetDialog.hidden = true;
});
document.getElementById("rulesetSaveBtn").addEventListener("click", async () => {
  const name = rulesetSelect.value;
  if (!name) return;
  try {
    await invoke("update_ruleset", { name, content: rulesetContentInput.value });
  } catch (err) {
    alert(`Save failed: ${err}`);
  }
});

// --- Sessions ---

// A session's persona is fixed at creation (`meta.persona`, read fresh by
// by the backend every turn) -- there's no "change persona mid-chat"
// command, so the dropdown is disabled while a session is open to avoid
// implying a choice made there would do anything.
function setPersonaSelectorForOpenSession(persona) {
  personaSelect.value = persona ?? "";
  personaSelect.disabled = true;
  personaSelect.title = "This chat's persona is fixed. Start a new chat to use a different one.";
}

function setPersonaSelectorForNewSession() {
  personaSelect.disabled = false;
  personaSelect.title = "Persona for the next new chat";
}

// Kept around (most-recent-first, as returned) so the chat-mode init above
// can default the persona picker to the last one actually used, without a
// second round-trip.
let lastLoadedSessions = [];

async function loadSessionList() {
  let sessions = [];
  try {
    sessions = await invoke("list_chat_sessions");
  } catch (err) {
    console.error("list_chat_sessions failed:", err);
  }
  lastLoadedSessions = sessions;
  chatSessionList.innerHTML = "";
  if (sessions.length === 0) {
    const li = document.createElement("li");
    li.className = "chat-session-empty";
    li.textContent = 'No chats yet — click "New chat" to start one.';
    chatSessionList.appendChild(li);
    return;
  }
  for (const s of sessions) {
    const li = document.createElement("li");
    if (s.id === currentSessionId) li.classList.add("active");

    const title = document.createElement("span");
    title.className = "chat-session-item-title";
    title.textContent = s.title;
    title.title = s.persona ? `${s.title} (${s.persona})` : s.title;

    const actions = document.createElement("span");
    actions.className = "chat-session-item-actions";
    const renameBtn = document.createElement("button");
    renameBtn.type = "button";
    setIcon(renameBtn, "pencil", 14);
    renameBtn.setAttribute("aria-label", "Rename");
    renameBtn.title = "Rename";
    renameBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      openRenameDialog(s.id, s.title);
    });
    const deleteBtn = document.createElement("button");
    deleteBtn.type = "button";
    setIcon(deleteBtn, "trash-2", 14);
    deleteBtn.setAttribute("aria-label", "Delete");
    deleteBtn.title = "Delete";
    deleteBtn.addEventListener("click", async (e) => {
      e.stopPropagation();
      if (!confirm(`Delete chat "${s.title}"? This can't be undone.`)) return;
      try {
        await invoke("delete_chat_session", { sessionId: s.id });
        if (currentSessionId === s.id) {
          currentSessionId = null;
          chatHistory = [];
          chatModeLog.innerHTML = "";
          setPersonaSelectorForNewSession();
          updateSendButton();
        }
        await loadSessionList();
      } catch (err) {
        alert(`Delete failed: ${err}`);
      }
    });
    actions.appendChild(renameBtn);
    actions.appendChild(deleteBtn);

    li.appendChild(title);
    li.appendChild(actions);
    li.addEventListener("click", () => openChatSession(s.id));
    chatSessionList.appendChild(li);
  }
}

const renameSessionDialog = document.getElementById("renameSessionDialog");
let renameSessionTargetId = null;

function openRenameDialog(id, currentTitle) {
  renameSessionTargetId = id;
  document.getElementById("renameSessionInput").value = currentTitle;
  focusMainWindow();
  renameSessionDialog.hidden = false;
}
document.getElementById("renameSessionCancelBtn").addEventListener("click", () => {
  renameSessionDialog.hidden = true;
});
document.getElementById("renameSessionSaveBtn").addEventListener("click", async () => {
  const title = document.getElementById("renameSessionInput").value.trim();
  const id = renameSessionTargetId;
  renameSessionDialog.hidden = true;
  if (!title || !id) return;
  try {
    await invoke("rename_chat_session", { sessionId: id, title });
    await loadSessionList();
  } catch (err) {
    alert(`Rename failed: ${err}`);
  }
});

// Read-only peek at the current session's `state.md` snapshot -- no
// textarea, nothing to save, just Refresh and Close buttons.
const chatStateDialog = document.getElementById("chatStateDialog");

// The state-update turn runs as a detached background task after a reply,
// not before it (see chat_turn.rs's module doc comment) -- opening this
// dialog right after sending a message can catch it still in flight, and
// there's no push notification for when it finishes. Refresh re-fetches in
// place rather than making that a close-and-reopen dance.
//
// Two tabs, both derived from the same fetch: "Raw JSON" is `state.json`
// itself (the full character sheet, source of truth), "Summary" is
// `state.md` with its precise/bolded fields stripped back out -- those are
// already shown verbatim in the raw tab, so repeating them in the summary
// would just be the same values twice. Kept as a display-only split (no new
// backend shape) since `state.md` already composes both halves from the
// same source; see `chat_prompts::build_state_markdown`.
let chatStateTab = "summary";
let chatStateSummaryMd = "";
let chatStateRawJson = "";

function chatStateNarrativeOnly(md) {
  return md
    .split("\n")
    .filter((line) => !/^\*\*.+\*\*\s*:/.test(line.trim()))
    .join("\n")
    .trim();
}

function renderChatStateTab() {
  const content = document.getElementById("chatStateContent");
  if (chatStateTab === "raw") {
    const trimmed = chatStateRawJson.trim();
    let formatted = "(nothing recorded yet)";
    if (trimmed && trimmed !== "{}") {
      try {
        formatted = JSON.stringify(JSON.parse(trimmed), null, 2);
      } catch {
        // Shouldn't happen -- the state-update turn validates before ever
        // writing state.json -- but show it raw rather than hide it.
        formatted = trimmed;
      }
    }
    content.textContent = formatted;
  } else {
    const narrative = chatStateNarrativeOnly(chatStateSummaryMd);
    content.innerHTML = narrative ? renderMarkdown(narrative) : escapeHtml("(nothing recorded yet)");
  }
}

async function loadChatStateDialogContent() {
  const [summaryMd, rawJson] = await Promise.all([
    invoke("get_chat_state", { sessionId: currentSessionId }),
    invoke("get_chat_raw_state", { sessionId: currentSessionId }),
  ]);
  chatStateSummaryMd = summaryMd;
  chatStateRawJson = rawJson;
  renderChatStateTab();
}

document.querySelectorAll(".chat-state-tab").forEach((btn) => {
  btn.addEventListener("click", () => {
    if (btn.dataset.tab === chatStateTab) return;
    chatStateTab = btn.dataset.tab;
    document.querySelectorAll(".chat-state-tab").forEach((b) => b.classList.toggle("active", b === btn));
    renderChatStateTab();
  });
});

document.getElementById("viewStateBtn").addEventListener("click", async () => {
  if (!currentSessionId) {
    alert("Open or start a chat first.");
    return;
  }
  try {
    await loadChatStateDialogContent();
    focusMainWindow();
    chatStateDialog.hidden = false;
  } catch (err) {
    alert(`Could not read chat state: ${err}`);
  }
});
document.getElementById("chatStateRefreshBtn").addEventListener("click", async () => {
  try {
    await loadChatStateDialogContent();
  } catch (err) {
    alert(`Could not read chat state: ${err}`);
  }
});
document.getElementById("chatStateCloseBtn").addEventListener("click", () => {
  chatStateDialog.hidden = true;
});

document.getElementById("newChatSessionBtn").addEventListener("click", async () => {
  try {
    const persona = personaSelect.value || null;
    const summary = await invoke("create_chat_session", { persona });
    await loadSessionList();
    await openChatSession(summary.id);
  } catch (err) {
    alert(`Could not create a new chat: ${err}`);
  }
});

// Re-draws the log from the `chatHistory` display cache (no round-trip, so
// the narration toggle takes effect at once). The backend owns the real
// history; this only mirrors what was loaded or has arrived since.
// `m.images` (data URLs) is the only per-message attachment data persisted;
// text attachments are folded into `content` at send time.
function attachmentsFromMessage(m) {
  return (m.images || []).map((data) => ({ kind: "image", name: "image", data }));
}

// A message's thinking bubble (if enabled and present) followed by its reply bubble.
function appendAssistantMessage(m) {
  if (m.thinking && (currentConfig?.chat_show_thinking ?? true)) {
    appendCompletedThinkingBubble(m.thinking);
  }
  return appendChatModeBubble("assistant", m.content);
}

function attachGeneratedImages(bubble, paths) {
  for (const path of paths || []) {
    loadGeneratedImage(path)
      .then((dataUrl) => {
        const img = document.createElement("img");
        img.className = "chat-generated-image";
        img.src = dataUrl;
        wireImagePreview(img, path);
        bubble.appendChild(img);
      })
      .catch((err) => console.error("read_generated_image failed:", err));
  }
}

function renderChatHistoryLog() {
  // Only this session's in-flight nodes survive the redraw; another chat's
  // would otherwise show up here (audit B1).
  const transient = Array.from(chatModeLog.querySelectorAll(".chat-transient")).filter(
    (el) => el.dataset.sessionId === currentSessionId,
  );
  chatModeLog.innerHTML = "";
  for (const m of chatHistory) {
    const bubble =
      m.role === "assistant"
        ? appendAssistantMessage(m)
        : appendChatModeBubble(m.role, m.content, attachmentsFromMessage(m));
    attachGeneratedImages(bubble, m.generated_images);
  }
  for (const el of transient) {
    chatModeLog.appendChild(el);
  }
  if (transient.length > 0) {
    chatModeLog.scrollTop = chatModeLog.scrollHeight;
  }
}

// Out-of-order guard: two quick clicks can resolve in either order.
let openSessionToken = 0;

async function openChatSession(id) {
  const token = ++openSessionToken;
  try {
    const { meta, history } = await invoke("load_chat_session", { sessionId: id });
    if (token !== openSessionToken) return;
    currentSessionId = id;
    chatHistory = history;
    setPersonaSelectorForOpenSession(meta.persona);
    renderChatHistoryLog();
    updateSendButton();
    await loadSessionList();
  } catch (err) {
    if (token === openSessionToken) alert(`Could not open that chat: ${err}`);
  }
}

// --- Attachments ---
//
// Read entirely client-side via FileReader -- the webview is a real browser
// engine, so there's no need to round-trip a picked path through a Tauri
// command just to get bytes back. Images become base64 data URLs sent as
// `ChatMessage.images` (see `llm.rs`); text documents are just folded into
// the message text (`content`), so they need no backend support at all.

const chatAttachBtn = document.getElementById("chatAttachBtn");
const chatAttachInput = document.getElementById("chatAttachInput");
const chatAttachmentsPreview = document.getElementById("chatAttachmentsPreview");
const chatModeMain = document.getElementById("chatModeMain");

// Each entry: { kind: "image"|"text", name, data }. `data` is a `data:` URL
// for images, plain text content for documents.
let pendingAttachments = [];

const MAX_IMAGE_ATTACHMENT_BYTES = 5 * 1024 * 1024;
const MAX_TEXT_ATTACHMENT_BYTES = 200 * 1024;

// Mirrors the `accept` attribute on #chatAttachInput -- that's only a native
// picker hint (and isn't consulted by drop/paste at all), so this is the
// real, enforced allowlist.
const ALLOWED_IMAGE_EXTENSIONS = ["png", "jpg", "jpeg", "gif", "webp"];
const ALLOWED_TEXT_EXTENSIONS = [
  "txt", "md", "csv", "json", "log", "js", "py", "rs", "html", "css", "yaml", "yml", "toml",
];

function readFileAs(file, method) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result);
    reader.onerror = () => reject(reader.error);
    reader[method](file);
  });
}

// Extension is authoritative when there is one -- an `image/*` MIME type
// alone used to be enough to wave a file through (e.g. `.svg`, `.bmp`, or
// anything else the OS happens to label `image/...`), which defeated the
// allowlist entirely. MIME is only a fallback for the rare file with no
// extension at all -- some clipboard-pasted images come through that way.
function classifyAttachment(file) {
  const ext = file.name.includes(".") ? file.name.split(".").pop().toLowerCase() : "";
  if (ALLOWED_IMAGE_EXTENSIONS.includes(ext)) return "image";
  if (ALLOWED_TEXT_EXTENSIONS.includes(ext)) return "text";
  if (!ext && file.type.startsWith("image/")) return "image";
  return null;
}

// The one ingestion path for the file picker, drag-and-drop, and paste --
// each just gathers a FileList/File[] and hands it here, so type/size
// guarding and the read/preview logic exists exactly once.
async function handleIncomingFiles(fileList) {
  for (const file of [...fileList]) {
    const kind = classifyAttachment(file);
    if (!kind) {
      appendChatModeBubble("system", `⚠ ${file.name}: unsupported file type, not attached.`);
      continue;
    }
    const limit = kind === "image" ? MAX_IMAGE_ATTACHMENT_BYTES : MAX_TEXT_ATTACHMENT_BYTES;
    if (file.size > limit) {
      appendChatModeBubble(
        "system",
        `⚠ ${file.name} is too large (max ${Math.round(limit / 1024)}KB), not attached.`,
      );
      continue;
    }
    try {
      const data = await readFileAs(file, kind === "image" ? "readAsDataURL" : "readAsText");
      pendingAttachments.push({ kind, name: file.name, data });
    } catch (err) {
      appendChatModeBubble("system", `⚠ Could not read ${file.name}: ${err}`);
    }
  }
  renderAttachmentsPreview();
}

chatAttachBtn.addEventListener("click", () => chatAttachInput.click());

chatAttachInput.addEventListener("change", async () => {
  // `.files` is a *live* FileList tied to the input -- snapshot it into a
  // plain array before resetting `.value`, or the reset empties it out from
  // under `handleIncomingFiles` before a single file is ever read.
  const files = [...chatAttachInput.files];
  chatAttachInput.value = ""; // allow re-selecting the same file later
  await handleIncomingFiles(files);
});

// No hover highlight here on purpose -- dragging a file over the window is
// currently unreliable (see project NOTES.md), and a highlight that lights
// up for a drop that then does nothing is worse than no feedback at all.
// `dragover`'s preventDefault is still required for `drop` to ever fire, so
// that part stays even without the visual.
["dragenter", "dragover"].forEach((evt) =>
  chatModeMain.addEventListener(evt, (e) => e.preventDefault()),
);
chatModeMain.addEventListener("drop", async (e) => {
  e.preventDefault();
  if (e.dataTransfer?.files?.length) await handleIncomingFiles(e.dataTransfer.files);
});

chatModeInput.addEventListener("paste", async (e) => {
  const items = e.clipboardData?.items;
  if (!items) return;
  const files = [...items]
    .filter((i) => i.kind === "file")
    .map((i) => i.getAsFile())
    .filter(Boolean);
  if (files.length > 0) {
    e.preventDefault(); // otherwise the raw image/binary also lands as pasted text
    await handleIncomingFiles(files);
  }
});

function renderAttachmentsPreview() {
  chatAttachmentsPreview.innerHTML = "";
  chatAttachmentsPreview.hidden = pendingAttachments.length === 0;
  pendingAttachments.forEach((att, i) => {
    const chip = document.createElement("div");
    chip.className = "chat-attachment-chip";
    if (att.kind === "image") {
      const img = document.createElement("img");
      img.src = att.data;
      chip.appendChild(img);
    }
    const name = document.createElement("span");
    name.className = "chat-attachment-name";
    name.textContent = att.name;
    chip.appendChild(name);
    const removeBtn = document.createElement("button");
    removeBtn.type = "button";
    setIcon(removeBtn, "x", 12);
    removeBtn.setAttribute("aria-label", "Remove");
    removeBtn.title = "Remove";
    removeBtn.addEventListener("click", () => {
      pendingAttachments.splice(i, 1);
      renderAttachmentsPreview();
    });
    chip.appendChild(removeBtn);
    chatAttachmentsPreview.appendChild(chip);
  });
}

// --- Sending ---
//
// The backend owns history: a send posts only `{sessionId, text, images}` and
// the UI renders from events, keeping `chatHistory` as a display cache. Every
// turn captures `sid` at submit time and renders only while
// `currentSessionId === sid` (audit B1); the backend persists either way.

// Sessions with a turn-1 stream in flight; Send acts as Stop for these.
const streamingSessions = new Set();
// Set synchronously on submit so a double Enter can't create two sessions (B10).
let creatingSession = false;
let imageGenConfigured = false;

const chatCameraBtn = document.getElementById("chatCameraBtn");

function updateSendButton() {
  const streaming = streamingSessions.has(currentSessionId);
  chatModeSendBtn.textContent = streaming ? "Stop" : "Send";
  chatModeSendBtn.type = streaming ? "button" : "submit";
  chatModeSendBtn.classList.toggle("stop-btn", streaming);
  chatModeSendBtn.disabled = creatingSession;
  chatCameraBtn.disabled = streaming || creatingSession || !imageGenConfigured;
  chatCameraBtn.title = imageGenConfigured
    ? "Ask for a photo (same as typing /pic)"
    : "Set up ComfyUI in Settings to enable photos";
}

// Also called after Settings saves, since ComfyUI config can change there.
async function refreshImageCapability() {
  try {
    const cfg = await invoke("get_comfyui_config");
    imageGenConfigured = !!(cfg.base_url?.trim() && cfg.workflow_json?.trim());
  } catch (err) {
    console.error("get_comfyui_config failed:", err);
    imageGenConfigured = false;
  }
  updateSendButton();
}

chatModeSendBtn.addEventListener("click", async () => {
  if (chatModeSendBtn.type !== "button") return; // acts as Stop only while streaming
  try {
    await invoke("stop_chat_generation", { sessionId: currentSessionId });
  } catch (err) {
    console.error("stop_chat_generation failed:", err);
  }
});

chatModeInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    if (!creatingSession && !streamingSessions.has(currentSessionId)) {
      chatModeForm.requestSubmit();
    }
  }
});

// Passive vision probe, fired once per endpoint+model in the background on
// the first send. Its result (`visionConfirmed`: null = unknown) goes to
// `generate_chat_image` so the reaction turn attaches the picture only when
// the model can actually see it.
let visionConfirmed = null;
let visionProbePromise = null;
// Endpoint+model the probe answered for; settings can change either between
// sends, and a stale "no vision" must not stick after switching models.
let visionProbedFor = null;
function ensureVisionProbe() {
  if (!currentConfig) return visionProbePromise;
  const target = `${currentConfig.endpoint}::${currentConfig.model}`;
  if (visionProbePromise && visionProbedFor === target) return visionProbePromise;
  visionProbedFor = target;
  const el = document.getElementById("visionStatusIndicator");
  el.classList.remove("vision-ok", "vision-off");
  setIcon(el, "eye", 14);
  el.title = "Checking vision support…";
  visionProbePromise = invoke("test_vision_support", {
    endpoint: currentConfig.endpoint,
    model: currentConfig.model,
    apiKey: currentConfig.api_key,
  })
    .then(() => {
      visionConfirmed = true;
      el.title = "Vision ready — this model can see attached images";
      el.classList.add("vision-ok");
    })
    .catch((err) => {
      visionConfirmed = false;
      el.title = `No vision support detected: ${err} -- generated photos will be described to it as text only`;
      el.classList.add("vision-off");
    });
  return visionProbePromise;
}

// Mirrors the backend's `/pic [hint]` parse, only to show what it will store.
const PIC_RE = /^\/pic(?:\s+([\s\S]*))?$/i;

chatModeForm.addEventListener("submit", (e) => {
  e.preventDefault();
  sendChatMessage(chatModeInput.value.trim());
});

chatCameraBtn.addEventListener("click", () => {
  const typed = chatModeInput.value.trim();
  sendChatMessage(PIC_RE.test(typed) ? typed : `/pic ${typed}`.trim());
});

// Whether a cached message with this id is already there (a reopen can load
// it from disk before the matching event arrives).
function chatCacheHas(id) {
  return !!id && chatHistory.some((m) => m.id === id);
}

// Cache copy of a message; thinking only if the backend persists it too, so a
// redraw matches what a reopen would show (audit B8).
function cacheMessage(id, content, thinking) {
  const m = { id, role: "assistant", content };
  if (thinking && currentConfig?.chat_persist_thinking) m.thinking = thinking;
  chatHistory.push(m);
}

async function sendChatMessage(typed) {
  if (creatingSession || streamingSessions.has(currentSessionId)) return;
  const picMatch = PIC_RE.exec(typed);
  if (!typed && pendingAttachments.length === 0) return;
  ensureVisionProbe(); // not awaited -- this shouldn't delay sending

  creatingSession = true; // before the first await
  updateSendButton();
  let sid = currentSessionId;
  try {
    if (!sid) {
      // Sending with nothing open implicitly starts a chat.
      const persona = personaSelect.value || null;
      const summary = await invoke("create_chat_session", { persona });
      sid = summary.id;
      currentSessionId = sid;
      chatHistory = [];
      setPersonaSelectorForOpenSession(persona);
      await loadSessionList();
    }
  } catch (err) {
    alert(`Could not start a chat: ${err}`);
    return;
  } finally {
    creatingSession = false;
    updateSendButton();
  }
  streamingSessions.add(sid);
  updateSendButton();
  const isCurrent = () => currentSessionId === sid;

  // `/pic` keeps text documents pending: the hint is a prompt, not a place for file bodies.
  const attachments = picMatch
    ? pendingAttachments.filter((a) => a.kind === "image")
    : pendingAttachments;
  pendingAttachments = picMatch ? pendingAttachments.filter((a) => a.kind !== "image") : [];
  renderAttachmentsPreview();

  const images = attachments.filter((a) => a.kind === "image").map((a) => a.data);
  let sentText = typed;
  let shownText;
  let cachedText;
  if (picMatch) {
    shownText = (picMatch[1] ?? "").trim() || "(asks for a photo)";
    cachedText = shownText;
  } else {
    // Documents need no backend support -- their content becomes message text.
    sentText = attachments
      .filter((a) => a.kind === "text")
      .reduce((acc, a) => `[attached: ${a.name}]\n${a.data}\n\n${acc}`, typed);
    if (!sentText.trim() && images.length > 0) sentText = "What's in this image?";
    shownText = typed || "(sent with attachment)"; // not the folded document text
    cachedText = sentText;
  }

  chatModeInput.value = "";
  const userMsg = { id: null, role: "user", content: cachedText, images };
  let userBubble = null;
  if (isCurrent()) {
    userBubble = appendChatModeBubble("user", shownText, attachments);
    chatHistory.push(userMsg);
  }
  // The backend drops the user message when a turn yields no reply, so the
  // bubble goes too and what was typed returns to the input for a retry.
  const rollbackUserTurn = () => {
    userBubble?.remove();
    const i = chatHistory.indexOf(userMsg);
    if (i >= 0) chatHistory.splice(i, 1);
    if (isCurrent() && !chatModeInput.value) {
      chatModeInput.value = typed;
      pendingAttachments = attachments.concat(pendingAttachments);
      renderAttachmentsPreview();
    }
  };

  const showThinking = currentConfig?.chat_show_thinking ?? true;
  const streamReplies = currentConfig?.chat_stream_replies ?? true;
  const thinkingPlaceholder = showThinking && isCurrent() ? createChatThinkingPlaceholder(sid) : null;
  // `chat_stream_replies` only controls whether content deltas show live;
  // the call is always streamed.
  const liveBubble = streamReplies && isCurrent() ? createLiveReplyBubble(sid) : null;
  let liveRawReply = "";
  try {
    const result = await invokeChatStream(sid, sentText, images, (delta) => {
      if (delta.kind === "content") {
        liveRawReply += delta.text;
        // Legacy `//`/`||` markers are stripped from this live
        // view only (`*action*` shows as typed), recomputed from the full text since a marker can
        // straddle two chunks. The final render parses them properly.
        if (liveBubble?.isConnected) {
          liveBubble.textContent = liveRawReply.replace(/\/\/|\|\|/g, "");
          chatModeLog.scrollTop = chatModeLog.scrollHeight;
        }
      } else if (delta.kind === "reasoning" && thinkingPlaceholder?.pending.isConnected) {
        appendChatThinkingDelta(thinkingPlaceholder, delta.text);
      }
    });
    liveBubble?.remove();
    thinkingPlaceholder?.pending.remove();
    loadSessionList(); // title/ordering may have just changed

    const reply = result.reply ?? "";
    if (result.stopped && !reply.trim()) {
      rollbackUserTurn(); // stopped before any text: nothing was kept
      return;
    }
    let turn1Bubble = null;
    if (isCurrent()) {
      if (chatHistory.includes(userMsg)) userMsg.id = result.userMessageId;
      if (reply.trim() && !chatCacheHas(result.assistantMessageId)) {
        cacheMessage(result.assistantMessageId, reply, result.thinking);
        turn1Bubble = appendAssistantMessage({ content: reply, thinking: result.thinking });
      }
      if (result.summarized) {
        const n = typeof result.summarized === "number" ? ` ${result.summarized}` : "";
        appendChatModeBubble(
          "system",
          `Conversation got long — summarized${n} older message(s) to fit.`,
        );
      }
      if (result.dropped > 0) {
        appendChatModeBubble(
          "system",
          `Conversation got long — dropped ${result.dropped} oldest message(s) to stay within the context window.`,
        );
      }
    }
    // A stopped reply is saved but gets no photo/search decision.
    if (reply.trim() && !result.stopped) {
      runChatFollowup(sid, result, turn1Bubble); // not awaited; the next send needn't wait
    }
  } catch (err) {
    liveBubble?.remove();
    thinkingPlaceholder?.pending.remove();
    rollbackUserTurn();
    if (isCurrent()) appendChatModeBubble("system", `Error: ${err}`);
    else console.error("chat turn failed in a background session:", err);
  } finally {
    streamingSessions.delete(sid);
    updateSendButton();
  }
}

// Follow-up after turn 1: the backend updates state and decides on a photo
// and/or a web search. Image generation and search run unawaited; the backend
// serializes by message id, and every render re-checks the pinned session.
async function runChatFollowup(sid, result, turn1Bubble) {
  const mask = currentSessionId === sid ? createChatThinkingPlaceholder(sid) : null;
  let plan;
  try {
    plan = await invoke("run_chat_followup", {
      sessionId: sid,
      assistantMessageId: result.assistantMessageId,
      photoForced: !!result.photoForced,
      photoHint: result.photoHint ?? null,
    });
  } catch (err) {
    console.debug(`chat follow-up failed: ${err}`);
    return;
  } finally {
    mask?.pending.remove(); // pure masking; there's never real reasoning to show
  }
  if (plan.stateUpdated && currentSessionId === sid) appendStateDispatchedIndicator(turn1Bubble);
  if (plan.image) runImageFollowup(sid, result.assistantMessageId, plan.image);
  if (plan.searchQuery) runSearchFollowup(sid, plan.searchQuery);
}

async function runImageFollowup(sid, messageId, image) {
  const placeholder = currentSessionId === sid ? createImageGenPlaceholder(sid) : null;
  let gen;
  try {
    gen = await invoke("generate_chat_image", {
      sessionId: sid,
      messageId,
      fields: image.fields,
      vision: visionConfirmed,
    });
  } catch (err) {
    if (placeholder) resolveImageGenPlaceholder(placeholder, null, null, String(err));
    else console.error("generate_chat_image failed:", err);
    return;
  }
  cacheGeneratedImage(gen.imagePath, gen.imageDataUrl);
  if (currentSessionId !== sid) return; // the backend persisted it; a reopen shows it

  // Mirror into the display cache so redraws rebuild image and reaction.
  const msg = chatHistory.find((m) => m.id === messageId);
  if (msg && !(msg.generated_images || []).includes(gen.imagePath)) {
    msg.generated_images = [...(msg.generated_images || []), gen.imagePath];
  }
  const reaction = gen.reaction?.text ? gen.reaction : null;
  const isNew = reaction && !chatCacheHas(reaction.id);
  if (isNew) cacheMessage(reaction.id, reaction.text, reaction.thinking);

  if (placeholder?.isConnected) {
    resolveImageGenPlaceholder(placeholder, gen.imageDataUrl, gen.imagePath);
    if (isNew) appendAssistantMessage({ content: reaction.text, thinking: reaction.thinking });
  } else {
    // The user left and came back, so the placeholder node is gone: redraw from the cache.
    renderChatHistoryLog();
  }
}

async function runSearchFollowup(sid, query) {
  const placeholder = currentSessionId === sid ? createWebSearchPlaceholder(sid) : null;
  let res;
  try {
    res = await invoke("run_chat_web_search", { sessionId: sid, query });
  } catch (err) {
    if (placeholder) resolveWebSearchPlaceholder(placeholder, String(err));
    else console.error("run_chat_web_search failed:", err);
    return;
  }
  if (placeholder) resolveWebSearchPlaceholder(placeholder);
  const answer = res.answer?.text ? res.answer : null;
  if (!answer || currentSessionId !== sid || chatCacheHas(answer.id)) return;
  cacheMessage(answer.id, answer.text, answer.thinking);
  appendAssistantMessage({ content: answer.text, thinking: answer.thinking });
}
