# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Tauri (Rust) desktop app that lets a user pick a folder, chat with a local
LLM (any OpenAI-compatible endpoint — LM Studio, Ollama), and have the model's
proposed shell commands actually executed — but confined to that one folder.
There is no fixed tool-call schema: the model is prompted to reply with a
one-line explanation plus a single fenced ```sh command, and the app decides
whether to run it automatically or ask the user first. Chat also works with
no folder open at all (plain assistant mode) — folder selection only gates
whether commands can be proposed/run, not chat itself.

Linux-only for now — the sandbox is built on `bubblewrap` (`bwrap`), which
has no equivalent wired up for Windows/macOS yet (see `TODO.md`, gitignored,
for planned follow-ups).

Single-crate layout: `Cargo.toml`/`src/`/`tauri.conf.json` live at the repo
root (no `src-tauri/` subdirectory — this was flattened deliberately since
there's no separate frontend build tooling to isolate it from). `ui/` is the
static frontend.

## Commands

Local dev requires `bubblewrap`, `webkit2gtk-4.1`, and `gtk3` dev packages
installed, plus the Tauri CLI (`cargo install tauri-cli --version "^2"`,
exposed as `cargo tauri`).

```sh
cargo tauri dev                          # run the app with live reload
cargo fmt -- --check                     # formatting
cargo clippy --all-targets -- -D warnings
cargo test                               # no tests exist yet
cargo build --release                    # compile-only check
cargo tauri build --bundles deb          # full bundle (see caveat below)
```

Pass a directory as the first CLI argument to preload it as the working
folder on startup, skipping the picker — useful for repeated manual testing:
`./target/release/llm-assistant /path/to/folder`.

`cargo tauri build --bundles appimage` can fail on very new/rolling-release
toolchains: the bundled `linuxdeploy`'s `strip` binary doesn't understand
`.relr.dyn` ELF sections emitted by newer binutils. Not an issue on the
`ubuntu-24.04` CI runner; `--bundles deb` sidesteps it locally since it uses
the host's own `strip`.

To cut a release: bump the version in `Cargo.toml` and `tauri.conf.json`,
promote `CHANGELOG.md`'s `[Unreleased]` section to that version, and commit
(no tag, no push — see below). There's no script for this; it's a small
enough edit to do by hand or ask Claude to do directly.

## Architecture

**Backend** (`src/`), one Tauri command handler per file group in `main.rs`:
- `sandbox.rs` — the security-relevant module. `classify_command` decides
  `ReadOnly` vs `NeedsConfirmation` by first checking for shell metacharacters
  (`| > < & ; \` $( `) anywhere in the raw string, then matching the first
  token against a small read-only-binary allowlist. This classification is
  UX only (auto-run vs. show the confirm dialog) — it is never the security
  boundary. The actual boundary is `run_sandboxed`, which shells out to
  `bwrap` with `--unshare-all` (no network, no PID/IPC visibility) and binds
  only the selected root directory (read-write) plus any user-granted paths;
  everything else is invisible to the command regardless of what it tries to
  do. `ensure_shims` writes a fake `rm` onto a `PATH` that's prepended inside
  the jail, which moves targets into `.temp-trash/<original relative path>`
  instead of deleting them — this runs unconditionally, independent of the
  read-only/confirmation split, so even a user-approved destructive command
  stays recoverable. Only `rm` is shimmed today (see `TODO.md`).
- `paths.rs` — resolves `$XDG_CONFIG_HOME/llm-assistant` (falls back to
  `~/.config/llm-assistant`) for both config and logs. Deliberately not tied
  to the selected folder, since both need to exist before any folder is
  picked.
- `config.rs` — loads/saves `<app-config-dir>/config.toml`. This is one
  global config (endpoint, model, `api_key`, system prompt, temperature,
  `granted_paths`, `auto_approve`), not scoped per-project.
- `llm.rs` — POSTs to `<endpoint>` in OpenAI `/chat/completions` shape; works
  against both Ollama and LM Studio without a vendor SDK. Sends
  `Authorization: Bearer <api_key>` when one is configured (LM Studio can be
  set to require it). On a non-2xx response or unparseable body, the error
  includes the raw response text rather than just the status code.
- `main.rs` — Tauri commands and app state (the selected root `PathBuf`
  behind a `Mutex`, optionally preloaded from a CLI argument via
  `resolve_cli_root`). `activate_root` is the shared "ensure `.temp-trash`
  exists" step used by both the picker and CLI-arg startup path.
  `pick_and_set_root` opens the folder dialog via the **non-blocking**
  `pick_folder` callback API bridged through a `tokio::oneshot` channel,
  awaited from an `async fn` command — calling the blocking variant
  (`blocking_pick_folder`) from inside a command is a known way to deadlock
  the picker, so don't switch back to it. `send_message` appends a note to
  the configured system prompt each turn stating whether a folder is
  currently open, so the model knows whether proposing commands makes sense.
  Logging goes through `log`/`simplelog` to both stderr and
  `<app-config-dir>/logs/app.log` (`init_logging`, called first thing in
  `main()`).

**Frontend** (`ui/`) is plain HTML/CSS/JS with no bundler — `tauri.conf.json`
points `frontendDist` straight at `ui/` and sets `app.withGlobalTauri: true`,
so `main.js` calls `window.__TAURI__.core.invoke(...)` directly instead of
importing `@tauri-apps/api`. On load it calls `get_current_root` to pick up a
CLI-preloaded folder without requiring a picker click. `main.js` owns the
propose → classify → (confirm if needed) → execute loop: it regexes a single
```sh fence out of the assistant's reply, calls `classify_command`, and
either runs it immediately or opens the confirm `<dialog>` showing the *raw*
command text (not an LLM-authored summary) before calling `run_command`. The
"always allow" checkbox in that dialog calls `add_auto_approve`, which is
still gated by the same no-shell-metacharacters check inside
`sandbox::is_auto_approved` — a user can whitelist a program, never a
pipe/redirect shape. Settings (gear icon) work with no folder open, since
config is global.

**CI** (`.github/workflows/autobuild.yml`) is a single workflow triggered by
PRs into `main` and pushes to `main` (no manual tag pushes). On push to
`main` it reads the version out of `Cargo.toml`, checks via `git ls-remote`
whether that tag already exists remotely, and only then builds the full
bundle and opens a **draft** GitHub release (tag created by
`tauri-apps/tauri-action`, release body pulled from the matching
`CHANGELOG.md` section). Every other trigger (PRs, or a push that didn't bump
the version) just runs fmt/clippy/test/build as a sanity check. This means
the version bump commit described above is what actually triggers a release
once it reaches `main` — there's no separate tagging step.
