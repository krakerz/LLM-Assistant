// File Operations mode: folder picking and the agent task. The backend runs the
// whole tool loop (src/agent.rs) and streams events; this file only renders
// them and answers confirmations. Everything from the model or from a tool
// reaches the DOM via textContent (or renderMarkdown, which escapes first).

const chatLog = document.getElementById("chatLog");
const chatForm = document.getElementById("chatForm");
const chatInput = document.getElementById("chatInput");
const sendBtn = document.getElementById("sendBtn");
const rootPath = document.getElementById("rootPath");
const pickBtn = document.getElementById("pickBtn");
const clearChatBtn = document.getElementById("clearChatBtn");
const unmountBtn = document.getElementById("unmountBtn");
const pickBtnDefaultLabel = pickBtn.textContent;
const NO_FOLDER_LABEL = "No folder selected";

let taskRunning = false;

function scrollToEnd() {
  chatLog.scrollTop = chatLog.scrollHeight;
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
  scrollToEnd();
  logToFile(`[${role}] ${text}`);
  return div;
}

function copyButton(getText, title) {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "copy-btn";
  btn.textContent = "Copy";
  btn.title = title;
  btn.addEventListener("click", (e) => {
    // Inside a <summary>, the click would also toggle the <details>.
    e.preventDefault();
    e.stopPropagation();
    navigator.clipboard.writeText(getText()).catch((err) => console.error("copy failed:", err));
  });
  return btn;
}

// A command the sandbox can never run (sudo and friends): the actionable
// result, so it sits outside the working tracker.
function appendManualCommand(cmd) {
  const div = document.createElement("div");
  div.className = "bubble manual-cmd";

  const header = document.createElement("div");
  header.className = "manual-cmd-header";
  const label = document.createElement("span");
  setIconText(label, "triangle-alert", "Needs administrator rights — run this yourself, then tell me how it went:");
  header.appendChild(label);
  header.appendChild(copyButton(() => cmd, "Copy the command"));

  const pre = document.createElement("pre");
  pre.textContent = cmd;
  div.appendChild(header);
  div.appendChild(pre);
  chatLog.appendChild(div);
  scrollToEnd();
  logToFile(`[needs-admin] ${cmd}`);
}

// The app's own record of what changed, not the model's account of it.
function appendChangedFiles(files) {
  const card = document.createElement("div");
  card.className = "bubble changes-card";
  const heading = document.createElement("div");
  heading.className = "changes-heading";
  heading.textContent = `Changed files (${files.length})`;
  card.appendChild(heading);

  const list = document.createElement("ul");
  list.className = "changes-list";
  const lines = [];
  for (const f of files) {
    const li = document.createElement("li");
    const kind = document.createElement("span");
    kind.className = `change-kind change-${String(f.change).replace(/\s+/g, "-")}`;
    kind.textContent = f.change;
    const path = document.createElement("code");
    path.textContent = f.from ? `${f.from} → ${f.path}` : f.path;
    li.appendChild(kind);
    li.appendChild(path);
    list.appendChild(li);
    lines.push(`${f.change}: ${path.textContent}`);
  }
  card.appendChild(list);
  chatLog.appendChild(card);
  scrollToEnd();
  logToFile(`[changed files]\n${lines.join("\n")}`);
}

const DONE_LABELS = {
  finished: "Done",
  stopped: "Stopped",
  stepCap: "Paused at the step limit",
  denied: "Denied",
  guard: "Stopped (was repeating itself)",
  error: "Failed",
};

