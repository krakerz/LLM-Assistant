// Only chat mode -- not file-ops mode, which needs a real sandboxed folder
// and has no meaning served to a remote browser -- runs outside the Tauri
// desktop shell too, via `llm-assistant --server` (see `src/server.rs`).
// `window.__TAURI__` only exists inside that shell, so its absence is what
// tells the rest of this file which backend it's actually talking to.
const invoke = window.__TAURI__
  ? window.__TAURI__.core.invoke
  : async (cmd, args = {}) => {
      const res = await fetch(`/api/${cmd}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(args),
      });
      if (res.status === 401) {
        // The session cookie `/api/login` set is gone or was never there
        // (server restarted, or someone else linked in without it) -- show
        // the same gate a fresh visit would have, rather than let this
        // surface as an opaque error wherever the call happened to be made.
        showLoginOverlay();
        throw "Session expired -- please log in again.";
      }
      if (!res.ok) {
        const body = await res.json().catch(() => null);
        throw body?.error ?? res.statusText;
      }
      return res.status === 204 ? undefined : res.json();
    };

// Generic SSE-style streaming transport, mirroring `invoke`'s own Tauri/
// `--server` fork above -- used only by chat mode's turn-1 reply for now
// (see `invokeChatStream` below). `onEvent` is called once per parsed
// backend event, an already-parsed JS object shaped like
// `{ type: "content" | "reasoning" | "done" | "error", ... }`. This
// function's own returned promise resolving does NOT mean the reply is
// done -- under Tauri it resolves as soon as the backend command itself
// returns, which happens before the last channel message is necessarily
// delivered; only an `onEvent` "done"/"error" event means the reply is
// actually finished (see `invokeChatStream`, which wraps this into a real
// completion promise).
async function invokeStream(cmd, args, onEvent) {
  if (window.__TAURI__) {
    const channel = new window.__TAURI__.core.Channel();
    channel.onmessage = onEvent;
    return invoke(cmd, { ...args, channel });
  }
  const res = await fetch(`/api/${cmd}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(args),
  });
  if (res.status === 401) {
    showLoginOverlay();
    throw "Session expired -- please log in again.";
  }
  if (!res.ok) {
    const body = await res.json().catch(() => null);
    throw body?.error ?? res.statusText;
  }
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    let idx;
    while ((idx = buf.indexOf("\n")) !== -1) {
      const line = buf.slice(0, idx).replace(/\r$/, "");
      buf = buf.slice(idx + 1);
      if (!line.startsWith("data:")) continue; // blank line, or a `:` keep-alive comment
      const payload = line.slice(5).trim();
      if (payload) onEvent(JSON.parse(payload));
    }
  }
}

// Wraps invokeStream in a real Promise, resolved/rejected only by the
// backend's own "done"/"error" event -- never by invokeStream's own return
// value (see its doc comment above). `onDelta` fires for every "content"/
// "reasoning" chunk as it arrives, with a small, transport-agnostic shape
// (`{ kind: "content" | "reasoning", text }`) so the caller doesn't need to
// know which backend event names produced it.
function invokeChatStream(sessionId, history, onDelta) {
  return new Promise((resolve, reject) => {
    const onEvent = (event) => {
      if (event.type === "content") onDelta({ kind: "content", text: event.text });
      else if (event.type === "reasoning") onDelta({ kind: "reasoning", text: event.text });
      else if (event.type === "done") resolve(event.result);
      else if (event.type === "error") reject(event.message);
    };
    invokeStream("send_chat_message_streaming", { sessionId, history }, onEvent).catch(reject);
  });
}

// `--server`'s login gate (see `src/server.rs`) -- a custom form instead of
// the browser's own Basic Auth prompt, which some browsers render
// inconsistently (or show the raw 401 body instead of prompting at all) for
// a plain top-level navigation. `/api/auth_check` and `/api/login` are the
// two routes that stay reachable with no session yet, so this can run
// before anything else does. Resolves `true` once it's safe to continue
// initializing the rest of the app -- either no password is configured, or
// this tab already has a valid session -- and `false` while the overlay is
// up waiting on one, in which case the caller should stop there; a
// successful login just reloads the page rather than trying to resume
// whatever init was mid-flight.
// `<body>` ships with the `app-not-ready` class (see style.css), which
// keeps the entire app shell off-screen -- every path through here either
// reveals it (`revealApp`) or leaves it hidden behind the login overlay;
// nothing else in this file un-hides it.
function revealApp() {
  document.body.classList.remove("app-not-ready");
}

async function ensureAuthenticated() {
  if (window.__TAURI__) {
    revealApp();
    return true;
  }
  let status;
  try {
    status = await fetch("/api/auth_check").then((r) => r.json());
  } catch (err) {
    console.error("auth_check failed, continuing without it:", err);
    revealApp(); // fail open rather than lock the user out over a network hiccup
    return true;
  }
  if (!status.required || status.authenticated) {
    revealApp();
    return true;
  }
  showLoginOverlay();
  return false;
}

function showLoginOverlay() {
  document.getElementById("loginOverlay").hidden = false;
  document.getElementById("loginPassword").focus();
}

function wireLoginOverlay() {
  const passwordInput = document.getElementById("loginPassword");
  const errorEl = document.getElementById("loginError");

  async function submit() {
    errorEl.textContent = "";
    try {
      const res = await fetch("/api/login", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ password: passwordInput.value }),
      });
      if (!res.ok) {
        errorEl.textContent = "Wrong password.";
        return;
      }
      // Simplest way to safely resume -- whatever init was mid-flight (or
      // hadn't started yet) just runs again from a clean slate, now with a
      // valid session cookie already set.
      location.reload();
    } catch (err) {
      errorEl.textContent = `Login failed: ${err}`;
    }
  }

  document.getElementById("loginSubmitBtn").addEventListener("click", submit);
  passwordInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") submit();
  });
}
wireLoginOverlay();

// If another application currently has the desktop's focus, the app window
// itself -- and so any popup inside it -- can be sitting behind it. Called
// right before every popup opens in this file so it actually appears on
// top. Fire-and-forget: a focus failure shouldn't block the popup itself.
// A no-op in a browser tab, which has no such window to focus.
function focusMainWindow() {
  if (!window.__TAURI__) return;
  window.__TAURI__.window
    .getCurrentWindow()
    .setFocus()
    .catch((err) => console.error("setFocus failed:", err));
}

let history = [];
let currentConfig = null;

const chatLog = document.getElementById("chatLog");
const chatForm = document.getElementById("chatForm");
const chatInput = document.getElementById("chatInput");
const sendBtn = document.getElementById("sendBtn");
const rootPath = document.getElementById("rootPath");

function escapeHtml(s) {
  return s.replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
  );
}

// Minimal Markdown. Escapes everything first so only our own tags survive.
function renderMarkdown(text) {
  const codeBlocks = [];
  let working = text.replace(/```([a-zA-Z]*)\n([\s\S]*?)```/g, (_, _lang, code) => {
    codeBlocks.push(code.replace(/\n$/, ""));
    return ` CODEBLOCK${codeBlocks.length - 1} `;
  });

  working = escapeHtml(working);
  working = working.replace(/`([^`\n]+)`/g, (_, code) => `<code>${code}</code>`);
  working = working.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  working = working.replace(/(?<!\*)\*([^*\n]+)\*(?!\*)/g, "<em>$1</em>");

  working = working.replace(
    / CODEBLOCK(\d+) /g,
    (_, i) => `<pre class="md-code"><code>${escapeHtml(codeBlocks[Number(i)])}</code></pre>`,
  );

  return working;
}

// Mirrors everything shown, thinking steps included, into this session's log.
function logToFile(text) {
  invoke("append_chat_log", { text }).catch((err) => console.error("append_chat_log failed:", err));
}

function appendBubble(role, text, container = chatLog) {
  const div = document.createElement("div");
  div.className = `bubble ${role}`;
  if (role === "assistant") {
    div.innerHTML = renderMarkdown(text);
  } else {
    div.textContent = text;
  }
  container.appendChild(div);
  chatLog.scrollTop = chatLog.scrollHeight;
  logToFile(`[${role}] ${text}`);
  return div;
}

// Collapses propose -> run -> respond steps into one "Thinking" disclosure,
// created lazily so a direct answer never grows one.
function createThinkingTracker() {
  let container = null;
  let summaryEl = null;
  let steps = 0;
  // Catches a model re-proposing the command it just ran (seen looping 20+
  // times). Any different command in between resets it, so a real re-check
  // after an actual change isn't flagged.
  let lastExecutedCommand = null;
  // Second guard: `lastExecutedCommand` alone only catches a command
  // repeated *immediately*. An alternating loop (`ls -F`, `cat notes.txt`,
  // `ls -F`, ...) never repeats back to back, and reproduced as a genuinely
  // endless chain during testing with max automatic steps set to unlimited.
  const executedCounts = new Map();
  return {
    ensure() {
      if (!container) {
        container = document.createElement("details");
        container.className = "bubble thinking";
        summaryEl = document.createElement("summary");
        container.appendChild(summaryEl);
        chatLog.appendChild(container);
      }
      steps++;
      summaryEl.textContent = `Thinking… (${steps} step${steps === 1 ? "" : "s"})`;
      return container;
    },
    // The chain is over: "Thinking…" on a finished, collapsed box reads as
    // still running. Called once when the whole chain unwinds, so it covers
    // every exit path (answer, denial, sudo, stop, step cap, error).
    finish(label) {
      if (!container) return;
      summaryEl.textContent = `${label} (${steps} step${steps === 1 ? "" : "s"})`;
    },
    isImmediateRepeat(cmd) {
      return lastExecutedCommand !== null && cmd === lastExecutedCommand;
    },
    isStuckInCycle(cmd) {
      return (executedCounts.get(cmd) ?? 0) >= STUCK_LOOP_REPEAT_THRESHOLD;
    },
    recordExecuted(cmd) {
      lastExecutedCommand = cmd;
      executedCounts.set(cmd, (executedCounts.get(cmd) ?? 0) + 1);
    },
  };
}

// Listings are the useful content, so they show expanded; everything else
// collapses, since the next reply is expected to explain it.
const LISTING_BINARIES = ["ls", "find", "tree", "pwd", "du", "df", "stat", "wc"];

function isListingCommand(cmd) {
  const first = cmd.trim().split(/\s+/)[0];
  const bin = first ? first.split("/").pop() : "";
  return LISTING_BINARIES.includes(bin);
}

// These can never gain root (bwrap sets no_new_privs), so skip the
// confirm-and-fail cycle. Every word, not just the first: `cd /x && sudo ...`.
const PRIVILEGE_ESCALATION_BINARIES = ["sudo", "su", "doas", "pkexec"];

function needsElevatedPrivileges(cmd) {
  return cmd
    .trim()
    .split(/\s+/)
    .some((word) => PRIVILEGE_ESCALATION_BINARIES.includes(word.split("/").pop()));
}

function appendOutput(cmd, outcome, container = chatLog) {
  const details = document.createElement("details");
  details.className = "bubble output";
  details.open = isListingCommand(cmd);

  const summary = document.createElement("summary");
  const status = outcome.exit_code === 0 ? "ok" : `exit ${outcome.exit_code}`;
  summary.innerHTML = `$ ${escapeHtml(cmd)}  <span class="badge">${status}</span>`;

  const copyBtn = document.createElement("button");
  copyBtn.type = "button";
  copyBtn.className = "copy-btn";
  copyBtn.textContent = "Copy";
  copyBtn.title = "Copy the command";
  copyBtn.addEventListener("click", (e) => {
    // Otherwise the click also toggles the <details>.
    e.preventDefault();
    e.stopPropagation();
    navigator.clipboard.writeText(cmd).catch((err) => console.error("copy failed:", err));
  });
  summary.appendChild(copyBtn);
  details.appendChild(summary);

  const outputText = [outcome.stdout, outcome.stderr].filter(Boolean).join("\n").trim() || "(no output)";
  const pre = document.createElement("pre");
  pre.textContent = outputText;
  details.appendChild(pre);

  container.appendChild(details);
  chatLog.scrollTop = chatLog.scrollHeight;
  logToFile(`[command] $ ${cmd}  (exit ${outcome.exit_code})\n${outputText}`);
}

// The summary becomes the record the assistant works from, so it's shown
// expanded and editable -- a wrong one is only catchable if the user sees it.
// Matched back into `history` by content, not index: later turns reshuffle.
function appendSummary(summary, count) {
  const details = document.createElement("details");
  details.className = "bubble summary";
  details.open = true;

  const heading = document.createElement("summary");
  heading.textContent = `Summarized ${count} older message${count === 1 ? "" : "s"} to save context — check it`;
  details.appendChild(heading);

  const note = document.createElement("div");
  note.className = "summary-note";
  note.textContent =
    "This replaced those messages in the assistant's memory. It was written by the model, so it can be wrong — edit it if it is.";
  details.appendChild(note);

  const pre = document.createElement("pre");
  pre.textContent = summary;
  details.appendChild(pre);

  const editBtn = document.createElement("button");
  editBtn.type = "button";
  editBtn.className = "link-btn";
  editBtn.textContent = "Edit";
  details.appendChild(editBtn);

  let stored = summary;
  editBtn.addEventListener("click", () => {
    const box = document.createElement("textarea");
    box.className = "summary-edit";
    box.value = pre.textContent;
    box.rows = Math.min(20, box.value.split("\n").length + 2);
    const save = document.createElement("button");
    save.type = "button";
    save.className = "link-btn";
    save.textContent = "Save";
    const cancel = document.createElement("button");
    cancel.type = "button";
    cancel.className = "link-btn";
    cancel.textContent = "Cancel";

    const restore = () => {
      box.remove();
      save.remove();
      cancel.remove();
      pre.hidden = false;
      editBtn.hidden = false;
    };
    pre.hidden = true;
    editBtn.hidden = true;
    details.appendChild(box);
    details.appendChild(save);
    details.appendChild(cancel);
    box.focus();

    cancel.addEventListener("click", restore);
    save.addEventListener("click", () => {
      const edited = box.value.trim();
      const idx = history.findIndex((m) => m.content.includes(stored));
      if (idx === -1) {
        appendBubble(
          "system",
          "That summary is no longer part of the conversation, so the edit wasn't applied.",
        );
        restore();
        return;
      }
      history[idx].content = history[idx].content.replace(stored, edited);
      stored = edited;
      pre.textContent = edited;
      logToFile(`[summary edited by user]\n${edited}`);
      restore();
    });
  });

  chatLog.appendChild(details);
  chatLog.scrollTop = chatLog.scrollHeight;
  logToFile(`[summary of ${count} older messages]\n${summary}`);
}

// Outside `thinking`: the actionable result, not an intermediate step.
function appendManualCommand(cmd) {
  const div = document.createElement("div");
  div.className = "bubble manual-cmd";

  const header = document.createElement("div");
  header.className = "manual-cmd-header";
  const label = document.createElement("span");
  label.textContent = "⚠ Needs sudo — run this yourself, then paste the output back:";
  const copyBtn = document.createElement("button");
  copyBtn.type = "button";
  copyBtn.className = "copy-btn";
  copyBtn.textContent = "Copy";
  copyBtn.addEventListener("click", () => {
    navigator.clipboard.writeText(cmd).catch((err) => console.error("copy failed:", err));
  });
  header.appendChild(label);
  header.appendChild(copyBtn);

  const pre = document.createElement("pre");
  pre.textContent = cmd;

  div.appendChild(header);
  div.appendChild(pre);
  chatLog.appendChild(div);
  chatLog.scrollTop = chatLog.scrollHeight;
  logToFile(`[needs-sudo] ${cmd}`);
}

// Keeps the bubble non-blank when a reply was nothing but a fence.
const COMMAND_ONLY_PLACEHOLDER = "(proposed a command, shown below)";

// Only an explicitly-tagged fence is a proposal; a plain ``` is the model
// showing text. Only the first ever runs, but *every* fence is stripped from
// the display -- a leftover one reads as still-pending and the model will
// report it as done. `extraCommands` counts them so the caller can say so.
// `history` keeps the full text, since that's what the model said.
function parseAssistantReply(text) {
  const regex = /```(?:sh|bash|shell)\n([\s\S]*?)```/g;
  const matches = [...text.matchAll(regex)];
  if (matches.length === 0) {
    return { command: null, displayText: text, extraCommands: 0 };
  }
  const command = matches[0][1].trim();
  let stripped = text;
  for (let i = matches.length - 1; i >= 0; i--) {
    const m = matches[i];
    stripped = stripped.slice(0, m.index) + stripped.slice(m.index + m[0].length);
  }
  // Collapse the blank space left behind by removing the fence(s) --
  // otherwise the paragraph before it and the paragraph after it can end up
  // separated by 3-4 blank lines instead of the normal one.
  const displayText = stripped.replace(/\n{3,}/g, "\n\n").trim() || COMMAND_ONLY_PLACEHOLDER;
  return { command, displayText, extraCommands: matches.length - 1 };
}

