// Shared by both modes: backend transport (Tauri/--server), auth overlay, markdown, font size, tabs, logging.

// Icons: Lucide (https://lucide.dev, ISC licence -- see LUCIDE-LICENSE.txt).
// Inner SVG elements copied verbatim from lucide-static v0.468.0; static
// markup only, never built from model data.
const LUCIDE_ICONS = {
  "settings": '<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>',
  "message-circle": '<path d="M7.9 20A9 9 0 1 0 4 16.1L2 22Z"/>',
  "folder-open": '<path d="m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2"/>',
  "upload": '<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="17 8 12 3 7 8"/><line x1="12" x2="12" y1="3" y2="15"/>',
  "plus": '<path d="M5 12h14"/><path d="M12 5v14"/>',
  "pencil": '<path d="M21.174 6.812a1 1 0 0 0-3.986-3.987L3.842 16.174a2 2 0 0 0-.5.83l-1.321 4.352a.5.5 0 0 0 .623.622l4.353-1.32a2 2 0 0 0 .83-.497z"/><path d="m15 5 4 4"/>',
  "trash-2": '<path d="M3 6h18"/><path d="M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6"/><path d="M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2"/><line x1="10" x2="10" y1="11" y2="17"/><line x1="14" x2="14" y1="11" y2="17"/>',
  "puzzle": '<path d="M15.39 4.39a1 1 0 0 0 1.68-.474 2.5 2.5 0 1 1 3.014 3.015 1 1 0 0 0-.474 1.68l1.683 1.682a2.414 2.414 0 0 1 0 3.414L19.61 15.39a1 1 0 0 1-1.68-.474 2.5 2.5 0 1 0-3.014 3.015 1 1 0 0 1 .474 1.68l-1.683 1.682a2.414 2.414 0 0 1-3.414 0L8.61 19.61a1 1 0 0 0-1.68.474 2.5 2.5 0 1 1-3.014-3.015 1 1 0 0 0 .474-1.68l-1.683-1.682a2.414 2.414 0 0 1 0-3.414L4.39 8.61a1 1 0 0 1 1.68.474 2.5 2.5 0 1 0 3.014-3.015 1 1 0 0 1-.474-1.68l1.683-1.682a2.414 2.414 0 0 1 3.414 0z"/>',
  "eye": '<path d="M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0"/><circle cx="12" cy="12" r="3"/>',
  "eye-off": '<path d="M10.733 5.076a10.744 10.744 0 0 1 11.205 6.575 1 1 0 0 1 0 .696 10.747 10.747 0 0 1-1.444 2.49"/><path d="M14.084 14.158a3 3 0 0 1-4.242-4.242"/><path d="M17.479 17.499a10.75 10.75 0 0 1-15.417-5.151 1 1 0 0 1 0-.696 10.75 10.75 0 0 1 4.446-5.143"/><path d="m2 2 20 20"/>',
  "notebook-text": '<path d="M2 6h4"/><path d="M2 10h4"/><path d="M2 14h4"/><path d="M2 18h4"/><rect width="16" height="20" x="4" y="2" rx="2"/><path d="M9.5 8h5"/><path d="M9.5 12H16"/><path d="M9.5 16H14"/>',
  "paperclip": '<path d="M13.234 20.252 21 12.3"/><path d="m16 6-8.414 8.586a2 2 0 0 0 0 2.828 2 2 0 0 0 2.828 0l8.414-8.586a4 4 0 0 0 0-5.656 4 4 0 0 0-5.656 0l-8.415 8.585a6 6 0 1 0 8.486 8.486"/>',
  "x": '<path d="M18 6 6 18"/><path d="m6 6 12 12"/>',
  "brain": '<path d="M12 5a3 3 0 1 0-5.997.125 4 4 0 0 0-2.526 5.77 4 4 0 0 0 .556 6.588A4 4 0 1 0 12 18Z"/><path d="M12 5a3 3 0 1 1 5.997.125 4 4 0 0 1 2.526 5.77 4 4 0 0 1-.556 6.588A4 4 0 1 1 12 18Z"/><path d="M15 13a4.5 4.5 0 0 1-3-4 4.5 4.5 0 0 1-3 4"/><path d="M17.599 6.5a3 3 0 0 0 .399-1.375"/><path d="M6.003 5.125A3 3 0 0 0 6.401 6.5"/><path d="M3.477 10.896a4 4 0 0 1 .585-.396"/><path d="M19.938 10.5a4 4 0 0 1 .585.396"/><path d="M6 18a4 4 0 0 1-1.967-.516"/><path d="M19.967 17.484A4 4 0 0 1 18 18"/>',
  "file-text": '<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/><path d="M10 9H8"/><path d="M16 13H8"/><path d="M16 17H8"/>',
  "image": '<rect width="18" height="18" x="3" y="3" rx="2" ry="2"/><circle cx="9" cy="9" r="2"/><path d="m21 15-3.086-3.086a2 2 0 0 0-2.828 0L6 21"/>',
  "search": '<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/>',
  "triangle-alert": '<path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"/><path d="M12 9v4"/><path d="M12 17h.01"/>',
  "sticky-note": '<path d="M16 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V8Z"/><path d="M15 3v4a2 2 0 0 0 2 2h4"/>',
};

