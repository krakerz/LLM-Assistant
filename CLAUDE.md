# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Tauri (Rust) desktop app that lets a user pick a folder, chat with a local
LLM (any OpenAI-compatible endpoint — LM Studio, Ollama), and have the model's
proposed shell commands actually executed — but confined to that one folder.
There is no fixed tool-call schema: the model is prompted to reply with a
one-line explanation plus a single fenced ```sh command, and the app decides
whether to run it automatically or ask the user first.

Linux-only for now — the sandbox is built on `bubblewrap` (`bwrap`), which
has no equivalent wired up for Windows/macOS yet (see `TODO.md`, gitignored,
for planned follow-ups).

## Commands

Local dev requires `bubblewrap`, `webkit2gtk-4.1`, and `gtk3` dev packages
installed, plus the Tauri CLI (`cargo install tauri-cli --version "^2"`,
exposed as `cargo tauri`).

```sh
cargo tauri dev                                                    # run the app with live reload
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check          # formatting
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml                    # no tests exist yet
cargo build --manifest-path src-tauri/Cargo.toml --release         # compile-only check
cargo tauri build --bundles deb                                    # full bundle (see caveat below)
```

`cargo tauri build --bundles appimage` can fail on very new/rolling-release
toolchains: the bundled `linuxdeploy`'s `strip` binary doesn't understand
`.relr.dyn` ELF sections emitted by newer binutils. Not an issue on the
`ubuntu-24.04` CI runner; `--bundles deb` sidesteps it locally since it uses
the host's own `strip`.

To cut a release: `scripts/release.sh X.Y.Z` bumps the version in
`src-tauri/Cargo.toml` and `tauri.conf.json` and promotes `CHANGELOG.md`'s
`[Unreleased]` section, then commits (no tag, no push — see below).

## Architecture

**Backend** (`src-tauri/src/`), one Tauri command handler per file group in
`main.rs`:
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
- `config.rs` — loads/saves `<selected-root>/.config/config.toml` (endpoint,
  model, system prompt, temperature, `granted_paths`, `auto_approve`). Config
  lives inside the user's chosen folder, not in app data — it's per-project.
- `llm.rs` — POSTs to `<endpoint>` in OpenAI `/chat/completions` shape; works
  against both Ollama and LM Studio without a vendor SDK.
- `main.rs` — Tauri commands and app state (currently just the selected root
  `PathBuf` behind a `Mutex`). `pick_and_set_root` is the only place that
  initializes config and creates `.temp-trash/`.

**Frontend** (`ui/`) is plain HTML/CSS/JS with no bundler — `tauri.conf.json`
points `frontendDist` straight at `ui/` and sets `app.withGlobalTauri: true`,
so `main.js` calls `window.__TAURI__.core.invoke(...)` directly instead of
importing `@tauri-apps/api`. `main.js` owns the propose → classify → (confirm
if needed) → execute loop: it regexes a single ```sh fence out of the
assistant's reply, calls `classify_command`, and either runs it immediately
or opens the confirm `<dialog>` showing the *raw* command text (not an
LLM-authored summary) before calling `run_command`. The "always allow"
checkbox in that dialog calls `add_auto_approve`, which is still gated by the
same no-shell-metacharacters check inside `sandbox::is_auto_approved` — a
user can whitelist a program, never a pipe/redirect shape.

**CI** (`.github/workflows/autobuild.yml`) is a single workflow triggered by
PRs into `main` and pushes to `main` (no manual tag pushes). On push to
`main` it reads the version out of `src-tauri/Cargo.toml`, checks via
`git ls-remote` whether that tag already exists remotely, and only then
builds the full bundle and opens a **draft** GitHub release (tag created by
`tauri-apps/tauri-action`, release body pulled from the matching
`CHANGELOG.md` section). Every other trigger (PRs, or a push that didn't bump
the version) just runs fmt/clippy/test/build as a sanity check. This means
the version bump commit from `scripts/release.sh` is what actually triggers a
release once it reaches `main` — there's no separate tagging step.