// --- Font size ---

const FONT_SIZE_MIN = 11;
const FONT_SIZE_MAX = 22;
const FONT_SIZE_STEP = 1;
const FONT_SIZE_DEFAULT = 14;

// A root custom property, not body.style.fontSize: dialogs render in the top
// layer and don't reliably inherit it.
function applyFontSize(px) {
  document.documentElement.style.setProperty("--ui-font-size", `${px}px`);
  try {
    localStorage.setItem("fontSize", String(px));
  } catch {
    // localStorage can throw in some contexts; font size just won't persist.
  }
}

function currentFontSize() {
  const raw = getComputedStyle(document.documentElement).getPropertyValue("--ui-font-size");
  const stored = parseInt(raw, 10);
  return Number.isFinite(stored) ? stored : FONT_SIZE_DEFAULT;
}

(function initFontSize() {
  let stored = FONT_SIZE_DEFAULT;
  try {
    const raw = parseInt(localStorage.getItem("fontSize"), 10);
    if (Number.isFinite(raw)) stored = raw;
  } catch {
    // ignore, use default
  }
  applyFontSize(Math.min(FONT_SIZE_MAX, Math.max(FONT_SIZE_MIN, stored)));
})();

document.getElementById("fontIncBtn").addEventListener("click", () => {
  applyFontSize(Math.min(FONT_SIZE_MAX, currentFontSize() + FONT_SIZE_STEP));
});

document.getElementById("fontDecBtn").addEventListener("click", () => {
  applyFontSize(Math.max(FONT_SIZE_MIN, currentFontSize() - FONT_SIZE_STEP));
});

// One shared font size across both modes -- same CSS variable, so these
// just reuse the file-ops buttons' handlers.
document.getElementById("chatFontIncBtn").addEventListener("click", () => {
  applyFontSize(Math.min(FONT_SIZE_MAX, currentFontSize() + FONT_SIZE_STEP));
});
document.getElementById("chatFontDecBtn").addEventListener("click", () => {
  applyFontSize(Math.max(FONT_SIZE_MIN, currentFontSize() - FONT_SIZE_STEP));
});

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

// Chat works with no folder open, so config is always needed. Also picks up
// a CLI-preloaded folder without requiring the picker.
window.addEventListener("DOMContentLoaded", async () => {
  if (!(await ensureAuthenticated())) return; // login overlay is up; it reloads on success
  try {
    currentConfig = await invoke("load_config");
  } catch (e) {
    console.error("initial load_config failed:", e);
  }
  updateNarrationToggleBtn();
  // Chat is the default view now (HTML already shows it unhidden at parse
  // time, before this even runs) -- this just triggers the same lazy
  // persona/session-list load `switchMode("chat")` does on a rail click,
  // since nothing else would otherwise fire it for whoever never clicks
  // the rail at all.
  switchMode("chat");
  // File-ops mode only -- there's no root/folder concept in `--server`, and
  // `get_current_root` was never ported there at all, so calling it
  // outside Tauri would only ever log a confusing "no such command" error.
  if (window.__TAURI__) {
    try {
      const root = await invoke("get_current_root");
      if (root) {
        console.log("startup: root already active:", root);
        rootPath.textContent = root;
        appendBubble("system", `Working in: ${root}`);
      }
    } catch (e) {
      console.error("startup root check failed:", e);
    }
  }
  try {
    const [version, buildHash] = await Promise.all([
      invoke("app_version"),
      invoke("app_build_hash"),
    ]);
    // The build hash is content-addressed (see `build.rs`'s `build_hash()`
    // doc comment), not tied to the version number -- two builds can share
    // a version while running genuinely different code, which is exactly
    // the "did my rebuild actually take effect" confusion this exists to
    // settle at a glance, without checking a binary's mtime by hand. Lives
    // in Settings (one gear click away), not the mode rail -- see
    // `.app-version`'s doc comment in style.css for why.
    const el = document.getElementById("appVersion");
    el.textContent = `v${version} (${buildHash})`;
    el.title = `LLM Assistant v${version}\nbuild ${buildHash}`;
    // The rail itself, in both the desktop app and the browser, only ever
    // shows the bare version -- the build hash is what actually made this
    // text wide enough to stretch the whole rail, not the short "vX.Y.Z"
    // on its own.
    const railEl = document.getElementById("railVersion");
    railEl.textContent = `v${version}`;
    railEl.title = el.title;
    railEl.hidden = false;
  } catch (e) {
    console.error("app_version/app_build_hash failed:", e);
  }
});

const pickBtn = document.getElementById("pickBtn");
const pickBtnDefaultLabel = pickBtn.textContent;

pickBtn.addEventListener("click", async () => {
  pickBtn.disabled = true;
  pickBtn.textContent = "Opening picker…";
  console.log("pick_and_set_root: invoking");
  try {
    const result = await invoke("pick_and_set_root");
    console.log("pick_and_set_root: got", result.root);
    rootPath.textContent = result.root;
    currentConfig = result.config;
    history = [];
    // Approvals don't carry across folders.
    resetApprovalFade();
    chatLog.innerHTML = "";
    appendBubble("system", `Working in: ${result.root}`);
  } catch (e) {
    console.error("pick_and_set_root failed:", e);
    appendBubble("system", `Error: ${e}`);
  } finally {
    pickBtn.disabled = false;
    pickBtn.textContent = pickBtnDefaultLabel;
  }
});

document.getElementById("clearChatBtn").addEventListener("click", async () => {
  if (sendBtn.type === "button") {
    // Stop the in-flight turn first, or a stale reply lands in the cleared chat.
    stopRequested = true;
    try {
      await invoke("stop_generation");
    } catch (err) {
      console.error("stop_generation failed:", err);
    }
  }
  history = [];
  resetApprovalFade();
  chatLog.innerHTML = "";
  logToFile("[system] --- chat cleared ---");
  if (rootPath.textContent && rootPath.textContent !== "No folder selected") {
    appendBubble("system", `Working in: ${rootPath.textContent}`);
  }
});

document.getElementById("unmountBtn").addEventListener("click", async () => {
  try {
    const old = await invoke("unmount_root");
    rootPath.textContent = "No folder selected";
    if (old) {
      appendBubble("system", `Unmounted ${old} — chat-only mode now.`);
    }
  } catch (err) {
    console.error("unmount_root failed:", err);
  }
});

// Enter submits, Shift+Enter newlines. Ignored mid-turn so Enter can't queue.
chatInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    if (sendBtn.type !== "button") {
      chatForm.requestSubmit();
    }
  }
});

// Send becomes a real Stop: it aborts the request in Rust, and stopRequested
// also blocks the next chain step (for Stop pressed during execution).
let stopRequested = false;

function setProcessing(isProcessing) {
  sendBtn.textContent = isProcessing ? "Stop" : "Send";
  sendBtn.type = isProcessing ? "button" : "submit";
  sendBtn.classList.toggle("stop-btn", isProcessing);
  sendBtn.disabled = false;
}

sendBtn.addEventListener("click", async () => {
  if (sendBtn.type !== "button") return; // acts as Stop only while processing
  stopRequested = true;
  try {
    await invoke("stop_generation");
  } catch (err) {
    console.error("stop_generation failed:", err);
  }
});

chatForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const text = chatInput.value.trim();
  if (!text) return;
  chatInput.value = "";
  appendBubble("user", text);
  history.push({ role: "user", content: text });
  stopRequested = false;
  // Rust re-reads config and rules every turn already; currentConfig is only
  // a JS display cache, so refresh it here or client-side settings go stale.
  try {
    currentConfig = await invoke("load_config");
  } catch (err) {
    console.error("load_config (pre-turn refresh) failed:", err);
  }
  // The task boundary (see memory.rs): driven by the user speaking, not by
  // the model saying it's done.
  try {
    await invoke("start_memory_task", { message: text });
  } catch (err) {
    console.error("start_memory_task failed:", err);
  }
  const maxSteps = currentConfig?.max_auto_steps ?? DEFAULT_MAX_AUTO_STEPS;
  const thinking = createThinkingTracker();
  await runAssistantTurn(0, maxSteps, thinking);
  thinking.finish(stopRequested ? "Stopped" : "Completed");
});

// One turn = one send_message. After a command runs we take another so the
// model reacts to the result rather than stopping at raw output; maxSteps
// (0 = no limit) caps the chain. Turns that propose another command go into
// the `thinking` disclosure; the one that answers in text does not.
const DEFAULT_MAX_AUTO_STEPS = 12;

// Must stay identical to rules::STUCK_LOOP_REPEAT_THRESHOLD (headless.rs).
// See createThinkingTracker's isStuckInCycle -- lastExecutedCommand alone
// only catches an *immediate* repeat, not an alternating loop.
const STUCK_LOOP_REPEAT_THRESHOLD = 4;

