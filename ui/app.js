// Startup: runs once the DOM is ready, after every other script has registered its handlers.

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