// One task's renderer. The "Working…" tracker is created lazily, so a direct
// answer never grows one; steps collapse inside it, the final answer does not.
function createTaskView() {
  let tracker = null;
  let summaryEl = null;
  let steps = 0;
  const rows = new Map();

  const stepWord = () => `${steps} step${steps === 1 ? "" : "s"}`;

  function ensureTracker() {
    if (!tracker) {
      tracker = document.createElement("details");
      tracker.className = "bubble thinking";
      summaryEl = document.createElement("summary");
      tracker.appendChild(summaryEl);
      chatLog.appendChild(tracker);
    }
    steps++;
    summaryEl.textContent = `Working… (${stepWord()})`;
    return tracker;
  }

  function addRow(title, extraClass) {
    const details = document.createElement("details");
    details.className = `bubble output step ${extraClass || ""}`.trim();
    const summary = document.createElement("summary");
    const titleEl = document.createElement("span");
    titleEl.className = "step-title";
    titleEl.textContent = title;
    const badge = document.createElement("span");
    badge.className = "badge";
    summary.appendChild(titleEl);
    summary.appendChild(badge);
    details.appendChild(summary);
    const pre = document.createElement("pre");
    details.appendChild(pre);
    ensureTracker().appendChild(details);
    scrollToEnd();
    return { details, badge, pre };
  }

  function onEvent(ev) {
    switch (ev.type) {
      case "thinking": {
        const row = addRow("Thinking", "step-thinking");
        row.pre.textContent = ev.text;
        logToFile(`[thinking] ${ev.text}`);
        break;
      }
      case "assistantText":
        if (ev.final) {
          appendBubble("assistant", ev.text);
        } else {
          const div = document.createElement("div");
          div.className = "bubble assistant";
          div.innerHTML = renderMarkdown(ev.text);
          ensureTracker().appendChild(div);
          scrollToEnd();
          logToFile(`[assistant, step] ${ev.text}`);
        }
        break;
      case "toolCall": {
        const row = addRow(ev.title);
        row.badge.textContent = "running…";
        rows.set(ev.id, { ...row, title: ev.title });
        logToFile(`[tool] ${ev.title}`);
        break;
      }
      case "toolResult": {
        const row = rows.get(ev.id);
        if (!row) break;
        row.badge.textContent = ev.ok ? "ok" : "error";
        row.details.classList.toggle("step-error", !ev.ok);
        row.pre.textContent = ev.output || "(no output)";
        logToFile(`[result: ${ev.ok ? "ok" : "error"}] ${row.title}\n${ev.output}`);
        break;
      }
      case "confirm":
        showConfirm(ev);
        break;
      case "notice":
        appendBubble("system", ev.text);
        break;
      case "manualCommand":
        appendManualCommand(ev.command);
        break;
      case "changedFiles":
        appendChangedFiles(ev.files);
        break;
      case "error":
        appendBubble("system", `Error: ${ev.message}`);
        break;
      case "done":
        hideConfirm();
        // A step still marked running means the task ended under it.
        for (const row of rows.values()) {
          if (row.badge.textContent === "running…") row.badge.textContent = "interrupted";
        }
        if (summaryEl) {
          summaryEl.textContent = `${DONE_LABELS[ev.reason] ?? "Done"} (${stepWord()})`;
        }
        break;
    }
  }

  return { onEvent };
}

// --- Confirmation dialog ---

const confirmDialog = document.getElementById("confirmDialog");
const confirmTitle = document.getElementById("confirmContext");
const confirmCmd = document.getElementById("confirmCmd");
const confirmDiff = document.getElementById("confirmDiff");
const alwaysAllow = document.getElementById("alwaysAllow");
const alwaysAllowRow = document.getElementById("alwaysAllowRow");
const approveBtn = document.getElementById("approveBtn");
const denyBtn = document.getElementById("denyBtn");

function renderDiff(pre, diff) {
  pre.textContent = "";
  for (const line of diff.split("\n")) {
    const span = document.createElement("span");
    let cls = "diff-ctx";
    if (line.startsWith("+++") || line.startsWith("---")) cls = "diff-head";
    else if (line.startsWith("@@")) cls = "diff-hunk";
    else if (line.startsWith("+")) cls = "diff-add";
    else if (line.startsWith("-")) cls = "diff-del";
    span.className = cls;
    span.textContent = `${line}\n`;
    pre.appendChild(span);
  }
}

function hideConfirm() {
  confirmDialog.hidden = true;
  approveBtn.onclick = null;
  denyBtn.onclick = null;
}

function showConfirm(ev) {
  confirmTitle.textContent = ev.title;
  confirmCmd.hidden = !ev.command;
  confirmCmd.textContent = ev.command || "";
  confirmDiff.hidden = !ev.diff;
  if (ev.diff) renderDiff(confirmDiff, ev.diff);
  alwaysAllowRow.hidden = !ev.canAlwaysAllow;
  alwaysAllow.checked = false;

  const answer = async (decision) => {
    hideConfirm();
    logToFile(`[confirm: ${decision}] ${ev.title}`);
    try {
      await invoke("agent_respond", { id: ev.id, decision });
      if (decision === "always") currentConfig = await invoke("load_config");
    } catch (err) {
      console.error("agent_respond failed:", err);
    }
  };
  approveBtn.onclick = () => answer(alwaysAllow.checked ? "always" : "approve");
  denyBtn.onclick = () => answer("deny");
  focusMainWindow();
  confirmDialog.hidden = false;
}