async function runAssistantTurn(depth, maxSteps, thinking) {
  if (stopRequested) {
    appendBubble("system", "Stopped.");
    // Or the model assumes its last action either completed or never happened.
    history.push({
      role: "user",
      content:
        "[the user stopped this before it finished -- if the last action wasn't confirmed as done, don't assume it completed; check first if it matters]",
    });
    return;
  }
  if (maxSteps > 0 && depth > maxSteps) {
    appendBubble("system", "Stopping after several automatic steps — send another message to continue.");
    history.push({
      role: "user",
      content:
        "[automatic continuation paused after several steps -- this is just a safety cap, not a deliberate stop; continue naturally from here]",
    });
    return;
  }
  setProcessing(true);
  try {
    const { reply, dropped, condensed, summary, summarized, rewritten_history } = await invoke(
      "send_message",
      { history },
    );
    // Before pushing the reply, or it lands on the array we're discarding.
    if (rewritten_history) {
      history = rewritten_history;
    }
    history.push({ role: "assistant", content: reply });
    const { command, displayText, extraCommands } = parseAssistantReply(reply);

    if (summary) {
      appendSummary(summary, summarized);
    }

    // Trimming is never silent. Condensing is reported more quietly: only
    // narration was shed, the commands and output are still there.
    if (condensed > 0) {
      appendBubble(
        "system",
        `Conversation got long — condensed ${condensed} finished step${condensed === 1 ? "" : "s"} down to just the command and its output.`,
      );
    }
    if (dropped > 0) {
      appendBubble(
        "system",
        `Conversation got long — dropped the ${dropped} oldest message${dropped === 1 ? "" : "s"} to stay within the context window.`,
      );
    }

    if (extraCommands > 0) {
      // Or the model reports the unrun ones as done next turn (it has).
      appendBubble(
        "system",
        `That reply included ${extraCommands + 1} commands; only the first one ran.`,
        command ? thinking.ensure() : chatLog,
      );
      history.push({
        role: "user",
        content: `[you included ${extraCommands + 1} commands in fenced blocks in that reply -- only the first one ran, since only one command runs per reply. Don't assume the others happened; if they're still needed, propose the next one now that you have the first one's result.]`,
      });
    }

    const isImmediateRepeat = command && thinking.isImmediateRepeat(command);
    const isStuckInCycle = command && !isImmediateRepeat && thinking.isStuckInCycle(command);
    if (isImmediateRepeat || isStuckInCycle) {
      // Fires when the work is done and it's re-running a listing to show
      // output it has (immediate case), or when a small local model has
      // fallen into an alternating loop that never repeats back to back
      // (cycle case). Bailing out here left a dangling half-sentence in a
      // collapsed box, so refuse the command and take one closing turn.
      appendBubble("assistant", displayText, thinking.ensure());
      appendBubble(
        "system",
        isImmediateRepeat
          ? "Skipped a repeat of that command — it was just run with nothing in between. Wrapping up instead."
          : `That command has come up ${STUCK_LOOP_REPEAT_THRESHOLD} times without moving anything forward. Wrapping up instead.`,
        thinking.ensure(),
      );
      // Must stay identical to rules::REPEATED_COMMAND_NOTE /
      // rules::STUCK_LOOP_NOTE (headless.rs).
      history.push({
        role: "user",
        content: isImmediateRepeat
          ? "[you proposed the exact same command again immediately after it already ran, with nothing new to justify re-running it -- it was not run again. You already have its output above.]"
          : "[that exact command has come up too many times in this conversation without moving anything forward -- it was not run again. You already have its output from earlier -- work from that, or try something genuinely different.]",
      });
      await finalAnswerTurn();
      return;
    }

    if (command && needsElevatedPrivileges(command)) {
      // A final turn, not a step: shown outside `thinking`. Does NOT
      // auto-continue -- letting it "try again" is what caused a loop.
      appendBubble("assistant", displayText, chatLog);
      appendManualCommand(command);
      recordBlocked(command, "needs sudo/root, which this sandbox can never grant");
      history.push({
        role: "user",
        content:
          "[the proposed command needs sudo/root, which this sandbox can never grant -- stop proposing it; wait for the user to run it manually and reply with the output]",
      });
      return;
    }

    appendBubble("assistant", displayText, command ? thinking.ensure() : chatLog);
    if (command) {
      await handleProposedCommand(command, displayText, depth, maxSteps, thinking);
    }
  } catch (err) {
    appendBubble("system", stopRequested ? "Stopped." : `Error: ${err}`);
    // The request died before a reply, so nothing in `history` shows it.
    history.push({
      role: "user",
      content: stopRequested
        ? "[the user stopped this before it finished -- if the last action wasn't confirmed as done, don't assume it completed; check first if it matters]"
        : `[that request failed before completing: ${err}. Don't assume anything after this point happened.]`,
    });
  } finally {
    setProcessing(false);
  }
}

// One turn with commands off the table, after a hard stop where the work is
// done. Shown outside `thinking` (the point), and any command is stripped.
async function finalAnswerTurn() {
  if (stopRequested) return;
  // Presupposing success got a fabricated reorganization after two denied
  // commands; the closing clause is the same problem after trimming.
  // Must stay identical to rules::FINAL_ANSWER_PROMPT.
  history.push({
    role: "user",
    content:
      "[don't run anything else. Reply now in plain text, with no command and no code fence, describing the CURRENT state strictly from the command output you actually received above. If commands were denied, never ran, or failed, say that plainly -- do not describe any file as moved, created, or deleted unless output above shows it actually happened. If part of this conversation was summarized or dropped to save context, don't reconstruct what was in it: say you no longer have it rather than describing file contents you cannot see.]",
  });
  try {
    const { reply, summary, summarized, rewritten_history } = await invoke("send_message", {
      history,
    });
    if (rewritten_history) {
      history = rewritten_history;
    }
    history.push({ role: "assistant", content: reply });
    if (summary) {
      appendSummary(summary, summarized);
    }
    const { command, displayText } = parseAssistantReply(reply);
    const text =
      command && displayText === COMMAND_ONLY_PLACEHOLDER
        ? "Done — nothing further to run."
        : displayText;
    appendBubble("assistant", text, chatLog);
  } catch (err) {
    appendBubble("system", stopRequested ? "Stopped." : `Error: ${err}`);
  }
}

// --- Confirmation fade-out ---
//
// A dialog approved every time stops being read, which is worse than one that
// appears rarely. Weaker than "always allow" on purpose: nothing is written to
// config, it dies with the session, and one denial reverts it. The permission
// check stays in Rust, so this can fade a program but never a pipe.
const DEFAULT_CONFIRM_FADE_AFTER = 3;

let approvalCounts = new Map();
let sessionApproved = new Set();

function resetApprovalFade() {
  approvalCounts = new Map();
  sessionApproved = new Set();
}

function commandBinary(cmd) {
  const first = cmd.trim().split(/\s+/)[0];
  return first ? first.split("/").pop() : "";
}

// Counts the binary, not the command: `mv a b` and `mv c d` are one decision.
function recordApproval(cmd) {
  const bin = commandBinary(cmd);
  if (!bin || sessionApproved.has(bin)) return;
  const fadeAfter = currentConfig?.confirm_fade_after ?? DEFAULT_CONFIRM_FADE_AFTER;
  if (fadeAfter <= 0) return;

  const count = (approvalCounts.get(bin) ?? 0) + 1;
  approvalCounts.set(bin, count);
  if (count < fadeAfter) return;

  sessionApproved.add(bin);
  appendFadeNotice(bin, count);
}

// A denial resets progress and revokes an existing fade.
function recordDenial(cmd) {
  const bin = commandBinary(cmd);
  if (!bin) return;
  approvalCounts.delete(bin);
  if (sessionApproved.delete(bin)) {
    appendBubble("system", `Will ask before running ${bin} again.`);
  }
}

function appendFadeNotice(bin, count) {
  const div = document.createElement("div");
  div.className = "bubble system";
  const label = document.createElement("span");
  label.textContent = `Approved ${bin} ${count} times — running it without asking for the rest of this session. `;
  const undo = document.createElement("button");
  undo.type = "button";
  undo.className = "link-btn";
  undo.textContent = "Keep asking";
  undo.addEventListener("click", () => {
    sessionApproved.delete(bin);
    approvalCounts.delete(bin);
    undo.disabled = true;
    label.textContent = `Will keep asking before running ${bin}. `;
  });
  div.appendChild(label);
  div.appendChild(undo);
  chatLog.appendChild(div);
  chatLog.scrollTop = chatLog.scrollHeight;
  logToFile(`[system] ${label.textContent}`);
}

async function handleProposedCommand(cmd, explanation, depth, maxSteps, thinking) {
  const info = await invoke("classify_command", {
    cmd,
    sessionApproved: [...sessionApproved],
  });
  if (info.classification === "ReadOnly" || info.auto_approved) {
    await executeCommand(cmd, depth, maxSteps, thinking);
    return;
  }
  await requestApproval(cmd, explanation, depth, maxSteps, thinking);
}

// Splits on top-level `&&`/`;`/newline, quote-aware. Returns null if a pipe,
// redirect, substitution or backgrounding is present, since splitting would
// change what the command does.
//
// Newlines matter: as one `sh -c` block, `sh -c` reports the *last* line's
// exit code, masking an earlier failure -- that caused a real multi-turn
// confusion. As a checklist, executeSequence stops at the first failure.
function splitCommandSequence(cmd) {
  const NON_SPLITTABLE = ["|", ">", "<", "`", "$("];
  // `for x in *; do` isn't a complete command on its own.
  const CONTROL_FLOW_KEYWORDS = new Set([
    "for",
    "while",
    "until",
    "if",
    "then",
    "else",
    "elif",
    "fi",
    "do",
    "done",
    "case",
    "esac",
    "function",
  ]);
  let inSingle = false;
  let inDouble = false;
  let current = "";
  const parts = [];
  for (let i = 0; i < cmd.length; i++) {
    const ch = cmd[i];
    if (ch === "'" && !inDouble) {
      inSingle = !inSingle;
      current += ch;
      continue;
    }
    if (ch === '"' && !inSingle) {
      inDouble = !inDouble;
      current += ch;
      continue;
    }
    if (!inSingle && !inDouble) {
      if (cmd.startsWith("&&", i)) {
        parts.push(current);
        current = "";
        i++;
        continue;
      }
      if (ch === ";" || ch === "\n") {
        parts.push(current);
        current = "";
        continue;
      }
      if (NON_SPLITTABLE.some((op) => cmd.startsWith(op, i)) || ch === "&") {
        return null;
      }
    }
    current += ch;
  }
  if (inSingle || inDouble) return null; // unbalanced quotes -- don't trust our own parse
  parts.push(current);
  const steps = parts.map((p) => p.trim()).filter(Boolean);
  if (steps.length <= 1) return null;
  for (const step of steps) {
    const firstWord = step.split(/\s+/)[0];
    if (CONTROL_FLOW_KEYWORDS.has(firstWord) || step.startsWith("#")) {
      return null;
    }
  }
  return steps;
}

function requestApproval(cmd, explanation, depth, maxSteps, thinking) {
  return new Promise((resolve) => {
    const dialog = document.getElementById("confirmDialog");
    const denyBtn = document.getElementById("denyBtn");
    const approveBtn = document.getElementById("approveBtn");
    const alwaysAllow = document.getElementById("alwaysAllow");
    const alwaysAllowRow = document.getElementById("alwaysAllowRow");
    const singleView = document.getElementById("confirmCmd");
    const stepsView = document.getElementById("confirmSteps");

    document.getElementById("confirmContext").textContent = explanation;

    // A checklist so steps can be approved individually; "always allow"
    // doesn't generalize to a chain, so it's hidden there.
    const steps = splitCommandSequence(cmd);
    stepsView.innerHTML = "";
    if (steps) {
      singleView.hidden = true;
      stepsView.hidden = false;
      alwaysAllowRow.hidden = true;
      approveBtn.textContent = "Run checked";
      for (const step of steps) {
        const li = document.createElement("li");
        const label = document.createElement("label");
        const checkbox = document.createElement("input");
        checkbox.type = "checkbox";
        checkbox.checked = true;
        checkbox.className = "confirm-step-checkbox";
        const code = document.createElement("code");
        code.textContent = step;
        label.appendChild(checkbox);
        label.appendChild(code);
        li.appendChild(label);
        stepsView.appendChild(li);
      }
    } else {
      singleView.hidden = false;
      singleView.textContent = cmd;
      stepsView.hidden = true;
      alwaysAllowRow.hidden = false;
      approveBtn.textContent = "Run it";
    }
    alwaysAllow.checked = false;
    focusMainWindow();
    dialog.hidden = false;

    const cleanup = () => {
      denyBtn.onclick = null;
      approveBtn.onclick = null;
      dialog.hidden = true;
    };

    denyBtn.onclick = async () => {
      cleanup();
      recordDenial(cmd);
      recordBlocked(cmd, "the user denied it");
      // Ends the chain, like the sudo case: auto-continuing let the model
      // flail and eventually report a task that never ran as finished.
      appendBubble("system", "Command denied — stopped. Nothing was run; send a message to continue.");
      history.push({
        role: "user",
        content:
          "[the user DENIED that command -- it did NOT run and nothing was changed by it. Do not claim or assume any part of it happened, and do not propose it again unless the user asks. Wait for the user.]",
      });
      resolve();
    };

    approveBtn.onclick = async () => {
      cleanup();
      if (steps) {
        const checked = [...stepsView.querySelectorAll(".confirm-step-checkbox")]
          .map((cb, i) => (cb.checked ? steps[i] : null))
          .filter(Boolean);
        // Each checked step is its own decision; unchecked ones count as neither.
        for (const step of checked) {
          recordApproval(step);
        }
        await executeSequence(checked, steps.length, depth, maxSteps, thinking);
      } else {
        if (alwaysAllow.checked) {
          const bin = commandBinary(cmd);
          currentConfig = await invoke("add_auto_approve", { binary: bin });
        }
        recordApproval(cmd);
        await executeCommand(cmd, depth, maxSteps, thinking);
      }
      resolve();
    };
  });
}

