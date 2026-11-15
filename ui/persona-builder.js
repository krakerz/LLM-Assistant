// Persona builder: a dialog that turns a short brief into an editable persona
// card, one section per backend call. The draft lives here and is sent back
// with every call; the backend is stateless apart from its cancel registry,
// keyed by `buildId`. Wire shapes: see the persona_builder_* commands in
// src/chat_commands.rs (struct fields and enum values are snake_case; Tauri
// camelCases the top-level argument names, e.g. `buildId`, `characterTag`).
//
// Everything below runs inside one function so none of its names leak into
// the global scope the other classic scripts share. Model and web text only
// ever goes in through `value` / `textContent` / DOM nodes, never innerHTML.
(function () {
  const SECTIONS = ["identity", "appearance", "wardrobe", "personality", "photo_habit", "stats", "rules"];
  // Section -> the Draft field its outcome value belongs to.
  const SECTION_FIELD = {
    identity: "identity",
    appearance: "appearance",
    wardrobe: "wardrobe",
    personality: "personality",
    photo_habit: "photo_reason",
    stats: "extra_stats",
    rules: "rules",
  };
  const STATUS_LABEL = {
    verified: "verified",
    alias: "alias replaced",
    online: "found online",
    unknown: "unknown",
    unverified: "not checked",
  };

  const el = (id) => document.getElementById(id);
  const dialog = el("personaBuilderDialog");

  const pb = {
    open: false,
    openToken: null,
    mode: "create", // "create" | "update"
    persona: "",
    buildId: null,
    running: false,
    stopRequested: false,
    snippets: [],
    charTag: "",
    dirty: new Set(), // sections the user edited by hand since their last fill
    touched: false, // anything worth a "discard?" prompt
    manual: false, // the preview text was edited by hand
    previewText: "",
    status: { tag_source: "none", online_lookup: false, web_lookup: false, test_photo: false },
    seq: { find: 0, appearance: 0, wardrobe: 0 },
    checkTimers: {},
    pendingLoad: null, // SavedBuild offered by the "Edit with builder" button
  };

  function newBuildId() {
    if (window.crypto && typeof crypto.randomUUID === "function") return crypto.randomUUID();
    // randomUUID needs a secure context; --server over plain http on a LAN isn't one.
    return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 12)}`;
  }

  // A result only counts while the dialog is open and the build it belongs to is current.
  function isCurrent(buildId) {
    return pb.open && pb.buildId === buildId;
  }

  // ---- small form helpers ----

  function splitLines(text) {
    return text
      .split("\n")
      .map((l) => l.trim())
      .filter(Boolean);
  }

  // Tags never contain commas, so both separators work for a tag list.
  function splitTags(text) {
    return text
      .split(/[\n,]/)
      .map((t) => t.trim())
      .filter(Boolean);
  }

  function splitNamed(line) {
    const i = line.indexOf(":");
    if (i < 0) return [line.trim(), ""];
    return [line.slice(0, i).trim(), line.slice(i + 1).trim()];
  }

  function parseWardrobe(text) {
    return splitLines(text).map((line) => {
      const [name, rest] = splitNamed(line);
      return { name, items: splitTags(rest) };
    });
  }

  function formatWardrobe(outfits) {
    return (outfits || [])
      .map((o) => (o.items && o.items.length ? `${o.name}: ${o.items.join(", ")}` : o.name))
      .join("\n");
  }

  function parseStats(text) {
    return splitLines(text).map((line) => {
      const [name, description] = splitNamed(line);
      return { name, description };
    });
  }

  function formatStats(stats) {
    return (stats || [])
      .map((s) => (s.description ? `${s.name}: ${s.description}` : s.name))
      .join("\n");
  }

  function isOriginal(source) {
    const s = source.trim().toLowerCase();
    return s === "" || s === "original";
  }

  function showError(msg) {
    const p = el("pbError");
    p.textContent = msg || "";
    p.hidden = !msg;
  }

  // ---- brief / draft <-> form ----

  function collectBrief() {
    return {
      name: el("pbName").value.trim(),
      source: el("pbSource").value.trim(),
      vibe: el("pbVibe").value.trim(),
      content_level: el("pbContentLevel").value,
      photo_habit: el("pbPhotoHabit").value,
      appearance_style: el("pbStyle").value,
    };
  }

  function fillBrief(brief) {
    el("pbName").value = brief.name || "";
    el("pbSource").value = brief.source || "";
    el("pbVibe").value = brief.vibe || "";
    el("pbContentLevel").value = brief.content_level || "suggestive";
    el("pbPhotoHabit").value = brief.photo_habit || "normal";
    el("pbStyle").value = brief.appearance_style || "tags";
  }

  function collectDraft() {
    return {
      identity: {
        summary: el("pbIdSummary").value.trim(),
        gender: el("pbIdGender").value,
        speaks_as: el("pbIdSpeaks").value.trim(),
        calls_user: el("pbIdCalls").value.trim(),
        short_name: el("pbIdShort").value.trim(),
      },
      appearance: {
        character_tag: pb.charTag,
        tags: splitTags(el("pbApTags").value),
        description: el("pbApDesc").value.trim(),
      },
      wardrobe: parseWardrobe(el("pbWardrobe").value),
      personality: splitLines(el("pbPersonality").value),
      photo_reason: el("pbPhotoReason").value.trim(),
      extra_stats: parseStats(el("pbStats").value),
      rules: {
        mood: el("pbRuleMood").value.trim(),
        affection_mid: el("pbRuleMid").value.trim(),
        affection_high: el("pbRuleHigh").value.trim(),
        extra: splitLines(el("pbRuleExtra").value),
      },
    };
  }

  // Writes one Draft field's value into its section's form fields.
  function fillSection(section, value) {
    switch (section) {
      case "identity": {
        const v = value || {};
        el("pbIdSummary").value = v.summary || "";
        el("pbIdGender").value = v.gender || "female";
        el("pbIdSpeaks").value = v.speaks_as || "";
        el("pbIdCalls").value = v.calls_user || "";
        el("pbIdShort").value = v.short_name || "";
        break;
      }
      case "appearance": {
        const v = value || {};
        if (v.character_tag) {
          pb.charTag = v.character_tag;
          el("pbCharTag").value = v.character_tag;
        }
        el("pbApTags").value = (v.tags || []).join("\n");
        el("pbApDesc").value = v.description || "";
        break;
      }
      case "wardrobe":
        el("pbWardrobe").value = formatWardrobe(value);
        break;
      case "personality":
        el("pbPersonality").value = (value || []).join("\n");
        break;
      case "photo_habit":
        el("pbPhotoReason").value = typeof value === "string" ? value : "";
        break;
      case "stats":
        el("pbStats").value = formatStats(value);
        break;
      case "rules": {
        const v = value || {};
        el("pbRuleMood").value = v.mood || "";
        el("pbRuleMid").value = v.affection_mid || "";
        el("pbRuleHigh").value = v.affection_high || "";
        el("pbRuleExtra").value = (v.extra || []).join("\n");
        break;
      }
    }
  }

  function fillDraft(draft) {
    pb.charTag = (draft.appearance && draft.appearance.character_tag) || "";
    el("pbCharTag").value = pb.charTag;
    for (const s of SECTIONS) fillSection(s, draft[SECTION_FIELD[s]]);
  }

  // ---- per-section notes, spinners, chips ----

  function setNote(section, text, warn, title) {
    const n = document.querySelector(`[data-note="${section}"]`);
    n.textContent = text || "";
    n.title = title || "";
    n.classList.toggle("warn", !!warn);
  }

  function setSpinner(section, on) {
    for (const s of SECTIONS) {
      document.querySelector(`[data-spinner="${s}"]`).hidden = !(on && s === section);
    }
  }

  function renderChips(containerId, checks) {
    const box = el(containerId);
    box.textContent = "";
    for (const c of checks || []) {
      const chip = document.createElement("span");
      chip.className = `pb-chip ${c.status}`;
      const label = STATUS_LABEL[c.status] || c.status;
      chip.textContent = c.status === "verified" ? c.display : `${c.display} · ${label}`;
      chip.title = c.canonical ? `${c.canonical} (${label})` : label;
      box.appendChild(chip);
    }
  }

  function applyStyle() {
    const tags = el("pbStyle").value === "tags";
    el("pbApTagsLabel").hidden = !tags;
    el("pbApChips").hidden = !tags;
    el("pbApDescLabel").hidden = tags;
    el("pbWardChips").hidden = !tags;
    el("pbCharRow").hidden = !tags;
    el("pbTagSource").hidden = !tags;
  }

  function tagSourceText(status) {
    const base =
      status.tag_source === "folder"
        ? "Tags are checked against your tag folder"
        : status.tag_source === "bundled"
          ? "Tags are checked against the built-in tag list"
          : status.online_lookup
            ? "Tags are only checked online"
            : "Tags are not checked";
    return status.online_lookup && status.tag_source !== "none"
      ? `${base}, unknown ones online`
      : base;
  }

  // ---- tag checks ----

  async function runCheck(kind, replaceAliases) {
    const field = kind === "appearance" ? el("pbApTags") : el("pbWardrobe");
    const chipsId = kind === "appearance" ? "pbApChips" : "pbWardChips";
    const tags =
      kind === "appearance"
        ? splitTags(field.value)
        : parseWardrobe(field.value).flatMap((o) => o.items);
    const seq = ++pb.seq[kind];
    if (el("pbStyle").value !== "tags" || tags.length === 0) {
      renderChips(chipsId, []);
      return;
    }
    try {
      const res = await invoke("persona_builder_check_tags", { tags });
      if (!pb.open || seq !== pb.seq[kind]) return;
      renderChips(chipsId, res.checks);
      if (replaceAliases && document.activeElement !== field) {
        const byInput = new Map(
          res.checks.filter((c) => c.status === "alias").map((c) => [c.input, c.display]),
        );
        if (byInput.size) {
          if (kind === "appearance") {
            field.value = splitTags(field.value)
              .map((t) => byInput.get(t) || t)
              .join("\n");
          } else {
            field.value = formatWardrobe(
              parseWardrobe(field.value).map((o) => ({
                name: o.name,
                items: o.items.map((t) => byInput.get(t) || t),
              })),
            );
          }
        }
      }
    } catch (err) {
      if (!pb.open || seq !== pb.seq[kind]) return;
      renderChips(chipsId, []);
      console.error("persona_builder_check_tags failed:", err);
    }
  }

  function scheduleCheck(kind) {
    clearTimeout(pb.checkTimers[kind]);
    pb.checkTimers[kind] = setTimeout(() => runCheck(kind, false), 600);
  }

  // ---- character tag lookup ----

  function renderCandidates(candidates) {
    const box = el("pbCharChips");
    box.textContent = "";
    for (const c of candidates) {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "pb-chip";
      b.textContent = c.name;
      b.title = `${c.count} posts`;
      b.classList.toggle("selected", c.name === pb.charTag);
      b.addEventListener("click", () => chooseCharacter(c.name));
      box.appendChild(b);
    }
  }

  async function findCharacter() {
    if (pb.mode === "update") return; // the draft already has its tag
    const brief = collectBrief();
    const seq = ++pb.seq.find;
    el("pbCharChips").textContent = "";
    el("pbCharNote").textContent = "";
    if (brief.appearance_style !== "tags" || !brief.name || isOriginal(brief.source)) return;
    try {
      const res = await invoke("persona_builder_find_character", {
        name: brief.name,
        source: brief.source,
      });
      if (!pb.open || seq !== pb.seq.find) return;
      renderCandidates(res.candidates || []);
      el("pbCharNote").textContent =
        res.source === "none" && !(res.candidates || []).length
          ? "Tags are not checked, so no character tag was looked up."
          : (res.candidates || []).length
            ? "Pick the character's tag, or type one."
            : "No matching tag found. Type one if you know it, or leave it empty.";
    } catch (err) {
      if (!pb.open || seq !== pb.seq.find) return;
      el("pbCharNote").textContent = `Character lookup failed: ${err}`;
    }
  }

  async function chooseCharacter(tag) {
    pb.charTag = tag;
    pb.touched = true;
    el("pbCharTag").value = tag;
    for (const b of el("pbCharChips").querySelectorAll("button")) {
      b.classList.toggle("selected", b.textContent === tag);
    }
    if (!tag) return;
    const seq = ++pb.seq.find;
    try {
      const s = await invoke("persona_builder_suggest", { characterTag: tag });
      if (!pb.open || seq !== pb.seq.find || pb.charTag !== tag || !s) return;
      if (s.character && s.character.name) {
        pb.charTag = s.character.name;
        el("pbCharTag").value = s.character.name;
      }
      if (s.body_tags && s.body_tags.length) {
        el("pbApTags").value = s.body_tags.join("\n");
        pb.dirty.delete("appearance");
        setNote("appearance", "from tag data", false);
        runCheck("appearance", false);
      }
      if (s.outfits && s.outfits.length) {
        el("pbWardrobe").value = formatWardrobe(s.outfits);
        pb.dirty.delete("wardrobe");
        setNote("wardrobe", "from tag data", false);
        runCheck("wardrobe", false);
      }
    } catch (err) {
      console.error("persona_builder_suggest failed:", err);
    }
  }

  // ---- running state ----

  function updateRunUi() {
    const gen = el("pbGenerateBtn");
    gen.textContent = pb.running ? "Stop" : "Generate";
    gen.disabled = false;
    for (const b of dialog.querySelectorAll(".pb-regen")) b.disabled = pb.running || pb.manual;
    el("pbSaveBtn").disabled = pb.running;
    el("pbPreviewBtn").disabled = pb.running || pb.manual;
    el("pbTestPhotoBtn").disabled = pb.running;
  }

  // Starts a run; returns its buildId, or null when one is already going.
  function beginRun() {
    if (pb.running) return null;
    pb.buildId = newBuildId();
    pb.running = true;
    pb.stopRequested = false;
    showError("");
    updateRunUi();
    return pb.buildId;
  }

  function endRun(buildId) {
    if (pb.buildId !== buildId) return;
    pb.running = false;
    setSpinner(null, false);
    updateRunUi();
  }

  async function requestStop() {
    if (!pb.running || !pb.buildId) return;
    pb.stopRequested = true;
    try {
      await invoke("stop_persona_builder", { buildId: pb.buildId });
    } catch (err) {
      console.error("stop_persona_builder failed:", err);
    }
  }

  // ---- generation ----

  function applyOutcome(out) {
    const section = out.section;
    const field = SECTION_FIELD[section];
    if (!field) return;
    fillSection(section, out.value);
    pb.dirty.delete(section);
    pb.touched = true;
    if (out.fallback) {
      setNote(section, "used a default", true, out.warning || "");
    } else if (out.warning) {
      setNote(section, out.warning, true, out.warning);
    } else {
      setNote(section, "", false);
    }
    if (section === "appearance") {
      pb.seq.appearance++;
      renderChips("pbApChips", el("pbStyle").value === "tags" ? out.tag_checks : []);
    } else if (section === "wardrobe") {
      pb.seq.wardrobe++;
      renderChips("pbWardChips", el("pbStyle").value === "tags" ? out.tag_checks : []);
    }
  }

  // Generates one section. Returns false when the run should end.
  async function runSection(buildId, brief, section) {
    setSpinner(section, true);
    const out = await invoke("persona_builder_generate_section", {
      buildId,
      brief,
      section,
      draft: collectDraft(),
      snippets: pb.snippets,
    });
    if (!isCurrent(buildId)) return false;
    setSpinner(section, false);
    if (out.stopped) return false;
    applyOutcome(out);
    return !pb.stopRequested;
  }

  function renderSnippets() {
    const box = el("pbResearch");
    box.textContent = "";
    if (!pb.snippets.length) {
      box.hidden = true;
      return;
    }
    box.appendChild(document.createTextNode("Web notes used:"));
    const ul = document.createElement("ul");
    for (const s of pb.snippets) {
      const li = document.createElement("li");
      if (/^https?:\/\//i.test(s.url)) {
        const a = document.createElement("a");
        a.href = s.url;
        a.rel = "noopener noreferrer";
        a.target = "_blank";
        a.textContent = s.title || s.url;
        li.appendChild(a);
      } else {
        li.textContent = s.title || s.url;
      }
      ul.appendChild(li);
    }
    box.appendChild(ul);
    box.hidden = false;
  }

  async function generateAll() {
    const brief = collectBrief();
    if (!brief.name) {
      showError("Enter a name first.");
      return;
    }
    const buildId = beginRun();
    if (!buildId) return;
    try {
      pb.snippets = [];
      renderSnippets();
      if (!el("pbWebRow").hidden && el("pbWeb").checked && !isOriginal(brief.source)) {
        const snippets = await invoke("persona_builder_research", { buildId, brief });
        if (!isCurrent(buildId)) return;
        pb.snippets = snippets || [];
        renderSnippets();
      }
      for (const section of SECTIONS) {
        if (pb.stopRequested) break;
        if (pb.dirty.has(section)) continue;
        if (!(await runSection(buildId, brief, section))) break;
      }
    } catch (err) {
      if (isCurrent(buildId)) showError(`Generation failed: ${err}`);
    } finally {
      endRun(buildId);
    }
  }

  async function regenerate(section) {
    const brief = collectBrief();
    if (!brief.name) {
      showError("Enter a name first.");
      return;
    }
    const buildId = beginRun();
    if (!buildId) return;
    try {
      await runSection(buildId, brief, section);
    } catch (err) {
      if (isCurrent(buildId)) showError(`Generation failed: ${err}`);
    } finally {
      endRun(buildId);
    }
  }

  // ---- preview / lock ----

  function setLocked(locked) {
    dialog.querySelector(".pb-sections").classList.toggle("pb-locked", locked);
    for (const node of dialog.querySelectorAll(".pb-sections input, .pb-sections textarea, .pb-sections select, .pb-sections button")) {
      node.disabled = locked;
    }
    el("pbBackToFormBtn").hidden = !locked;
    el("pbPreviewNote").hidden = !locked;
    updateRunUi();
  }

  async function renderPreview() {
    const brief = collectBrief();
    if (!brief.name) throw "Enter a name first.";
    return invoke("persona_builder_render", { brief, draft: collectDraft() });
  }

  async function showPreview() {
    showError("");
    try {
      const text = await renderPreview();
      if (!pb.open) return;
      pb.previewText = text;
      el("pbPreview").value = text;
      el("pbPreview").hidden = false;
    } catch (err) {
      showError(`Preview failed: ${err}`);
    }
  }

  // ---- test photo ----

  async function testPhoto() {
    const brief = collectBrief();
    if (!brief.name) {
      showError("Enter a name first.");
      return;
    }
    const buildId = beginRun();
    if (!buildId) return;
    const btn = el("pbTestPhotoBtn");
    const result = el("pbTestPhotoResult");
    const img = el("pbTestPhotoImg");
    btn.disabled = true;
    result.hidden = false;
    result.classList.remove("warn");
    result.textContent = "Generating… this can take a while.";
    img.hidden = true;
    try {
      const draft = collectDraft();
      const dataUrl = await invoke("persona_builder_test_photo", { buildId, brief, draft });
      if (!isCurrent(buildId)) return;
      if (pb.stopRequested) {
        result.textContent = "Stopped.";
      } else {
        result.textContent = "Generated successfully:";
        img.src = dataUrl;
        img.hidden = false;
      }
    } catch (err) {
      if (isCurrent(buildId)) {
        result.textContent = pb.stopRequested ? "Stopped." : `Failed: ${err}`;
        result.classList.toggle("warn", !pb.stopRequested);
      }
    } finally {
      endRun(buildId);
    }
  }

  // ---- save ----

  async function save() {
    if (pb.running) return;
    const brief = collectBrief();
    if (!brief.name) {
      showError("Enter a name first.");
      return;
    }
    showError("");
    try {
      const draft = collectDraft();
      const content = pb.manual ? el("pbPreview").value : await renderPreview();
      const persona = pb.mode === "update" ? pb.persona : brief.name;
      const summary = await invoke("persona_builder_save", {
        mode: pb.mode,
        persona,
        brief,
        draft,
        content,
      });
      if (!pb.open) return;
      await loadPersonaList();
      personaSelect.value = summary.name;
      closeBuilder(true);
    } catch (err) {
      // "Already exists" and similar stay in the dialog.
      showError(`Save failed: ${err}`);
    }
  }

  // ---- open / close ----

  function resetForm() {
    fillBrief({});
    el("pbName").disabled = false;
    el("pbWeb").checked = false;
    el("pbCharChips").textContent = "";
    el("pbCharNote").textContent = "";
    for (const s of SECTIONS) {
      fillSection(s, undefined);
      setNote(s, "", false);
    }
    setSpinner(null, false);
    renderChips("pbApChips", []);
    renderChips("pbWardChips", []);
    pb.snippets = [];
    renderSnippets();
    el("pbPreview").value = "";
    el("pbPreview").hidden = true;
    el("pbTestPhotoResult").hidden = true;
    el("pbTestPhotoImg").hidden = true;
    el("pbTestPhotoImg").removeAttribute("src");
    showError("");
    setLocked(false);
  }

  async function loadStatus(buildForOpen) {
    let status = { tag_source: "none", online_lookup: false, web_lookup: false, test_photo: false };
    try {
      status = await invoke("persona_builder_status");
    } catch (err) {
      console.error("persona_builder_status failed:", err);
    }
    if (!pb.open || pb.openToken !== buildForOpen) return;
    pb.status = status;
    el("pbWebRow").hidden = !status.web_lookup;
    el("pbTestPhotoBtn").hidden = !status.test_photo;
    el("pbTagSource").textContent = tagSourceText(status);
  }

  function openBuilder(saved) {
    pb.open = true;
    pb.openToken = newBuildId();
    pb.buildId = null;
    pb.running = false;
    pb.stopRequested = false;
    pb.dirty = new Set();
    pb.touched = false;
    pb.manual = false;
    pb.previewText = "";
    pb.charTag = "";
    resetForm();
    if (saved) {
      pb.mode = "update";
      pb.persona = saved.persona;
      fillBrief(saved.brief || {});
      fillDraft(saved.draft || {});
      el("pbName").disabled = true;
      el("pbName").value = saved.persona;
    } else {
      pb.mode = "create";
      pb.persona = "";
    }
    applyStyle();
    updateRunUi();
    focusMainWindow();
    dialog.hidden = false;
    loadStatus(pb.openToken);
    if (saved) {
      runCheck("appearance", false);
      runCheck("wardrobe", false);
    }
  }

  function closeBuilder(force) {
    if (!pb.open) return;
    if (!force && pb.touched && !confirm("Discard this persona draft?")) return;
    if (pb.running) requestStop();
    pb.open = false;
    pb.buildId = null;
    pb.running = false;
    for (const k of Object.keys(pb.checkTimers)) clearTimeout(pb.checkTimers[k]);
    dialog.hidden = true;
  }

  // ---- wiring ----

  for (const b of dialog.querySelectorAll(".pb-regen")) {
    setIcon(b, "refresh-cw", 14);
    b.addEventListener("click", () => regenerate(b.dataset.regen));
  }

  el("buildPersonaBtn").addEventListener("click", () => openBuilder(null));
  el("pbCancelBtn").addEventListener("click", () => closeBuilder(false));
  el("pbGenerateBtn").addEventListener("click", () => {
    if (pb.running) requestStop();
    else generateAll();
  });
  el("pbSaveBtn").addEventListener("click", save);
  el("pbPreviewBtn").addEventListener("click", showPreview);
  el("pbTestPhotoBtn").addEventListener("click", testPhoto);

  el("pbPreview").addEventListener("input", () => {
    pb.touched = true;
    if (!pb.manual && el("pbPreview").value !== pb.previewText) {
      pb.manual = true;
      setLocked(true);
    }
  });
  el("pbBackToFormBtn").addEventListener("click", () => {
    pb.manual = false;
    pb.previewText = "";
    el("pbPreview").value = "";
    el("pbPreview").hidden = true;
    setLocked(false);
  });

  el("pbStyle").addEventListener("change", () => {
    applyStyle();
    runCheck("appearance", false);
    runCheck("wardrobe", false);
    findCharacter();
  });
  el("pbName").addEventListener("change", findCharacter);
  el("pbSource").addEventListener("change", findCharacter);
  el("pbCharTag").addEventListener("change", () => {
    const tag = el("pbCharTag").value.trim().replace(/\s+/g, "_");
    el("pbCharTag").value = tag;
    chooseCharacter(tag);
  });

  // Hand edits mark their section so "Generate" leaves it alone.
  for (const node of dialog.querySelectorAll(".pb-section input, .pb-section textarea, .pb-section select")) {
    const section = node.closest(".pb-section").dataset.section;
    const handler = () => {
      pb.dirty.add(section);
      pb.touched = true;
      setNote(section, "edited", false);
    };
    node.addEventListener("input", handler);
    node.addEventListener("change", handler);
  }
  for (const node of dialog.querySelectorAll(".pb-brief input, .pb-brief textarea, .pb-brief select")) {
    node.addEventListener("input", () => {
      pb.touched = true;
    });
  }
  el("pbApTags").addEventListener("input", () => scheduleCheck("appearance"));
  el("pbApTags").addEventListener("change", () => runCheck("appearance", true));
  el("pbWardrobe").addEventListener("input", () => scheduleCheck("wardrobe"));
  el("pbWardrobe").addEventListener("change", () => runCheck("wardrobe", true));

  // ---- "Edit with builder" in the plain persona editor (called from chat.js) ----

  const openBuilderBtn = el("newPersonaOpenBuilderBtn");

  window.refreshOpenInBuilderButton = async function (name) {
    openBuilderBtn.hidden = true;
    pb.pendingLoad = null;
    try {
      const saved = await invoke("persona_builder_load", { name });
      // The editor may have been closed or pointed at another persona meanwhile.
      if (!saved || newPersonaDialog.hidden || newPersonaNameInput.value !== name) return;
      pb.pendingLoad = saved;
      openBuilderBtn.hidden = false;
    } catch (err) {
      console.error("persona_builder_load failed:", err);
    }
  };

  openBuilderBtn.addEventListener("click", () => {
    const saved = pb.pendingLoad;
    if (!saved) return;
    if (
      saved.hand_edited &&
      !confirm(
        "This card was edited by hand after the builder saved it. Opening it in the builder will replace those edits when you save.",
      )
    ) {
      return;
    }
    newPersonaDialog.hidden = true;
    openBuilder(saved);
  });
})();
