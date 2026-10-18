// Settings dialog and its tabs (General/Chat/File Operations, ComfyUI mapping, SearXNG).

// --- Settings ---

const settingsDialog = document.getElementById("settingsDialog");
let currentGeneralRules = "";
let currentCommandRules = "";
let currentComfyConfig = null;
let currentSearxngConfig = null;


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
    unload_llm_during_generation: document.getElementById("cfgComfyUnloadLlm").checked,
    free_after_generation: document.getElementById("cfgComfyFreeAfter").checked,
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
  document.getElementById("cfgToolCalling").value = currentConfig.tool_calling || "auto";
  document.getElementById("cfgCommandTimeoutSecs").value = currentConfig.command_timeout_secs ?? 120;
  document.getElementById("cfgMaxContextTokens").value = currentConfig.max_context_tokens;
  document.getElementById("cfgConfirmFadeAfter").value = currentConfig.confirm_fade_after;
  document.getElementById("cfgMemoryEnabled").checked = !!currentConfig.memory_enabled;
  document.getElementById("cfgMemoryMaxTokens").value = currentConfig.memory_max_tokens;
  document.getElementById("cfgSandboxTrashDir").value = currentConfig.sandbox_trash_dir || "";
  document.getElementById("cfgChatStateMaxTokens").value = currentConfig.chat_state_max_tokens;
  document.getElementById("cfgChatShowThinking").checked = !!currentConfig.chat_show_thinking;
  document.getElementById("cfgChatPersistThinking").checked = !!currentConfig.chat_persist_thinking;
  document.getElementById("cfgChatAutoImages").checked = currentConfig.chat_auto_images ?? true;
  document.getElementById("cfgChatStreamReplies").checked = !!currentConfig.chat_stream_replies;
  document.getElementById("cfgChatRecall").checked = currentConfig.chat_recall ?? true;
  document.getElementById("cfgChatRecallPersona").checked =
    currentConfig.chat_recall_scope === "persona";
  document.getElementById("cfgChatRecallEmbeddingModel").value =
    currentConfig.chat_recall_embedding_model || "";
  document.getElementById("cfgChatRecallMax").value = currentConfig.chat_recall_max ?? 4;
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
  document.getElementById("cfgComfyUnloadLlm").checked = !!currentComfyConfig.unload_llm_during_generation;
  document.getElementById("cfgComfyFreeAfter").checked = !!currentComfyConfig.free_after_generation;
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
    setIconText(el, "triangle-alert", `Your endpoint is ${host}, which is not this machine. Anything the assistant reads from this folder will be sent there.`);
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
  const saveResult = document.getElementById("settingsSaveResult");
  saveResult.textContent = "";
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
  currentConfig.tool_calling = document.getElementById("cfgToolCalling").value;
  currentConfig.command_timeout_secs = Math.max(
    1,
    parseInt(document.getElementById("cfgCommandTimeoutSecs").value, 10) || 120,
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
  currentConfig.sandbox_trash_dir = document.getElementById("cfgSandboxTrashDir").value.trim();
  currentConfig.chat_state_max_tokens = Math.max(
    0,
    parseInt(document.getElementById("cfgChatStateMaxTokens").value, 10) || 0,
  );
  currentConfig.chat_show_thinking = document.getElementById("cfgChatShowThinking").checked;
  currentConfig.chat_persist_thinking = document.getElementById("cfgChatPersistThinking").checked;
  currentConfig.chat_auto_images = document.getElementById("cfgChatAutoImages").checked;
  currentConfig.chat_stream_replies = document.getElementById("cfgChatStreamReplies").checked;
  currentConfig.chat_recall = document.getElementById("cfgChatRecall").checked;
  currentConfig.chat_recall_scope = document.getElementById("cfgChatRecallPersona").checked
    ? "persona"
    : "session";
  currentConfig.chat_recall_embedding_model = document
    .getElementById("cfgChatRecallEmbeddingModel")
    .value.trim();
  currentConfig.chat_recall_max = Math.min(
    10,
    Math.max(1, parseInt(document.getElementById("cfgChatRecallMax").value, 10) || 4),
  );
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
  try {
    await Promise.all(saves);
  } catch (err) {
    console.error("settings save failed:", err);
    saveResult.textContent = `Save failed (some settings may not have been saved): ${err}`;
    return;
  }
  updateNarrationToggleBtn();
  refreshImageCapability(); // ComfyUI setup may have changed
  if (chatModeView && !chatModeView.hidden) renderChatHistoryLog();
  settingsDialog.hidden = true;
});

document.getElementById("settingsCloseBtn").addEventListener("click", () => {
  document.getElementById("settingsSaveResult").textContent = "";
  settingsDialog.hidden = true;
});
