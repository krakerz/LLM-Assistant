// Chat-mode rendering: block splitting, bubbles, placeholders, generated-image preview.

// Chat mode is persona/roleplay-shaped. Replies use the common roleplay
// convention small models already know: `*action*`, `"speech"`, `(thought)`.
// Text outside asterisks and parentheses counts as speech too, so an
// unmarked reply is not an error. Inside a quoted span `*x*`/`**x**` is plain
// emphasis (<em>/<strong>), not an action, and parentheses are just text.
// Edge cases: an unpaired `*` is dropped, an unpaired `(`/`"` stays literal,
// `(...)` is a thought only if it holds a letter (`(1)`, `(:` stay speech),
// and adjacent speech (quoted + plain) merges into one block.
//
// Older sessions used `// narration //` and `|| dialogue ||`; any text with
// at least one complete such pair is parsed by `splitLegacyBlocks` exactly as
// before. Fenced/inline code is pulled out first for both grammars.
//
// `hideNarration` (from `chat_hide_narration`) drops action blocks only
// (thoughts stay) rather than just styling them differently -- the model is
// still always told to write them (`CHAT_NARRATION_PROMPT`), this is
// purely a display choice.
// Parses `text` into blocks (`{type: "action"|"dialogue"|"thought", html}`),
// kept separate rather than joined into one string --
// `groupChatBlocksIntoBubbles` below needs the blocks apart to split them
// across multiple bubbles.
function splitChatBlocks(text, hideNarration = false) {
  const codeBlocks = [];
  // NUL delimits our placeholders below; a literal one in model text would forge one.
  let working = text.replace(/\u0000/g, "").replace(/```([a-zA-Z]*)\n([\s\S]*?)```/g, (_, _lang, code) => {
    codeBlocks.push(code.replace(/\n$/, ""));
    return `\u0000CB${codeBlocks.length - 1}\u0000`;
  });
  const inlineCode = [];
  working = working.replace(/`([^`\n]+)`/g, (_, code) => {
    inlineCode.push(code);
    return `\u0000IC${inlineCode.length - 1}\u0000`;
  });
  // URLs are blanked for this test only: two `https://` in one message would
  // otherwise look like a `//...//` pair.
  const legacyPair = /(\/\/|\|\|)[\s\S]+?(\/\/|\|\|)/;
  if (legacyPair.test(working.replace(/\b[a-z][a-z0-9+.-]*:\/\/\S*/gi, ""))) {
    return splitLegacyBlocks(working, codeBlocks, inlineCode, hideNarration);
  }
  return splitRoleplayBlocks(working, codeBlocks, inlineCode, hideNarration);
}