// Successful runs are recorded by `run_command` in Rust; these have no exit
// code, so they come from here.
function recordBlocked(cmd, why) {
  invoke("record_blocked_command", { cmd, why }).catch((err) =>
    console.error("record_blocked_command failed:", err),
  );
}

// A bare "not found" isn't a strong enough signal; the rule lands better at
// the point of failure than once at the top of the prompt.
const NOT_FOUND_PATTERN = /no such file or directory|cannot access/i;

// Must stay byte-identical to COMMAND_OUTPUT_PREFIX in src/context.rs --
// condensing recognizes a finished step by this prefix.
const COMMAND_OUTPUT_PREFIX = "[command output, exit ";

function formatCommandFeedback(cmd, outcome, { withCommand = false } = {}) {
  const summary = [outcome.stdout, outcome.stderr].filter(Boolean).join("\n").trim();
  const header = withCommand
    ? `${COMMAND_OUTPUT_PREFIX}${outcome.exit_code}] $ ${cmd}`
    : `${COMMAND_OUTPUT_PREFIX}${outcome.exit_code}]`;
  let feedback = `${header}\n${summary || "(no output)"}`;
  if (
    outcome.exit_code !== 0 &&
    NOT_FOUND_PATTERN.test(summary) &&
    currentConfig?.granted_paths?.length > 0
  ) {
    feedback +=
      "\n\n(hint: if you were trying to reach a granted path, use its full absolute path -- " +
      "your current directory is always the working folder, never a granted path.)";
  }
  return feedback;
}

async function executeCommand(cmd, depth, maxSteps, thinking) {
  try {
    const outcome = await invoke("run_command", { cmd });
    thinking.recordExecuted(cmd);
    appendOutput(cmd, outcome, thinking.ensure());
    history.push({ role: "user", content: formatCommandFeedback(cmd, outcome) });
    await runAssistantTurn(depth + 1, maxSteps, thinking);
  } catch (err) {
    appendBubble("system", `Execution error: ${err}`, thinking.ensure());
  }
}

// Stops at the first failure (like `&&`); one turn covers the whole batch.
async function executeSequence(checked, totalSteps, depth, maxSteps, thinking) {
  if (checked.length === 0) {
    appendBubble("system", "No steps were checked -- nothing was run.", thinking.ensure());
    history.push({ role: "user", content: "[the user deselected every step; nothing was run]" });
    await runAssistantTurn(depth + 1, maxSteps, thinking);
    return;
  }
  if (checked.length < totalSteps) {
    appendBubble(
      "system",
      `Running ${checked.length} of ${totalSteps} steps (the rest were left unchecked).`,
      thinking.ensure(),
    );
  }

  const feedbacks = [];
  let stoppedEarly = false;
  try {
    for (const step of checked) {
      const outcome = await invoke("run_command", { cmd: step });
      thinking.recordExecuted(step);
      appendOutput(step, outcome, thinking.ensure());
      feedbacks.push(formatCommandFeedback(step, outcome, { withCommand: true }));
      if (outcome.exit_code !== 0) {
        stoppedEarly = true;
        break;
      }
    }
  } catch (err) {
    appendBubble("system", `Execution error: ${err}`, thinking.ensure());
    feedbacks.push(`[execution error]\n${err}`);
    stoppedEarly = true;
  }

  if (stoppedEarly) {
    appendBubble("system", "Stopped the sequence after a failed step.", thinking.ensure());
    // The steps after the failure never ran.
    for (const step of checked.slice(feedbacks.length)) {
      recordBlocked(step, "an earlier step in the same sequence failed");
    }
  }
  history.push({ role: "user", content: feedbacks.join("\n\n") });
  await runAssistantTurn(depth + 1, maxSteps, thinking);
}

// --- Settings ---

const settingsDialog = document.getElementById("settingsDialog");
let currentGeneralRules = "";
let currentCommandRules = "";
let currentComfyConfig = null;
let currentSearxngConfig = null;

// Shared by the top-level General/Chat/File Operations tabs and the nested
// Rules/Commands sub-tabs inside File Operations -- distinct classes so a
// click on one level never touches `.hidden`/`.active` state on the other.
function wireTabs(btnSelector, panelSelector, btnAttr, panelAttr) {
  const btns = document.querySelectorAll(btnSelector);
  for (const tabBtn of btns) {
    tabBtn.addEventListener("click", () => {
      for (const btn of btns) {
        btn.classList.toggle("active", btn === tabBtn);
      }
      for (const panel of document.querySelectorAll(panelSelector)) {
        panel.hidden = panel.dataset[panelAttr] !== tabBtn.dataset[btnAttr];
      }
    });
  }
}
wireTabs(".tab-btn", ".tab-panel", "tab", "panel");
wireTabs(".subtab-btn", ".subtab-panel", "subtab", "subpanel");

// Shared by both modes' gear icon -- Settings isn't file-ops-specific (chat
// mode has its own settings in the General tab too), so it opens the same
// way from either topbar.
async function openSettingsDialog() {
  try {
    [currentConfig, currentComfyConfig, currentSearxngConfig] = await Promise.all([
      invoke("load_config"),
      invoke("get_comfyui_config"),
      invoke("get_searxng_config"),
    ]);
    // General/command rules back file-ops mode's own tab, which is hidden
    // outside Tauri (see where `fileops` tab gets `hidden` set) -- these two
    // commands were never ported to `--server` at all (no folder/sandbox to
    // have rules about), so calling them here unconditionally is exactly
    // what produced "Method Not Allowed" the one time this ran outside Tauri.
    if (window.__TAURI__) {
      [currentGeneralRules, currentCommandRules] = await Promise.all([
        invoke("load_general_rules"),
        invoke("load_command_rules"),
      ]);
    }
  } catch (err) {
    console.error("load_config/load_rules failed:", err);
    alert(`Error loading settings: ${err}`);
    return;
  }
  renderSettings();
  focusMainWindow();
  settingsDialog.hidden = false;
}
document.getElementById("settingsBtn").addEventListener("click", openSettingsDialog);

// --- ComfyUI field mapping ---

const COMFY_MAPPING_FIELDS = [
  ["cfgComfyMapCheckpoint", "checkpoint"],
  ["cfgComfyMapPositive", "positive"],
  ["cfgComfyMapNegative", "negative"],
  ["cfgComfyMapWidth", "width"],
  ["cfgComfyMapHeight", "height"],
  ["cfgComfyMapSampler", "sampler"],
  ["cfgComfyMapScheduler", "scheduler"],
  ["cfgComfyMapCfg", "cfg"],
  ["cfgComfyMapSteps", "steps"],
];

// Parses the pasted ComfyUI workflow JSON and rebuilds each of the 9 mapping
// <select>s from its actual nodes -- node ids are specific to one workflow
// export, so the user picks from what's really in *this* JSON rather than
// typing one. Only scalar input values (string/number/boolean) are offered;
// an array value like `["58", 0]` is a node-graph wire, not a user-settable
// field. `applyMapping`, when given (only on dialog open, from the saved
// config), sets each select to that saved path; otherwise each select keeps
// whatever it was already showing, if that path still exists in the
// reparsed workflow -- lets the workflow textarea reparse live as the user
// types/pastes without losing selections that are still valid.
function rebuildComfyMappingOptions(applyMapping) {
  const options = [];
  try {
    const workflow = JSON.parse(document.getElementById("cfgComfyWorkflowJson").value);
    if (workflow && typeof workflow === "object") {
      for (const [nodeId, node] of Object.entries(workflow)) {
        const inputs = node?.inputs;
        if (!inputs || typeof inputs !== "object") continue;
        const label = node._meta?.title || node.class_type || nodeId;
        for (const [key, value] of Object.entries(inputs)) {
          if (value === null || typeof value === "object") continue; // array wire or object
          options.push({ path: `${nodeId}.${key}`, text: `${nodeId} · ${label} · ${key}` });
        }
      }
    }
  } catch {
    // Invalid/incomplete JSON while typing -- just offer no options yet.
  }

  for (const [id, field] of COMFY_MAPPING_FIELDS) {
    const select = document.getElementById(id);
    const previousValue = applyMapping ? applyMapping[field] || "" : select.value;
    select.innerHTML = "";
    const blank = document.createElement("option");
    blank.value = "";
    blank.textContent = "(not mapped)";
    select.appendChild(blank);
    for (const opt of options) {
      const o = document.createElement("option");
      o.value = opt.path;
      o.textContent = opt.text;
      select.appendChild(o);
    }
    if (options.some((o) => o.path === previousValue)) {
      select.value = previousValue;
    }
  }
}

document
  .getElementById("cfgComfyWorkflowJson")
  .addEventListener("input", () => rebuildComfyMappingOptions());

document.getElementById("cfgComfyOutputDirBrowseBtn").addEventListener("click", async () => {
  try {
    const path = await invoke("pick_comfyui_output_dir");
    if (path) document.getElementById("cfgComfyOutputDir").value = path;
  } catch (err) {
    console.error("pick_comfyui_output_dir failed:", err);
  }
});

document.getElementById("testComfyGenerationBtn").addEventListener("click", async () => {
  const btn = document.getElementById("testComfyGenerationBtn");
  const result = document.getElementById("testComfyGenerationResult");
  const img = document.getElementById("testComfyGenerationImg");
  btn.disabled = true;
  result.classList.remove("warn");
  result.textContent = "Generating… this can take a while.";
  img.hidden = true;
  try {
    const cfg = collectComfyConfigFromForm();
    const dataUrl = await invoke("test_comfyui_generation", { cfg });
    result.textContent = "Generated successfully:";
    img.src = dataUrl;
    img.hidden = false;
  } catch (err) {
    result.textContent = `Failed: ${err}`;
    result.classList.add("warn");
  } finally {
    btn.disabled = false;
  }
});

document.getElementById("testSearxngSearchBtn").addEventListener("click", async () => {
  const btn = document.getElementById("testSearxngSearchBtn");
  const result = document.getElementById("testSearxngSearchResult");
  const results = document.getElementById("testSearxngSearchResults");
  btn.disabled = true;
  result.classList.remove("warn");
  result.textContent = "Searching…";
  results.hidden = true;
  try {
    const cfg = collectSearxngConfigFromForm();
    const found = await invoke("test_searxng_search", { cfg });
    result.textContent = `${found.length} result(s):`;
    results.textContent = found.map((r) => `${r.title}\n${r.url}\n${r.content}`).join("\n\n");
    results.hidden = false;
  } catch (err) {
    result.textContent = `Failed: ${err}`;
    result.classList.add("warn");
  } finally {
    btn.disabled = false;
  }
});

// Reads the ComfyUI tab's current form state into a config object -- shared
// by the "Test image generation" button (tests what's typed, not what's
// saved, same contract as testConnectionBtn) and the main Settings save
// handler.
function collectComfyConfigFromForm() {
  return {
    base_url: document.getElementById("cfgComfyBaseUrl").value.trim(),
    workflow_json: document.getElementById("cfgComfyWorkflowJson").value,
    mapping: Object.fromEntries(
      COMFY_MAPPING_FIELDS.map(([id, field]) => [field, document.getElementById(id).value]),
    ),
    output_dir: document.getElementById("cfgComfyOutputDir").value.trim(),
    filename_pattern: document.getElementById("cfgComfyFilenamePattern").value.trim() || "{session}-{timestamp}",
    reaction_mode: document.getElementById("cfgComfyReactionMode").value,
  };
}

function collectSearxngConfigFromForm() {
  return {
    base_url: document.getElementById("cfgSearxngBaseUrl").value.trim(),
    api_key: document.getElementById("cfgSearxngApiKey").value.trim(),
    max_results: Math.max(1, Number(document.getElementById("cfgSearxngMaxResults").value) || 5),
  };
}

