const { invoke } = window.__TAURI__.core;

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

function appendBubble(role, text) {
  const div = document.createElement("div");
  div.className = `bubble ${role}`;
  div.textContent = text;
  chatLog.appendChild(div);
  chatLog.scrollTop = chatLog.scrollHeight;
  return div;
}

function appendOutput(cmd, outcome) {
  const div = document.createElement("div");
  div.className = "bubble output";
  const status = outcome.exit_code === 0 ? "ok" : `exit ${outcome.exit_code}`;
  div.innerHTML = `<div class="output-cmd">$ ${escapeHtml(cmd)}  <span class="badge">${status}</span></div>`;
  const pre = document.createElement("pre");
  pre.textContent = [outcome.stdout, outcome.stderr].filter(Boolean).join("\n").trim() || "(no output)";
  div.appendChild(pre);
  chatLog.appendChild(div);
  chatLog.scrollTop = chatLog.scrollHeight;
}

function extractCommand(text) {
  const match = text.match(/```(?:sh|bash|shell)?\n([\s\S]*?)```/);
  return match ? match[1].trim() : null;
}

document.getElementById("pickBtn").addEventListener("click", async () => {
  try {
    const result = await invoke("pick_and_set_root");
    rootPath.textContent = result.root;
    currentConfig = result.config;
    chatInput.disabled = false;
    sendBtn.disabled = false;
    history = [];
    chatLog.innerHTML = "";
    appendBubble("system", `Working in: ${result.root}`);
  } catch (e) {
    appendBubble("system", `Error: ${e}`);
  }
});

chatForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const text = chatInput.value.trim();
  if (!text) return;
  chatInput.value = "";
  appendBubble("user", text);
  history.push({ role: "user", content: text });
  sendBtn.disabled = true;
  try {
    const reply = await invoke("send_message", { history });
    history.push({ role: "assistant", content: reply });
    appendBubble("assistant", reply);
    const cmd = extractCommand(reply);
    if (cmd) {
      await handleProposedCommand(cmd);
    }
  } catch (err) {
    appendBubble("system", `Error: ${err}`);
  } finally {
    sendBtn.disabled = false;
  }
});

async function handleProposedCommand(cmd) {
  const info = await invoke("classify_command", { cmd });
  if (info.classification === "ReadOnly" || info.auto_approved) {
    await executeCommand(cmd);
    return;
  }
  await requestApproval(cmd);
}

function requestApproval(cmd) {
  return new Promise((resolve) => {
    const dialog = document.getElementById("confirmDialog");
    const denyBtn = document.getElementById("denyBtn");
    const approveBtn = document.getElementById("approveBtn");
    const alwaysAllow = document.getElementById("alwaysAllow");

    document.getElementById("confirmCmd").textContent = cmd;
    alwaysAllow.checked = false;
    dialog.showModal();

    const cleanup = () => {
      denyBtn.onclick = null;
      approveBtn.onclick = null;
      dialog.close();
    };

    denyBtn.onclick = () => {
      cleanup();
      appendBubble("system", "Command denied.");
      history.push({ role: "user", content: "[the user denied that command]" });
      resolve();
    };

    approveBtn.onclick = async () => {
      const always = alwaysAllow.checked;
      cleanup();
      if (always) {
        const bin = cmd.trim().split(/\s+/)[0].split("/").pop();
        currentConfig = await invoke("add_auto_approve", { binary: bin });
      }
      await executeCommand(cmd);
      resolve();
    };
  });
}

async function executeCommand(cmd) {
  try {
    const outcome = await invoke("run_command", { cmd });
    appendOutput(cmd, outcome);
    const summary = [outcome.stdout, outcome.stderr].filter(Boolean).join("\n").trim();
    history.push({
      role: "user",
      content: `[command output, exit ${outcome.exit_code}]\n${summary || "(no output)"}`,
    });
  } catch (err) {
    appendBubble("system", `Execution error: ${err}`);
  }
}

// --- Settings ---

const settingsDialog = document.getElementById("settingsDialog");

document.getElementById("settingsBtn").addEventListener("click", async () => {
  try {
    currentConfig = await invoke("load_config");
  } catch (err) {
    appendBubble("system", "Select a folder first.");
    return;
  }
  renderSettings();
  settingsDialog.showModal();
});

function renderSettings() {
  document.getElementById("cfgEndpoint").value = currentConfig.endpoint;
  document.getElementById("cfgModel").value = currentConfig.model;
  document.getElementById("cfgTemperature").value = currentConfig.temperature;
  document.getElementById("cfgSystemPrompt").value = currentConfig.system_prompt;

  const list = document.getElementById("grantedList");
  list.innerHTML = "";
  for (const g of currentConfig.granted_paths) {
    const li = document.createElement("li");
    const label = document.createElement("span");
    label.textContent = `${g.path} (${g.read_write ? "rw" : "ro"}) — ${g.note}`;
    const rm = document.createElement("button");
    rm.type = "button";
    rm.textContent = "remove";
    rm.onclick = async () => {
      currentConfig = await invoke("remove_granted_path", { path: g.path });
      renderSettings();
    };
    li.appendChild(label);
    li.appendChild(rm);
    list.appendChild(li);
  }

  document.getElementById("autoApproveList").textContent =
    currentConfig.auto_approve.length > 0
      ? currentConfig.auto_approve.join(", ")
      : "(none yet — approve a command with the checkbox to add one)";
}

document.getElementById("grantAddBtn").addEventListener("click", async () => {
  const path = document.getElementById("grantPathInput").value.trim();
  const note = document.getElementById("grantNoteInput").value.trim();
  if (!path) return;
  currentConfig = await invoke("add_granted_path", { path, note, readWrite: false });
  document.getElementById("grantPathInput").value = "";
  document.getElementById("grantNoteInput").value = "";
  renderSettings();
});

document.getElementById("settingsSaveBtn").addEventListener("click", async () => {
  currentConfig.endpoint = document.getElementById("cfgEndpoint").value;
  currentConfig.model = document.getElementById("cfgModel").value;
  currentConfig.temperature = parseFloat(document.getElementById("cfgTemperature").value) || 0;
  currentConfig.system_prompt = document.getElementById("cfgSystemPrompt").value;
  await invoke("save_config", { cfg: currentConfig });
  settingsDialog.close();
});

document.getElementById("settingsCloseBtn").addEventListener("click", () => {
  settingsDialog.close();
});