// Escapes `raw`, applies `**bold**`/`*italic*`, drops any leftover `*`, then
// restores the pulled-out code. Code is restored last so asterisks inside it
// survive.
function renderRoleplayInline(raw, codeBlocks, inlineCode) {
  return escapeHtml(raw)
    .replace(/\*\*([^*\n]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\*([^*\n]+)\*/g, "<em>$1</em>")
    .replace(/\*/g, "")
    .replace(
      /\u0000CB(\d+)\u0000/g,
      (_, i) => `<pre class="md-code"><code>${escapeHtml(codeBlocks[Number(i)] ?? "")}</code></pre>`,
    )
    .replace(/\u0000IC(\d+)\u0000/g, (_, i) => `<code>${escapeHtml(inlineCode[Number(i)] ?? "")}</code>`);
}

// Index of the `)` matching the `(` at `open` on the same line, or -1.
function matchingParen(s, open) {
  let depth = 0;
  for (let i = open; i < s.length && s[i] !== "\n"; i++) {
    if (s[i] === "(") depth++;
    else if (s[i] === ")" && --depth === 0) return i;
  }
  return -1;
}

function splitRoleplayBlocks(working, codeBlocks, inlineCode, hideNarration) {
  const fmt = (raw) => renderRoleplayInline(raw, codeBlocks, inlineCode);
  const blocks = [];
  // Speech pieces (quoted + plain) accumulate until an action/thought
  // interrupts; `speechRaw` is only for the "anything but whitespace?" test.
  let speechHtml = "";
  let speechRaw = "";
  const flushSpeech = () => {
    // Punctuation left between blocks (`*smiles*.`) isn't speech; a lone "."
    // line would only look broken.
    // Leading punctuation is the tail of the block before (`*smiles*. "Hi"`).
    speechHtml = speechHtml.replace(/^[\s.,;:…]+/u, "");
    if (/[\p{L}\p{N}]/u.test(speechRaw)) {
      blocks.push({ type: "dialogue", html: `<div class="chat-dialogue">${speechHtml.trim()}</div>` });
    }
    speechHtml = "";
    speechRaw = "";
  };
  const push = (type, raw) => {
    if (!raw.trim()) return;
    flushSpeech();
    // No fallback to raw text when everything is hidden: an all-action
    // message under `hideNarration` legitimately renders as nothing.
    if (type === "action" && hideNarration) return;
    blocks.push({ type, html: `<div class="chat-${type}">${fmt(raw.trim())}</div>` });
  };
  let plain = "";
  const flushPlain = () => {
    speechHtml += fmt(plain);
    speechRaw += plain;
    plain = "";
  };

  let i = 0;
  while (i < working.length) {
    const ch = working[i];
    if (ch === '"' || ch === "\u201c") {
      const close = ch === '"' ? working.slice(i + 1).search(/["\u201d]/) : working.indexOf("\u201d", i + 1);
      const end = ch === '"' && close >= 0 ? i + 1 + close : close;
      if (end > i) {
        flushPlain();
        const span = working.slice(i, end + 1);
        speechHtml += fmt(span);
        speechRaw += span;
        i = end + 1;
        continue;
      }
    } else if (ch === "*") {
      const m = /^\*\*([^*\n]+)\*\*|^\*([^*\n]+)\*/.exec(working.slice(i));
      if (m) {
        flushPlain();
        push("action", m[1] ?? m[2]);
        i += m[0].length;
      } else {
        i++; // unpaired `*`
      }
      continue;
    } else if (ch === "(") {
      const end = matchingParen(working, i);
      const inner = end > i ? working.slice(i + 1, end) : "";
      if (/\p{L}/u.test(inner)) {
        flushPlain();
        push("thought", inner);
        i = end + 1;
        continue;
      }
    }
    plain += ch;
    i++;
  }
  flushPlain();
  flushSpeech();
  return blocks;
}

function splitLegacyBlocks(working, codeBlocks, inlineCode, hideNarration) {
  const bold = [];
  working = working.replace(/\*\*([^*\n]+)\*\*/g, (_, b) => {
    bold.push(b);
    return `\u0000B${bold.length - 1}\u0000`;
  });
  // Bold has already been pulled out above, so any `*...*` left at this
  // point is unambiguously italic, not half of a `**bold**` pair.
  const italic = [];
  working = working.replace(/\*([^*\n]+)\*/g, (_, em) => {
    italic.push(em);
    return `\u0000EM${italic.length - 1}\u0000`;
  });

  // Bold/italic bodies can hold code/bold placeholders, so they restore recursively.
  const restore = (escaped) =>
    escaped
      .replace(
        /\u0000CB(\d+)\u0000/g,
        (_, i) => `<pre class="md-code"><code>${escapeHtml(codeBlocks[Number(i)] ?? "")}</code></pre>`,
      )
      .replace(/\u0000IC(\d+)\u0000/g, (_, i) => `<code>${escapeHtml(inlineCode[Number(i)] ?? "")}</code>`)
      .replace(
        /\u0000B(\d+)\u0000/g,
        (_, i) => `<strong>${restore(escapeHtml(bold[Number(i)] ?? ""))}</strong>`,
      )
      .replace(
        /\u0000EM(\d+)\u0000/g,
        (_, i) => `<em>${restore(escapeHtml(italic[Number(i)] ?? ""))}</em>`,
      );

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
      setIconText(note, "file-text", att.name);
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

// Placeholders cover a request round-trip (the follow-up decision call, image
// generation, web search). Turn 1's own reply streams; for a backend that
// reports reasoning separately, appendChatThinkingDelta upgrades the plain
// placeholder live. It starts plain, not expandable, because most uses never
// get reasoning text; resolveChatThinking removes it if nothing arrived.
// `chat-transient` nodes carry their session in `dataset.sessionId`:
// renderChatHistoryLog keeps only the current session's across a redraw, so
// one chat's in-flight UI never leaks into another.
function createChatThinkingPlaceholder(sid = currentSessionId) {
  const pending = document.createElement("div");
  pending.className = "bubble thinking-pending chat-transient";
  pending.dataset.sessionId = sid ?? "";
  setIconText(pending, "brain", "Thinking…");
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
  setIconText(summary, "brain", "Completed");
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

// Lazily upgrades a plain "Thinking…" placeholder into an expandable,
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
    setIconText(summary, "brain", "Thinking…");
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
function createLiveReplyBubble(sid = currentSessionId) {
  const div = document.createElement("div");
  div.className = "bubble assistant chat-transient";
  div.dataset.sessionId = sid ?? "";
  chatModeLog.appendChild(div);
  chatModeLog.scrollTop = chatModeLog.scrollHeight;
  return div;
}

// Small icon-only badge beside the bubble of a turn whose follow-up queued a
// memory update (queued, not finished -- the update runs in the background).
// A sibling in a flex wrapper rather than an overlay, so it never covers text.
function appendStateDispatchedIndicator(bubble) {
  if (!bubble?.isConnected) return; // redrawn or switched away meanwhile
  const wrapper = document.createElement("div");
  wrapper.className = "bubble-with-indicator";
  bubble.replaceWith(wrapper);
  wrapper.appendChild(bubble);
  const badge = document.createElement("span");
  badge.className = "state-updated-badge";
  setIcon(badge, "sticky-note", 12);
  badge.title = "this turn triggered a memory update in the background";
  wrapper.appendChild(badge);
}

// Generation is a separate, possibly slow step after the text reply is on
// screen, so it gets its own placeholder instead of holding the whole turn.
function createImageGenPlaceholder(sid = currentSessionId) {
  const div = document.createElement("div");
  div.className = "bubble assistant chat-transient";
  div.dataset.sessionId = sid ?? "";
  setIconText(div, "image", "Generating image… this can take a while");
  chatModeLog.appendChild(div);
  chatModeLog.scrollTop = chatModeLog.scrollHeight;
  return div;
}

function resolveImageGenPlaceholder(div, dataUrl, path, errorText) {
  if (errorText) {
    // Nothing in history represents a failed generation, so it stays transient
    // to survive a redraw.
    setIconText(div, "triangle-alert", `Image generation failed: ${errorText}`);
    return;
  }
  // The caller mirrors the image into the history cache, so a redraw rebuilds
  // it; staying transient would duplicate this node.
  div.classList.remove("chat-transient");
  div.textContent = "";
  const img = document.createElement("img");
  img.className = "chat-generated-image";
  img.src = dataUrl;
  wireImagePreview(img, path);
  div.appendChild(img);
  chatModeLog.scrollTop = chatModeLog.scrollHeight;
}

// Disappears on success; the real payoff is the answer message appended
// separately.
function createWebSearchPlaceholder(sid = currentSessionId) {
  const div = document.createElement("div");
  div.className = "bubble assistant chat-transient";
  div.dataset.sessionId = sid ?? "";
  setIconText(div, "search", "Searching the web… this can take a moment");
  chatModeLog.appendChild(div);
  chatModeLog.scrollTop = chatModeLog.scrollHeight;
  return div;
}

function resolveWebSearchPlaceholder(div, errorText) {
  if (errorText) {
    setIconText(div, "triangle-alert", `Web search failed: ${errorText}`);
    return;
  }
  div.remove();
}

// --- Generated image cache ---
//
// Redraws (narration toggle, settings save, reopening a chat) would otherwise
// re-read and re-base64 every image over IPC. Capped because entries are MB-sized.
const GENERATED_IMAGE_CACHE_MAX = 24;
const generatedImageCache = new Map(); // path -> data URL, insertion-ordered

function cacheGeneratedImage(path, dataUrl) {
  generatedImageCache.delete(path);
  generatedImageCache.set(path, dataUrl);
  while (generatedImageCache.size > GENERATED_IMAGE_CACHE_MAX) {
    generatedImageCache.delete(generatedImageCache.keys().next().value);
  }
}

async function loadGeneratedImage(path) {
  const hit = generatedImageCache.get(path);
  if (hit) {
    cacheGeneratedImage(path, hit); // refresh recency
    return hit;
  }
  const dataUrl = await invoke("read_generated_image", { path });
  cacheGeneratedImage(path, dataUrl);
  return dataUrl;
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
  imagePreviewImg.removeAttribute("src"); // don't keep a full-size data URL alive
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