function renderSettings() {
  // A stale "Connected." says nothing about the endpoint now in the box.
  const testResult = document.getElementById("testConnectionResult");
  testResult.textContent = "";
  testResult.classList.remove("warn");

  document.getElementById("cfgEndpoint").value = currentConfig.endpoint;
  document.getElementById("cfgModel").value = currentConfig.model;
  document.getElementById("cfgApiKey").value = currentConfig.api_key || "";
  document.getElementById("cfgTemperature").value = currentConfig.temperature;
  document.getElementById("cfgChatTemperature").value = currentConfig.chat_temperature;
  document.getElementById("cfgMaxAutoSteps").value = currentConfig.max_auto_steps;
  document.getElementById("cfgMaxContextTokens").value = currentConfig.max_context_tokens;
  document.getElementById("cfgConfirmFadeAfter").value = currentConfig.confirm_fade_after;
  document.getElementById("cfgMemoryEnabled").checked = !!currentConfig.memory_enabled;
  document.getElementById("cfgMemoryMaxTokens").value = currentConfig.memory_max_tokens;
  document.getElementById("cfgChatStateMaxTokens").value = currentConfig.chat_state_max_tokens;
  document.getElementById("cfgChatShowThinking").checked = !!currentConfig.chat_show_thinking;
  document.getElementById("cfgChatPersistThinking").checked = !!currentConfig.chat_persist_thinking;
  document.getElementById("cfgChatStreamReplies").checked = !!currentConfig.chat_stream_replies;
  document.getElementById("cfgSystemPrompt").value = currentConfig.system_prompt;
  document.getElementById("cfgDisableBuiltinRules").checked = !!currentConfig.disable_builtin_rules;
  document.getElementById("cfgSummarizeBeforeDropping").checked =
    !!currentConfig.summarize_before_dropping;
  document.getElementById("cfgServerSessionExpiryDays").value =
    currentConfig.server_session_expiry_days;
  document.getElementById("cfgGeneralRules").value = currentGeneralRules;
  document.getElementById("cfgCommandRules").value = currentCommandRules;

  document.getElementById("cfgComfyBaseUrl").value = currentComfyConfig.base_url;
  document.getElementById("cfgComfyWorkflowJson").value = currentComfyConfig.workflow_json;
  document.getElementById("cfgComfyOutputDir").value = currentComfyConfig.output_dir;
  document.getElementById("cfgComfyFilenamePattern").value = currentComfyConfig.filename_pattern;
  document.getElementById("cfgComfyReactionMode").value = currentComfyConfig.reaction_mode || "always";
  rebuildComfyMappingOptions(currentComfyConfig.mapping);
  document.getElementById("testComfyGenerationResult").textContent = "";
  document.getElementById("testComfyGenerationResult").classList.remove("warn");
  document.getElementById("testComfyGenerationImg").hidden = true;

  document.getElementById("cfgSearxngBaseUrl").value = currentSearxngConfig.base_url;
  document.getElementById("cfgSearxngApiKey").value = currentSearxngConfig.api_key;
  document.getElementById("cfgSearxngMaxResults").value = currentSearxngConfig.max_results;
  document.getElementById("testSearxngSearchResult").textContent = "";
  document.getElementById("testSearxngSearchResult").classList.remove("warn");
  document.getElementById("testSearxngSearchResults").hidden = true;

  const list = document.getElementById("grantedList");
  list.innerHTML = "";
  if (currentConfig.granted_paths.length === 0) {
    const li = document.createElement("li");
    li.className = "granted-empty";
    li.textContent = "No additional folders granted yet.";
    list.appendChild(li);
  }
  for (const g of currentConfig.granted_paths) {
    const li = document.createElement("li");
    const info = document.createElement("div");
    info.className = "granted-info";
    const scope = g.recursive ? "recursive" : "top-level only";
    const pathLine = document.createElement("div");
    pathLine.className = "granted-path-line";
    pathLine.textContent = `${g.path} (${g.read_write ? "rw" : "ro"}, ${scope})`;
    info.appendChild(pathLine);
    if (g.note) {
      const noteLine = document.createElement("div");
      noteLine.className = "granted-note-line";
      noteLine.textContent = g.note;
      info.appendChild(noteLine);
    }
    const rm = document.createElement("button");
    rm.type = "button";
    rm.textContent = "remove";
    rm.onclick = async () => {
      currentConfig = await invoke("remove_granted_path", { path: g.path });
      renderSettings();
    };
    li.appendChild(info);
    li.appendChild(rm);
    list.appendChild(li);
  }

  const autoApproveList = document.getElementById("autoApproveList");
  autoApproveList.innerHTML = "";
  if (currentConfig.auto_approve.length === 0) {
    const li = document.createElement("li");
    li.className = "granted-empty";
    li.textContent = "(none yet — approve a command with the checkbox to add one)";
    autoApproveList.appendChild(li);
  }
  for (const bin of currentConfig.auto_approve) {
    const li = document.createElement("li");
    const label = document.createElement("span");
    label.textContent = bin;
    const rm = document.createElement("button");
    rm.type = "button";
    rm.textContent = "remove";
    rm.onclick = async () => {
      currentConfig = await invoke("remove_auto_approve", { binary: bin });
      renderSettings();
    };
    li.appendChild(label);
    li.appendChild(rm);
    autoApproveList.appendChild(li);
  }
}

// --- Add-granted-path modal ---
// The "what's it for" note reaches the model (via build_root_note), so it can
// use a granted folder proactively rather than only on an explicit path.

const addPathDialog = document.getElementById("addPathDialog");

// Granting a path sends its contents to whatever endpoint is configured --
// a data-egress decision made on a different Settings tab, so warn here and
// name the endpoint.
const LOCAL_ENDPOINT_HOSTS = ["localhost", "127.0.0.1", "[::1]", "::1", "0.0.0.0"];

function endpointIsLocal(endpoint) {
  try {
    return LOCAL_ENDPOINT_HOSTS.includes(new URL(endpoint).hostname);
  } catch {
    // Warn rather than reassure: quiet is the expensive direction to be wrong.
    return false;
  }
}

function renderGrantEndpointWarning() {
  const el = document.getElementById("addPathEndpointWarning");
  const endpoint = currentConfig?.endpoint ?? "";
  let host;
  try {
    host = new URL(endpoint).host;
  } catch {
    host = endpoint || "(none configured)";
  }
  if (endpointIsLocal(endpoint)) {
    el.textContent = `Anything the assistant reads here is sent to ${host}, which is on this machine. Change the endpoint to a remote or cloud model and these files leave it.`;
    el.classList.remove("warn");
  } else {
    el.textContent = `⚠ Your endpoint is ${host}, which is not this machine. Anything the assistant reads from this folder will be sent there.`;
    el.classList.add("warn");
  }
}

document.getElementById("grantQuickAddBtn").addEventListener("click", () => {
  document.getElementById("addPathInput").value = "";
  document.getElementById("addPathContext").value = "";
  document.getElementById("addPathRecursive").checked = true;
  renderGrantEndpointWarning();
  focusMainWindow();
  addPathDialog.hidden = false;
});

document.getElementById("addPathBrowseBtn").addEventListener("click", async () => {
  try {
    const path = await invoke("pick_granted_path");
    if (path) {
      document.getElementById("addPathInput").value = path;
    }
  } catch (err) {
    console.error("pick_granted_path failed:", err);
  }
});

document.getElementById("addPathCancelBtn").addEventListener("click", () => {
  addPathDialog.hidden = true;
});

document.getElementById("addPathConfirmBtn").addEventListener("click", async () => {
  const path = document.getElementById("addPathInput").value.trim();
  const note = document.getElementById("addPathContext").value.trim();
  const recursive = document.getElementById("addPathRecursive").checked;
  if (!path) return;
  try {
    currentConfig = await invoke("add_granted_path", { path, note, readWrite: false, recursive });
    addPathDialog.hidden = true;
    renderSettings();
  } catch (err) {
    console.error("add_granted_path failed:", err);
  }
});

// Tests what's typed, not what's saved.
document.getElementById("testConnectionBtn").addEventListener("click", async () => {
  const btn = document.getElementById("testConnectionBtn");
  const result = document.getElementById("testConnectionResult");
  btn.disabled = true;
  result.classList.remove("warn");
  result.textContent = "Testing…";
  try {
    result.textContent = await invoke("test_connection", {
      endpoint: document.getElementById("cfgEndpoint").value.trim(),
      model: document.getElementById("cfgModel").value.trim(),
      apiKey: document.getElementById("cfgApiKey").value,
    });
  } catch (err) {
    // Raw, not summarized: a 404 and a 401 need different fixes.
    result.textContent = `Failed: ${err}`;
    result.classList.add("warn");
  } finally {
    btn.disabled = false;
  }
});

document.getElementById("resetPromptBtn").addEventListener("click", async () => {
  try {
    const defaultPrompt = await invoke("default_system_prompt");
    document.getElementById("cfgSystemPrompt").value = defaultPrompt;
  } catch (err) {
    console.error("default_system_prompt failed:", err);
  }
});

document.getElementById("resetGeneralRulesBtn").addEventListener("click", async () => {
  try {
    const defaultRules = await invoke("default_general_rules");
    document.getElementById("cfgGeneralRules").value = defaultRules;
  } catch (err) {
    console.error("default_general_rules failed:", err);
  }
});

document.getElementById("resetCommandRulesBtn").addEventListener("click", async () => {
  try {
    const defaultRules = await invoke("default_command_rules");
    document.getElementById("cfgCommandRules").value = defaultRules;
  } catch (err) {
    console.error("default_command_rules failed:", err);
  }
});

document.getElementById("settingsSaveBtn").addEventListener("click", async () => {
  currentConfig.endpoint = document.getElementById("cfgEndpoint").value;
  currentConfig.model = document.getElementById("cfgModel").value;
  currentConfig.api_key = document.getElementById("cfgApiKey").value;
  currentConfig.temperature = parseFloat(document.getElementById("cfgTemperature").value) || 0;
  currentConfig.chat_temperature =
    parseFloat(document.getElementById("cfgChatTemperature").value) || 0;
  currentConfig.max_auto_steps = Math.max(
    0,
    parseInt(document.getElementById("cfgMaxAutoSteps").value, 10) || 0,
  );
  currentConfig.max_context_tokens = Math.max(
    0,
    parseInt(document.getElementById("cfgMaxContextTokens").value, 10) || 0,
  );
  currentConfig.confirm_fade_after = Math.max(
    0,
    parseInt(document.getElementById("cfgConfirmFadeAfter").value, 10) || 0,
  );
  currentConfig.memory_enabled = document.getElementById("cfgMemoryEnabled").checked;
  currentConfig.memory_max_tokens = Math.max(
    0,
    parseInt(document.getElementById("cfgMemoryMaxTokens").value, 10) || 0,
  );
  currentConfig.chat_state_max_tokens = Math.max(
    0,
    parseInt(document.getElementById("cfgChatStateMaxTokens").value, 10) || 0,
  );
  currentConfig.chat_show_thinking = document.getElementById("cfgChatShowThinking").checked;
  currentConfig.chat_persist_thinking = document.getElementById("cfgChatPersistThinking").checked;
  currentConfig.chat_stream_replies = document.getElementById("cfgChatStreamReplies").checked;
  currentConfig.system_prompt = document.getElementById("cfgSystemPrompt").value;
  currentConfig.disable_builtin_rules = document.getElementById("cfgDisableBuiltinRules").checked;
  currentConfig.summarize_before_dropping = document.getElementById(
    "cfgSummarizeBeforeDropping",
  ).checked;
  currentConfig.server_session_expiry_days = Math.max(
    0,
    parseInt(document.getElementById("cfgServerSessionExpiryDays").value, 10) || 0,
  );
  currentGeneralRules = document.getElementById("cfgGeneralRules").value;
  currentCommandRules = document.getElementById("cfgCommandRules").value;
  currentComfyConfig = collectComfyConfigFromForm();
  currentSearxngConfig = collectSearxngConfigFromForm();
  const saves = [
    invoke("save_config", { cfg: currentConfig }),
    invoke("save_comfyui_config", { cfg: currentComfyConfig }),
    invoke("save_searxng_config", { cfg: currentSearxngConfig }),
  ];
  // Same reasoning as loading them in `openSettingsDialog` -- these two back
  // the hidden File Operations tab and were never ported to `--server`.
  if (window.__TAURI__) {
    saves.push(
      invoke("save_general_rules", { rules: currentGeneralRules }),
      invoke("save_command_rules", { rules: currentCommandRules }),
    );
  }
  await Promise.all(saves);
  updateNarrationToggleBtn();
  if (chatModeView && !chatModeView.hidden) renderChatHistoryLog();
  settingsDialog.hidden = true;
});

document.getElementById("settingsCloseBtn").addEventListener("click", () => {
  settingsDialog.hidden = true;
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
    Promise.all([loadPersonaList(), loadSessionList()])
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
      // An open session's persona content is read fresh on the next turn
      // (`chat_turn::run_chat_turn` loads it every call), so nothing here
      // needs to refresh the active chat -- the edit just takes effect.
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
  if (!confirm(`Delete persona "${name}"? This can't be undone.`)) return;
  try {
    await invoke("delete_persona", { name });
    await loadPersonaList();
  } catch (err) {
    alert(`Delete failed: ${err}`);
  }
});

// --- Rulesets ---
//
// Editing only, not full persona-style CRUD -- the two rulesets always
// exist (self-healing seeded by `ruleset::list_rulesets` on the Rust side),
// so there's no "new"/"import"/"delete" to offer here, just a way to change
// their content without opening the `.md` files by hand.

const rulesetDialog = document.getElementById("rulesetDialog");
const rulesetSelect = document.getElementById("rulesetSelect");
const rulesetContentInput = document.getElementById("rulesetContent");
const rulesetExampleRow = document.getElementById("rulesetExampleRow");
const rulesetExampleDialog = document.getElementById("rulesetExampleDialog");

// Cached per selected ruleset -- refetched on every selection change, not
// on every click, since the example button itself doesn't need a
// round-trip just to show/hide.
let currentRulesetExample = null;

async function loadRulesetContent(name) {
  try {
    rulesetContentInput.value = await invoke("get_ruleset_content", { name });
  } catch (err) {
    alert(`Could not load that ruleset: ${err}`);
  }
  try {
    currentRulesetExample = await invoke("get_ruleset_example", { name });
  } catch (err) {
    console.error("get_ruleset_example failed:", err);
    currentRulesetExample = null;
  }
  rulesetExampleRow.hidden = !currentRulesetExample;
}