function lucideSvg(name, size = 16) {
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" class="lucide-icon">${LUCIDE_ICONS[name]}</svg>`;
}

// Replaces `el`'s content with just the icon.
function setIcon(el, name, size = 16) {
  el.innerHTML = lucideSvg(name, size);
}

// Replaces `el`'s content with an icon followed by `text` (text goes in as a
// text node, so it's safe for arbitrary strings).
function setIconText(el, name, text, size = 14) {
  el.innerHTML = lucideSvg(name, size);
  el.appendChild(document.createTextNode(" " + text));
}

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
// `{ type: "delta" | "done" | "error", ... }`. This
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

// Wraps invokeStream in a real Promise, resolved/rejected by the backend's
// own "done"/"error" event. `onDelta` gets `{ kind: "content" | "reasoning",
// text }` per `delta` event. A stream that ends with neither (server panic,
// proxy cut) rejects instead of hanging the chat forever.
function invokeChatStream(sessionId, text, images, onDelta) {
  return new Promise((resolve, reject) => {
    let settled = false;
    const settle = (fn, value) => {
      if (settled) return;
      settled = true;
      fn(value);
    };
    const onEvent = (event) => {
      if (event.type === "delta") {
        if (event.content) onDelta({ kind: "content", text: event.content });
        if (event.thinking) onDelta({ kind: "reasoning", text: event.thinking });
      } else if (event.type === "done") settle(resolve, event.result);
      else if (event.type === "error") settle(reject, event.message);
    };
    invokeStream("send_chat_message_streaming", { sessionId, text, images }, onEvent)
      .then(() => {
        // Tauri resolves the command before its last channel message is
        // necessarily delivered, so give that a moment before calling it lost.
        setTimeout(() => settle(reject, "stream ended unexpectedly"), window.__TAURI__ ? 1500 : 0);
      })
      .catch((err) => settle(reject, err));
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


let currentConfig = null;


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
    return `\u0000CODEBLOCK${codeBlocks.length - 1}\u0000`;
  });

  working = escapeHtml(working);
  working = working.replace(/`([^`\n]+)`/g, (_, code) => `<code>${code}</code>`);
  working = working.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  working = working.replace(/(?<!\*)\*([^*\n]+)\*(?!\*)/g, "<em>$1</em>");

  working = working.replace(
    /\u0000CODEBLOCK(\d+)\u0000/g,
    (_, i) => `<pre class="md-code"><code>${escapeHtml(codeBlocks[Number(i)])}</code></pre>`,
  );

  return working;
}

// Mirrors everything shown, thinking steps included, into this session's log.
function logToFile(text) {
  invoke("append_chat_log", { text }).catch((err) => console.error("append_chat_log failed:", err));
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