// --- Controls ---

function setProcessing(running) {
  taskRunning = running;
  sendBtn.textContent = running ? "Stop" : "Send";
  sendBtn.type = running ? "button" : "submit";
  sendBtn.classList.toggle("stop-btn", running);
  sendBtn.disabled = false;
  pickBtn.disabled = running;
  clearChatBtn.disabled = running;
  unmountBtn.disabled = running;
}

pickBtn.addEventListener("click", async () => {
  if (taskRunning) return;
  pickBtn.disabled = true;
  pickBtn.textContent = "Opening picker…";
  try {
    const result = await invoke("pick_and_set_root");
    rootPath.textContent = result.root;
    currentConfig = result.config;
    chatLog.innerHTML = "";
    appendBubble("system", `Working in: ${result.root}`);
  } catch (e) {
    console.error("pick_and_set_root failed:", e);
    appendBubble("system", `Error: ${e}`);
  } finally {
    pickBtn.disabled = taskRunning;
    pickBtn.textContent = pickBtnDefaultLabel;
  }
});

clearChatBtn.addEventListener("click", async () => {
  if (taskRunning) return;
  chatLog.innerHTML = "";
  // Forgets the backend's conversation and session record too: wiping only
  // the visible chat would let the next message inherit the old one.
  try {
    await invoke("agent_reset");
  } catch (err) {
    console.error("agent_reset failed:", err);
  }
  logToFile("[system] --- chat cleared ---");
  if (rootPath.textContent && rootPath.textContent !== NO_FOLDER_LABEL) {
    appendBubble("system", `Working in: ${rootPath.textContent}`);
  }
});

unmountBtn.addEventListener("click", async () => {
  if (taskRunning) return;
  try {
    const old = await invoke("unmount_root");
    rootPath.textContent = NO_FOLDER_LABEL;
    if (old) {
      appendBubble("system", `Unmounted ${old} — chat-only mode now.`);
    }
  } catch (err) {
    console.error("unmount_root failed:", err);
  }
});

// Enter submits, Shift+Enter newlines. Ignored mid-task so Enter can't queue.
chatInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    if (!taskRunning) chatForm.requestSubmit();
  }
});

// Send turns into Stop while a task runs.
sendBtn.addEventListener("click", async () => {
  if (!taskRunning) return;
  hideConfirm();
  try {
    await invoke("agent_stop");
  } catch (err) {
    console.error("agent_stop failed:", err);
  }
});

chatForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  if (taskRunning) return;
  const text = chatInput.value.trim();
  if (!text) return;
  if (rootPath.textContent === NO_FOLDER_LABEL) {
    appendBubble("system", "Select a folder first, so I know where I'm allowed to work.");
    return;
  }
  // Before the first await: a second submit must not slip in.
  setProcessing(true);
  chatInput.value = "";
  appendBubble("user", text);
  // The backend re-reads config every task; currentConfig is only a display cache.
  try {
    currentConfig = await invoke("load_config");
  } catch (err) {
    console.error("load_config (pre-task refresh) failed:", err);
  }

  const view = createTaskView();
  const channel = new window.__TAURI__.core.Channel();
  await new Promise((resolve) => {
    // Done only counts once the command has also returned, so the backend has
    // released the task and an immediate next send can't hit "already running".
    let doneSeen = false;
    let returned = false;
    const maybeFinish = () => {
      if (doneSeen && returned) resolve();
    };
    channel.onmessage = (ev) => {
      view.onEvent(ev);
      if (ev.type === "done") {
        doneSeen = true;
        maybeFinish();
      }
    };
    invoke("agent_send", { message: text, onEvent: channel })
      .then(() => {
        returned = true;
        maybeFinish();
        // The command can resolve before its last channel message arrives.
        setTimeout(resolve, 1500);
      })
      .catch((err) => {
        appendBubble("system", `Error: ${err}`);
        resolve();
      });
  });
  hideConfirm();
  setProcessing(false);
});