document.getElementById("rulesetExampleBtn").addEventListener("click", () => {
  if (!currentRulesetExample) return;
  document.getElementById("rulesetExampleContent").textContent = currentRulesetExample;
  focusMainWindow();
  rulesetExampleDialog.hidden = false;
});
document.getElementById("rulesetExampleCloseBtn").addEventListener("click", () => {
  rulesetExampleDialog.hidden = true;
});

document.getElementById("editRulesetsBtn").addEventListener("click", async () => {
  try {
    const rulesets = await invoke("list_rulesets");
    rulesetSelect.innerHTML = "";
    for (const r of rulesets) {
      const opt = document.createElement("option");
      opt.value = r.name;
      opt.textContent = r.name;
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
    // A running session reads its loaded ruleset content fresh every turn
    // (`chat_turn::run_chat_turn`), same as personas -- no need to refresh
    // anything else here, the edit just takes effect on the next turn.
  } catch (err) {
    alert(`Save failed: ${err}`);
  }
});

// --- Sessions ---

// A session's persona is fixed at creation (`meta.persona`, read fresh by
// `send_chat_message` every turn) -- there's no "change persona mid-chat"
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
    renameBtn.textContent = "✎";
    renameBtn.title = "Rename";
    renameBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      openRenameDialog(s.id, s.title);
    });
    const deleteBtn = document.createElement("button");
    deleteBtn.type = "button";
    deleteBtn.textContent = "🗑";
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
// same source; see `rules::build_state_markdown`.
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

// Re-draws the log from `chatHistory` already in memory -- no network
// round-trip, so it's also what the narration toggle uses to make itself
// take effect immediately on whatever's already on screen.
// Reconstructs the image thumbnails a redraw would otherwise drop --
// `m.images` (raw data URLs) is the only attachment data actually persisted
// per message; text attachments are folded into `content` at send time
// instead, so there's nothing to reconstruct for those.
function attachmentsFromMessage(m) {
  return (m.images || []).map((data) => ({ kind: "image", name: "image", data }));
}

function renderChatHistoryLog() {
  // A still-in-flight placeholder (a "thinking"/generating/searching mask)
  // isn't in `chatHistory` yet -- only once it resolves does its content
  // get mirrored in (see the `thinking`/`generated_images` mirroring
  // comments above). Wiping and rebuilding purely from history would
  // silently drop it, and worse, orphan it: a later `replaceWith`/mutation
  // call resolving it would then target a detached node and do nothing
  // visible. Carried over physically instead -- same nodes, same live
  // references, just re-appended after the rebuild in their original
  // relative order (they're always logically "after" the last
  // history-backed bubble at the moment they're created).
  const transient = Array.from(chatModeLog.querySelectorAll(".chat-transient"));
  chatModeLog.innerHTML = "";
  const showThinking = currentConfig?.chat_show_thinking ?? true;
  for (const m of chatHistory) {
    if (m.role === "assistant" && m.thinking && showThinking) {
      appendCompletedThinkingBubble(m.thinking);
    }
    const bubble = appendChatModeBubble(m.role, m.content, attachmentsFromMessage(m));
    // `generated_images` holds local file paths, not data URLs (unlike
    // `images`), so each one needs its own async read -- kept separate from
    // attachmentsFromMessage (synchronous, reused by this same hot
    // toggle-redraw path) rather than blocking the initial render on a
    // round-trip per image.
    for (const path of m.generated_images || []) {
      invoke("read_generated_image", { path })
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
  for (const el of transient) {
    chatModeLog.appendChild(el);
  }
  if (transient.length > 0) {
    chatModeLog.scrollTop = chatModeLog.scrollHeight;
  }
}

async function openChatSession(id) {
  try {
    const { meta, history } = await invoke("load_chat_session", { sessionId: id });
    currentSessionId = id;
    chatHistory = history;
    setPersonaSelectorForOpenSession(meta.persona);
    renderChatHistoryLog();
    await loadSessionList();
  } catch (err) {
    alert(`Could not open that chat: ${err}`);
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
    removeBtn.textContent = "✕";
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

// Chat mode is persona/roleplay-shaped, so every reply is expected to be
// fully wrapped in one of two explicit markers: narration/action in
// `// text //`, spoken dialogue in `|| text ||` -- e.g. "// she takes a
// slow sip // || That's really refreshing. ||". Both used to share one
// implicit rule (unwrapped text defaulted to dialogue), which a real
// session showed the model reading as license to drop a stray, unpaired
// `//` in as an ad-hoc separator between sentences -- it couldn't pair with
// anything, so it leaked into the display as literal slashes. Scanning for
// complete pairs instead of an alternating split() means a stray marker
// like that just falls through to `stripStrayMarkers` below instead of
// throwing off everything that comes after it. (A single asterisk used to
// mark narration before `//`/`||` existed, which is why plain Markdown
// italics went unhandled for a while -- `*text*` collided with it.)
// Bold/italic/code/fenced-code markup still works inside any block; it's
// pulled out first.
//
// `hideNarration` (from `chat_hide_narration`) drops action blocks from the
// output entirely rather than just styling them differently -- the model is
// still always told to write them (`rules::CHAT_NARRATION_PROMPT`), this is
// purely a display choice.
// Parses `text` into narration/dialogue blocks (`{type: "action"|"dialogue",
// html}`), kept separate rather than joined into one string --
// `groupChatBlocksIntoBubbles` below needs the blocks apart to split them
// across multiple bubbles.
function splitChatBlocks(text, hideNarration = false) {
  const codeBlocks = [];
  let working = text.replace(/```([a-zA-Z]*)\n([\s\S]*?)```/g, (_, _lang, code) => {
    codeBlocks.push(code.replace(/\n$/, ""));
    return ` CB${codeBlocks.length - 1} `;
  });
  const inlineCode = [];
  working = working.replace(/`([^`\n]+)`/g, (_, code) => {
    inlineCode.push(code);
    return ` IC${inlineCode.length - 1} `;
  });
  const bold = [];
  working = working.replace(/\*\*([^*\n]+)\*\*/g, (_, b) => {
    bold.push(b);
    return ` B${bold.length - 1} `;
  });
  // Bold has already been pulled out above, so any `*...*` left at this
  // point is unambiguously italic, not half of a `**bold**` pair.
  const italic = [];
  working = working.replace(/\*([^*\n]+)\*/g, (_, em) => {
    italic.push(em);
    return ` EM${italic.length - 1} `;
  });

  const restore = (escaped) =>
    escaped
      .replace(
        / CB(\d+) /g,
        (_, i) => `<pre class="md-code"><code>${escapeHtml(codeBlocks[Number(i)])}</code></pre>`,
      )
      .replace(/ IC(\d+) /g, (_, i) => `<code>${escapeHtml(inlineCode[Number(i)])}</code>`)
      .replace(/ B(\d+) /g, (_, i) => `<strong>${escapeHtml(bold[Number(i)])}</strong>`)
      .replace(/ EM(\d+) /g, (_, i) => `<em>${escapeHtml(italic[Number(i)])}</em>`);

  // Any leftover `//`/`||` here is a stray, unpaired marker (a real pair
  // would already have been consumed by the scan below) -- strip it rather
  // than showing it as literal punctuation. A run of backslashes gets the
  // same treatment: our protocol never uses one, so it only ever shows up
  // as degenerate trailing noise from a reply that trails off mid-block
  // (observed: a narration block left unclosed, ending in a handful of
  // stray `\` with no real content after them).
  const stripStrayMarkers = (s) => s.replace(/\/\/|\|\||\\+/g, "").trim();

  // Scans for complete narration/dialogue pairs in one pass, in whichever
  // order they actually appear, rather than assuming a strict alternation.
  // The closing marker can be *either* `//` or `||`, not just a match of
  // the opener -- a real reply opened with `//` and, partway through,
  // switched to closing with `||` before eventually closing that with `//`
  // again; matching only same-delimiter pairs would swallow the model's
  // whole mismatched run (dialogue included) as one giant narration block.
  // Classified by the *opening* delimiter, which reflects what the model
  // actually intended even when it fumbles the close. Non-greedy `[\s\S]+?`
  // lets a pair span multiple lines -- an accidental line break shouldn't
  // be enough to break the match and leak both markers as stray text. Not
  // a `blocks || restore(...text)`-style fallback to the raw text on an
  // empty result -- with `hideNarration` on, an all-narration message
  // legitimately renders as nothing, and falling back would put the very
  // markers it's meant to hide right back on screen.
  const blockPattern = /(\/\/|\|\|)([\s\S]+?)(?:\/\/|\|\|)/g;
  const blocks = [];
  let lastIndex = 0;
  let match;
  while ((match = blockPattern.exec(working))) {
    const before = stripStrayMarkers(working.slice(lastIndex, match.index));
    // A fumbled close right next to an open (e.g. "...picture //. || Oh")
    // leaves the stray "." sitting between the two matched blocks -- real
    // prose always has a letter/digit in it, so anything without one here
    // is punctuation debris from the marker fumble, not content worth its
    // own bubble.
    if (before && /[a-zA-Z0-9]/.test(before)) {
      blocks.push({
        type: "dialogue",
        html: `<div class="chat-dialogue">${restore(escapeHtml(before))}</div>`,
      });
    }
    const [, opener, content] = match;
    // A malformed close right next to the next open (e.g. "...embrace \\ ||
    // ...here! ||\n\n// Her hips...") can make the regex pair a closing
    // marker with the *next* block's opening marker instead of a real one,
    // leaving nothing but whitespace between them -- without this check
    // that becomes an empty `<div>`, a blank bubble with nothing in it. Same
    // "no letter/digit means it's debris, not content" rule the `before`
    // and `tail` leftovers below already use, and stray backslash noise
    // (see `stripStrayMarkers`) gets cleaned out of real content here too,
    // not just leftovers.
    const cleaned = content.trim().replace(/\\+/g, "").trim();
    if (/[a-zA-Z0-9]/.test(cleaned)) {
      if (opener === "//") {
        if (!hideNarration) {
          blocks.push({
            type: "action",
            html: `<div class="chat-action">${restore(escapeHtml(cleaned))}</div>`,
          });
        }
      } else {
        blocks.push({
          type: "dialogue",
          html: `<div class="chat-dialogue">${restore(escapeHtml(cleaned))}</div>`,
        });
      }
    }
    lastIndex = blockPattern.lastIndex;
  }
  const tail = stripStrayMarkers(working.slice(lastIndex));
  if (tail && /[a-zA-Z0-9]/.test(tail)) {
    blocks.push({
      type: "dialogue",
      html: `<div class="chat-dialogue">${restore(escapeHtml(tail))}</div>`,
    });
  }
  return blocks;
}

// Splits blocks across bubbles so each one holds at most one narration and
// one dialogue block, in whatever order they actually appeared -- mirrors
// `CHAT_NARRATION_PROMPT`'s alternating narration/dialogue beats in the DOM
// as separate bubbles instead of one wall of stacked divs. A group only ever
// grows past one block when the next block is the *other* kind; hitting the
// same kind twice (a still-malformed reply, or two dialogue blocks left over
// once narration is hidden) starts a fresh bubble instead of silently
// merging mismatched beats together.
function groupChatBlocksIntoBubbles(blocks) {
  const groups = [];
  let current = [];
  for (const block of blocks) {
    if (current.length === 0) {
      current.push(block);
    } else if (current.length === 1 && current[0].type !== block.type) {
      current.push(block);
      groups.push(current);
      current = [];
    } else {
      groups.push(current);
      current = [block];
    }
  }
  if (current.length > 0) groups.push(current);
  return groups;
}

function appendAttachmentsToBubble(div, attachments) {
  for (const att of attachments) {
    if (att.kind === "image") {
      const img = document.createElement("img");
      img.className = "chat-attachment-thumb";
      img.src = att.data;
      div.appendChild(img);
    } else {
      const note = document.createElement("div");
      note.className = "chat-attachment-note";
      note.textContent = `📄 ${att.name}`;
      div.appendChild(note);
    }
  }
}

// A narration/dialogue reply becomes one bubble *per* `groupChatBlocksIntoBubbles`
// group -- never more than one narration and one dialogue beat stacked in a
// single bubble -- so the chat log visually mirrors the same alternation
// `CHAT_NARRATION_PROMPT` asks the model for, instead of the whole reply
// landing as one wall of divs. Attachments go on the last bubble created,
// which is also what's returned, matching every caller's use of the return
// value (appending a generated-image thumbnail, the state-updated badge).
function appendChatModeBubble(role, text, attachments = []) {
  if (role === "system") {
    const div = document.createElement("div");
    div.className = `bubble ${role}`;
    div.textContent = text;
    appendAttachmentsToBubble(div, attachments);
    chatModeLog.appendChild(div);
    chatModeLog.scrollTop = chatModeLog.scrollHeight;
    return div;
  }

  const hideNarration = currentConfig?.chat_hide_narration ?? false;
  const blocks = splitChatBlocks(text, hideNarration);
  // `CHAT_NARRATION_PROMPT`'s strict alternation is a rule for the *model*,
  // never the user -- a person typing is free to mix narration-style asides
  // into a message however they like without it getting split apart, so only
  // the assistant's own replies get grouped into one bubble per
  // narration+dialogue pair; every other role keeps everything in one bubble.
  const groups = role === "assistant" ? groupChatBlocksIntoBubbles(blocks) : [blocks];
  if (groups.length === 0) groups.push([]); // still show an (empty) bubble, e.g. all-narration + hidden

  let lastDiv = null;
  groups.forEach((group, i) => {
    const div = document.createElement("div");
    div.className = `bubble ${role}`;
    div.innerHTML = group.map((b) => b.html).join("");
    if (i === groups.length - 1) {
      appendAttachmentsToBubble(div, attachments);
    }
    chatModeLog.appendChild(div);
    lastDiv = div;
  });
  chatModeLog.scrollTop = chatModeLog.scrollHeight;
  return lastDiv;
}

// Turns 2/3/4 (dispatch masking, image reaction, search answer) still get
// no live "thinking" phase -- their reply arrives in one piece, so this
// placeholder's job there is only to cover the request round-trip itself.
// Turn 1's own reply is streamed (see invokeChatStream/createLiveReplyBubble
// below); for a backend that reports reasoning via a separate delta field,
// appendChatThinkingDelta upgrades this same placeholder live as reasoning
// text actually arrives, rather than only once the whole reply is done.
// Either way this starts *plain*, not expandable: whether there'll be real
// reasoning to show is only known once something actually arrives, and an
// expandable `details` with a disclosure triangle sitting open on nothing
// yet is misleading (and, `align-self: stretch`-wide, unnecessarily large)
// for something that's often going to stay empty -- including every use of
// this as pure masking (`run_turn_followup`'s call in the chat submit
// handler), which never has real content to show at all. `resolveChatThinking`
// below decides the final shape once the reply is fully done: left upgraded
// (or upgraded now, if nothing streamed in live) if real text arrived,
// removed outright otherwise -- so a masking call and a real call with
// nothing to show end up looking and behaving identically, and only a call
// that actually got reasoning back pays for the bigger, expandable form.
function createChatThinkingPlaceholder() {
  const pending = document.createElement("div");
  // `chat-transient` -- see renderChatHistoryLog's doc comment: anything
  // still live and not yet reflected in `chatHistory` needs to survive a
  // redraw (the narration toggle, most commonly) by being physically
  // carried over rather than rebuilt, or a `replaceWith`/mutation call that
  // resolves later silently targets an orphaned node.
  pending.className = "bubble thinking-pending chat-transient";
  pending.textContent = "🧠 Thinking…";
  chatModeLog.appendChild(pending);
  chatModeLog.scrollTop = chatModeLog.scrollHeight;
  return { pending };
}

function resolveChatThinking(placeholder, thinkingText) {
  if (!thinkingText) {
    placeholder.pending.remove();
    return;
  }
  placeholder.pending.replaceWith(buildCompletedThinkingBubble(thinkingText));
}

// Shared by resolveChatThinking (upgrading a live placeholder once real
// content arrives) and appendCompletedThinkingBubble (session-history
// replay, which has the text immediately and was never a placeholder to
// begin with) -- same finished shape either way, built in exactly one place.
function buildCompletedThinkingBubble(thinkingText) {
  const details = document.createElement("details");
  details.className = "bubble thinking";
  const summary = document.createElement("summary");
  summary.textContent = "🧠 Completed";
  details.appendChild(summary);
  const pre = document.createElement("pre");
  pre.className = "chat-thinking-content";
  pre.textContent = thinkingText;
  details.appendChild(pre);
  return details;
}

function appendCompletedThinkingBubble(thinkingText) {
  const details = buildCompletedThinkingBubble(thinkingText);
  chatModeLog.appendChild(details);
  return details;
}

// Lazily upgrades a plain "🧠 Thinking…" placeholder into an expandable,
// left-open, live-updating shape the first time real reasoning text
// actually arrives -- only reached for a backend that reports reasoning via
// the separate delta field (see llm::ChatDelta's doc comment); a backend
// that embeds it inline in the regular content stream never calls this,
// same known limitation noted there. Keeps `placeholder.pending` pointed at
// whatever's actually live in the DOM, so resolveChatThinking's own
// `placeholder.pending.replaceWith(...)` -- called once the reply is fully
// done, with the authoritative complete thinking text -- and the submit
// handler's `catch` block's `placeholder.pending.remove()` both keep
// working unmodified whether or not this ever ran.
function appendChatThinkingDelta(placeholder, text) {
  if (!placeholder.pre) {
    const details = document.createElement("details");
    details.className = "bubble thinking";
    details.open = true;
    const summary = document.createElement("summary");
    summary.textContent = "🧠 Thinking…";
    details.appendChild(summary);
    const pre = document.createElement("pre");
    pre.className = "chat-thinking-content";
    details.appendChild(pre);
    placeholder.pending.replaceWith(details);
    placeholder.pending = details;
    placeholder.pre = pre;
  }
  placeholder.pre.textContent += text;
  chatModeLog.scrollTop = chatModeLog.scrollHeight;
}

// Turn 1's live reply bubble while its stream is still in flight --
// deliberately plain, unstyled running text (`textContent +=`, not
// `innerHTML`), never run through splitChatBlocks's narration/dialogue
// marker parsing: that parser assumes a complete string, and a trailing
// unclosed `//`/`||` marker on a still-growing partial string gets silently
// stripped as stray debris (stripStrayMarkers) rather than treated as
// "pending, don't finalize yet." Replaced outright by the real,
// properly-split appendChatModeBubble call once the stream finishes -- see
// the submit handler below.
function createLiveReplyBubble() {
  const div = document.createElement("div");
  // `chat-transient` -- see renderChatHistoryLog's doc comment, same
  // reasoning as createChatThinkingPlaceholder's placeholder: not yet
  // reflected in `chatHistory`, so it must survive a redraw (the narration
  // toggle) by being physically carried over rather than rebuilt.
  div.className = "bubble assistant chat-transient";
  chatModeLog.appendChild(div);
  chatModeLog.scrollTop = chatModeLog.scrollHeight;
  return div;
}

// A memory update being *triggered* for a turn, not confirmation it
// *finished* -- state-update now runs as a detached background task (see
// TurnFollowupOutcome::state_update_dispatched's doc comment in main.rs),
// so there's no moment at which this could accurately report completion
// without waiting on it, which is exactly what this app no longer does for
// this turn. Shown the instant the dispatching call's result comes back, as
// a small icon-only badge beside the bubble it belongs to, bottom-aligned
// -- a sibling, not overlaid on top of the bubble's own content, so it
// never sits over a narration/dialogue line near the bottom edge. Wraps the
// bubble in a small flex row in place of it (same position in `chat-log`,
// same alignment) since the badge needs a layout neighbor to sit beside,
// not something CSS alone can add to an existing standalone bubble. Hover
// for what it means.
function appendStateDispatchedIndicator(bubble) {
  const wrapper = document.createElement("div");
  wrapper.className = "bubble-with-indicator";
  bubble.replaceWith(wrapper);
  wrapper.appendChild(bubble);
  const badge = document.createElement("span");
  badge.className = "state-updated-badge";
  badge.textContent = "📝";
  badge.title = "this turn triggered a memory update in the background";
  wrapper.appendChild(badge);
}

// Same "placeholder now, filled in or removed once the real thing arrives"
// shape as createChatThinkingPlaceholder -- generation is a separate,
// possibly slow (seconds to minutes) step after the text reply already
// showed (see ChatTurnOutcome::image_prompt_requested's doc comment in
// chat_turn.rs for why), so it gets its own placeholder rather than making
// the whole turn wait on it.
function createImageGenPlaceholder() {
  const div = document.createElement("div");
  // See createChatThinkingPlaceholder's comment on `chat-transient`.
  div.className = "bubble assistant chat-transient";
  div.textContent = "🎨 Generating image… this can take a while";
  chatModeLog.appendChild(div);
  chatModeLog.scrollTop = chatModeLog.scrollHeight;
  return div;
}

function resolveImageGenPlaceholder(div, dataUrl, path, errorText) {
  if (errorText) {
    // No chatHistory entry represents a failed generation, so this div
    // stays `chat-transient` (see createChatThinkingPlaceholder's comment)
    // -- it still needs to survive a redraw on its own.
    div.textContent = `⚠ Image generation failed: ${errorText}`;
    return;
  }
  // The image itself is mirrored into chatHistory right after this call
  // (see the `turn1Message.generated_images` assignment at its call site),
  // so a future redraw already reconstructs an equivalent bubble from
  // history -- keeping `chat-transient` here would carry this exact node
  // over too and duplicate it. Drop it now that the mirror covers this.
  div.classList.remove("chat-transient");
  div.textContent = "";
  const img = document.createElement("img");
  img.className = "chat-generated-image";
  img.src = dataUrl;
  wireImagePreview(img, path);
  div.appendChild(img);
  chatModeLog.scrollTop = chatModeLog.scrollHeight;
}

// Same shape as createImageGenPlaceholder/resolveImageGenPlaceholder, but
// there's no equivalent of "the image" to show in place -- the real payoff
// is the answer turn's own message, appended separately once it lands, so
// this placeholder just disappears on success rather than being filled in.
function createWebSearchPlaceholder() {
  const div = document.createElement("div");
  // See createChatThinkingPlaceholder's comment on `chat-transient`.
  div.className = "bubble assistant chat-transient";
  div.textContent = "🔎 Searching the web… this can take a moment";
  chatModeLog.appendChild(div);
  chatModeLog.scrollTop = chatModeLog.scrollHeight;
  return div;
}

function resolveWebSearchPlaceholder(div, errorText) {
  if (errorText) {
    div.textContent = `⚠ Web search failed: ${errorText}`;
    return;
  }
  div.remove();
}

// --- Generated image preview ---
//
// A plain fixed overlay, not a <dialog>/showModal() -- see the CSS comment
// on `.image-preview-overlay` for why (this webview's <dialog> top-layer
// support turned out not to actually center or backdrop-dim, which every
// other -- opaque, content-sized -- dialog in this app happened to mask).

const imagePreviewOverlay = document.getElementById("imagePreviewOverlay");
const imagePreviewInner = imagePreviewOverlay.querySelector(".image-preview-inner");
const imagePreviewImg = document.getElementById("imagePreviewImg");
const imagePreviewSaveResult = document.getElementById("imagePreviewSaveResult");
let imagePreviewPath = null;

function openImagePreview(dataUrl, path) {
  imagePreviewImg.src = dataUrl;
  imagePreviewPath = path;
  imagePreviewSaveResult.textContent = "";
  focusMainWindow();
  imagePreviewOverlay.hidden = false;
}

function closeImagePreview() {
  imagePreviewOverlay.hidden = true;
}

// Every generated `<img>` gets this -- clicking it pops the same quick-view
// preview, path included so the Save button knows which on-disk file to
// copy from.
function wireImagePreview(img, path) {
  img.addEventListener("click", () => openImagePreview(img.src, path));
}

document.getElementById("imagePreviewCloseBtn").addEventListener("click", closeImagePreview);

// Clicking the dimmed backdrop (anywhere outside the image/buttons) closes
// it too, same as a native <dialog>'s backdrop would have.
imagePreviewOverlay.addEventListener("click", (e) => {
  if (!imagePreviewInner.contains(e.target)) closeImagePreview();
});

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !imagePreviewOverlay.hidden) closeImagePreview();
});

document.getElementById("imagePreviewSaveBtn").addEventListener("click", async () => {
  if (!imagePreviewPath) return;
  // No native "save as" dialog outside Tauri -- the image is already sitting
  // in the preview as a `data:` URL, so a browser can just download it
  // directly, no server round trip needed at all.
  if (!window.__TAURI__) {
    const a = document.createElement("a");
    a.href = imagePreviewImg.src;
    a.download = imagePreviewPath.split(/[/\\]/).pop() || "image.png";
    a.click();
    return;
  }
  try {
    const dest = await invoke("save_generated_image_as", { path: imagePreviewPath });
    imagePreviewSaveResult.textContent = dest ? `Saved to ${dest}` : "";
  } catch (err) {
    imagePreviewSaveResult.textContent = `Save failed: ${err}`;
  }
});

chatModeInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    if (!chatModeSendBtn.disabled) {
      chatModeForm.requestSubmit();
    }
  }
});

// Replaces the old Settings "Test vision support" button: the real,
// authoritative probe (`test_vision_support` -- see its own doc comment for
// why the passive `probe_vision_capability` isn't good enough here) now
// fires once per app run, in the background, the first time a message is
// actually sent, with the result surfaced next to the narration toggle --
// and now also gates whether the image reaction turn (turn 3) is worth
// running at all: without vision, the model can't actually see the picture,
// so continuing the scene "from" it would only be guessing at what it
// looks like, worse than just not reacting. `visionConfirmed` starts `null`
// (not yet known) rather than `false`, so a caller can tell "definitely no
// vision" apart from "haven't checked yet" and await the in-flight probe
// instead of wrongly treating "not checked" as "no".
let visionConfirmed = null;
let visionProbePromise = null;
// What `visionProbePromise` actually answered for -- endpoint+model, not
// just "have we ever probed". Settings can change either between sends
// without a restart, and a probe result for the old pairing has nothing to
// say about the new one (a prior "no vision" model switched out for a real
// vision model would otherwise stay wrongly cached as `false` all session).
let visionProbedFor = null;
function ensureVisionProbe() {
  if (!currentConfig) return visionProbePromise;
  const target = `${currentConfig.endpoint}::${currentConfig.model}`;
  if (visionProbePromise && visionProbedFor === target) return visionProbePromise;
  visionProbedFor = target;
  const el = document.getElementById("visionStatusIndicator");
  el.classList.remove("vision-ok", "vision-off");
  el.textContent = "👁️";
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
      el.title = `No vision support detected: ${err} -- image reactions will be skipped`;
      el.classList.add("vision-off");
    });
  return visionProbePromise;
}

chatModeForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  ensureVisionProbe(); // not awaited -- this shouldn't delay sending
  const typed = chatModeInput.value.trim();
  if (!typed && pendingAttachments.length === 0) return;

  if (!currentSessionId) {
    // Sending with nothing open implicitly starts a chat, so a first-time
    // user isn't forced to click "New chat" before saying anything.
    try {
      const persona = personaSelect.value || null;
      const summary = await invoke("create_chat_session", { persona });
      currentSessionId = summary.id;
      setPersonaSelectorForOpenSession(persona);
      await loadSessionList();
    } catch (err) {
      alert(`Could not start a chat: ${err}`);
      return;
    }
  }

  const attachments = pendingAttachments;
  pendingAttachments = [];
  renderAttachmentsPreview();

  const images = attachments.filter((a) => a.kind === "image").map((a) => a.data);
  // Documents need no backend support at all -- their content just becomes
  // part of the message text the model already reads.
  let sentContent = attachments
    .filter((a) => a.kind === "text")
    .reduce((acc, a) => `[attached: ${a.name}]\n${a.data}\n\n${acc}`, typed);
  if (!sentContent.trim() && images.length > 0) {
    sentContent = "What's in this image?";
  }

  chatModeInput.value = "";
  // The log shows what was actually typed plus a thumbnail/note, not the
  // full folded document text -- that would dump a whole file into the chat.
  appendChatModeBubble("user", typed || "(sent with attachment)", attachments);
  chatHistory.push({ role: "user", content: sentContent, images });

  chatModeSendBtn.disabled = true;
  const showThinking = currentConfig?.chat_show_thinking ?? true;
  const streamReplies = currentConfig?.chat_stream_replies ?? true;
  const thinkingPlaceholder = showThinking ? createChatThinkingPlaceholder() : null;
  // The backend call is always the streaming one either way (see
  // invokeChatStream) -- this setting only controls whether content deltas
  // also get shown live in their own bubble, or just silently accumulate
  // behind the thinking placeholder until the full reply is ready (the
  // pre-streaming behavior). Reasoning deltas above are unaffected by this
  // setting either way.
  const liveBubble = streamReplies ? createLiveReplyBubble() : null;
  let liveRawReply = "";
  try {
    const result = await invokeChatStream(currentSessionId, chatHistory, (delta) => {
      if (delta.kind === "content" && liveBubble) {
        // `//`/`||` are the narration/dialogue markers (CHAT_NARRATION_PROMPT)
        // -- stripped from this live view only, so raw slashes/pipes don't
        // flash past mid-sentence before the real, properly-split render
        // takes over below. Recomputed from the full text accumulated so
        // far each chunk, not stripped per-delta and appended -- a marker
        // can straddle two chunks, and appending an already-stripped
        // fragment could leave half of one behind. `result.reply` below
        // (storage, dispatch, the final render) is completely untouched.
        liveRawReply += delta.text;
        liveBubble.textContent = liveRawReply.replace(/\/\/|\|\|/g, "");
        chatModeLog.scrollTop = chatModeLog.scrollHeight;
      } else if (delta.kind === "reasoning" && thinkingPlaceholder) {
        appendChatThinkingDelta(thinkingPlaceholder, delta.text);
      }
    });
    liveBubble?.remove();
    // Before pushing the reply, or it lands on the array being discarded --
    // same reasoning as the file-ops summary handling above.
    if (result.rewritten_history) {
      chatHistory = result.rewritten_history;
    }
    // `thinking` mirrored in too, same reasoning as `generated_images`
    // below -- renderChatHistoryLog (the narration toggle, most commonly)
    // rebuilds purely from this array, so a resolved thinking bubble held
    // only in the DOM would vanish on the next redraw and never come back.
    chatHistory.push({ role: "assistant", content: result.reply, thinking: result.thinking });
    const turn1Message = chatHistory[chatHistory.length - 1];
    if (thinkingPlaceholder) {
      resolveChatThinking(thinkingPlaceholder, result.thinking);
    }
    const turn1Bubble = appendChatModeBubble("assistant", result.reply);
    if (result.summary) {
      appendChatModeBubble(
        "system",
        `Conversation got long — summarized ${result.summarized} older message(s) to fit.`,
      );
    }
    if (result.dropped > 0) {
      appendChatModeBubble(
        "system",
        `Conversation got long — dropped ${result.dropped} oldest message(s) to stay within the context window.`,
      );
    }
    await loadSessionList(); // title/ordering may have just changed

    // Turn 2: dispatch, plus the raw-JSON half of the state-update turn --
    // awaited server-side *before* dispatch runs, not alongside it, since
    // dispatch's own completion is what writes an image-prompt fence when
    // that ruleset is loaded, and it needs this turn's fresh state to
    // describe the character accurately (see chat_turn.rs's module doc
    // comment). That means this call genuinely takes a moment before
    // resolving, unlike a purely-detached background task -- masked below
    // with the same "thinking" placeholder style used elsewhere, purely
    // cosmetic (there's no real model reasoning behind it, and it's always
    // resolved with no content) so the user sees *something* is running
    // instead of a silent gap between the reply and whatever dispatch
    // decides next (a ruleset loading, an image starting to generate).
    // Still fired only now that the reply above is already on screen, and
    // still not part of the button re-enabling/`finally` below -- once
    // this resolves, everything downstream (image generation, web search)
    // continues to run in its own follow-up exactly as before.
    const followupSessionId = currentSessionId;
    const followupMask = createChatThinkingPlaceholder();
    invoke("run_turn_followup", {
      sessionId: followupSessionId,
      lastUserMessage: sentContent,
      lastAssistantReply: result.reply,
    })
      .then((followup) => {
        resolveChatThinking(followupMask, null); // always just removes it -- see above
        if (currentSessionId !== followupSessionId) return; // switched chats meanwhile
        // Known the instant dispatch's own result comes back, not once the
        // background state-update actually finishes -- see
        // appendStateDispatchedIndicator's doc comment.
        if (followup.state_update_dispatched && turn1Bubble) {
          appendStateDispatchedIndicator(turn1Bubble);
        }
        // Dispatch-mechanism chatter (which ruleset got loaded, whether it
        // then decided not to use it, a bad ruleset name) is internal
        // bookkeeping, not something worth a chat bubble -- it's already in
        // the app log for whoever needs to debug it. Only real outcomes (a
        // reply, a generated image) show up in the chat.
        if (followup.ruleset_loaded) {
          console.debug(`dispatch: loaded ruleset ${followup.ruleset_loaded}`);
        }
        if (followup.ruleset_error) {
          console.debug(`dispatch: ${followup.ruleset_error}`);
        }
        if (followup.image_prompt_requested) {
          // Not awaited -- generation can take anywhere from seconds to
          // minutes, and the rest of this turn's UI updates shouldn't wait
          // on it.
          const placeholder = createImageGenPlaceholder();
          const genSessionId = followupSessionId;
          const positivePrompt = followup.image_prompt_requested.positive || "an image";
          invoke("generate_comfyui_image", {
            sessionId: genSessionId,
            fields: followup.image_prompt_requested,
          })
            .then(async (genResult) => {
              resolveImageGenPlaceholder(placeholder, genResult.data_url, genResult.path);
              // Mirror what's already persisted on the Rust side into the
              // in-memory `chatHistory` too -- `renderChatHistoryLog` (used
              // by the narration toggle, and by re-sending on the next
              // turn) rebuilds purely from this array, so anything only
              // pushed to the DOM here (the placeholder image, the reaction
              // bubble) used to vanish the moment the toggle redrew the log.
              if (currentSessionId !== genSessionId) return; // switched chats mid-generation
              turn1Message.generated_images = [
                ...(turn1Message.generated_images || []),
                genResult.path,
              ];
              if (!genResult.reaction_pending) return; // ReactionMode::Never -- nothing coming
              // Without vision, the model can't actually see the picture, so
              // "continuing the scene" from it would just be guessing what
              // it looks like -- worse than saying nothing.
              // `ensureVisionProbe()` was already kicked off (not awaited)
              // when this turn's message was sent, so this is normally an
              // instant no-op by now; awaiting it here only matters for a
              // fast image generation racing ahead of that probe's own
              // round-trip.
              await ensureVisionProbe();
              if (visionConfirmed === false) return;
              // Turn 3 (the in-character reaction) is its own follow-up
              // round-trip, deliberately not awaited above -- the image
              // itself is already final the moment `generate_comfyui_image`
              // resolves, so it shouldn't sit behind a second, separate LLM
              // call it has no bearing on. Reuses the same "thinking"
              // placeholder turn 1 uses: a live indicator while this call is
              // in flight, filled in with the model's own reasoning (if
              // any) once it resolves.
              const reactionPlaceholder = createChatThinkingPlaceholder();
              invoke("run_image_reaction", {
                sessionId: genSessionId,
                positivePrompt,
                imageDataUrl: genResult.data_url,
              })
                .then((reaction) => {
                  resolveChatThinking(reactionPlaceholder, reaction.thinking);
                  if (currentSessionId !== genSessionId) return;
                  // Absent (not an error) if the reaction call itself
                  // failed or decided not to comment; the image itself
                  // already generated fine either way.
                  if (reaction.text) {
                    const reactionBubble = appendChatModeBubble("assistant", reaction.text);
                    if (reaction.state_update_dispatched && reactionBubble) {
                      appendStateDispatchedIndicator(reactionBubble);
                    }
                    // See the `thinking` mirroring comment on turn 1's push
                    // above -- same reasoning applies here.
                    chatHistory.push({
                      role: "assistant",
                      content: reaction.text,
                      thinking: reaction.thinking,
                    });
                  }
                })
                .catch((err) => {
                  reactionPlaceholder.pending.remove();
                  console.debug(`image reaction failed: ${err}`);
                });
            })
            .catch((err) => resolveImageGenPlaceholder(placeholder, null, null, String(err)));
        }
        if (followup.web_search_requested) {
          // Not awaited, same reasoning as image generation above.
          const placeholder = createWebSearchPlaceholder();
          const searchSessionId = followupSessionId;
          const query = followup.web_search_requested;
          invoke("run_web_search", { query })
            .then((searchResult) => {
              resolveWebSearchPlaceholder(placeholder);
              if (currentSessionId !== searchSessionId) return; // switched chats mid-search
              // The search itself failing (vs. succeeding with nothing
              // relevant) still gets an in-character answer below -- this
              // is just for debugging, not shown to the user directly.
              if (searchResult.search_error) {
                console.debug(`web search failed: ${searchResult.search_error}`);
              }
              // Turn 4 (the in-character answer), as its own follow-up
              // round-trip -- same reasoning and the same "thinking"
              // indicator as the image reaction split above.
              const answerPlaceholder = createChatThinkingPlaceholder();
              invoke("run_search_answer", {
                sessionId: searchSessionId,
                query,
                results: searchResult.results,
                searchError: searchResult.search_error,
              })
                .then((answer) => {
                  resolveChatThinking(answerPlaceholder, answer.thinking);
                  if (currentSessionId !== searchSessionId) return;
                  if (answer.text) {
                    const answerBubble = appendChatModeBubble("assistant", answer.text);
                    if (answer.state_update_dispatched && answerBubble) {
                      appendStateDispatchedIndicator(answerBubble);
                    }
                    // See the `thinking` mirroring comment on turn 1's push
                    // above -- same reasoning applies here.
                    chatHistory.push({
                      role: "assistant",
                      content: answer.text,
                      thinking: answer.thinking,
                    });
                  }
                })
                .catch((err) => {
                  answerPlaceholder.pending.remove();
                  console.debug(`search answer failed: ${err}`);
                });
            })
            .catch((err) => resolveWebSearchPlaceholder(placeholder, String(err)));
        }
      })
      .catch((err) => {
        // The `.then()` above never ran, so the mask is still up -- make
        // sure it doesn't linger forever just because this call itself
        // failed outright.
        resolveChatThinking(followupMask, null);
        console.debug(`turn follow-up failed: ${err}`);
      });
  } catch (err) {
    // The optimistically-pushed user message above never got a reply, and
    // `run_chat_turn`/`run_chat_turn_streaming` never persisted it (only
    // saves history after a successful reply) -- so leaving it in
    // `chatHistory` would silently resend it, and whatever made it fail, on
    // every turn after this one.
    liveBubble?.remove();
    chatHistory.pop();
    if (thinkingPlaceholder) thinkingPlaceholder.pending.remove();
    appendChatModeBubble("system", `Error: ${err}`);
  } finally {
    chatModeSendBtn.disabled = false;
  }
});
