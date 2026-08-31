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

Pass a folder *and* a message to skip the GUI entirely: `./target/release/
llm-assistant /path/to/folder "what is my shopping list?"` runs one turn
(plus any commands it leads to) and prints the result to stdout, then exits
-- see `src/headless.rs`. This is the fastest way to test a prompt/behavior
change without driving the actual window. It never runs anything that would
need a confirmation dialog in the GUI (prints `[needs confirmation ...]` and
stops instead) -- headless mode isn't a way to bypass the safety model, just
a way to exercise it without clicking through it.

`--help`/`-h` prints usage and exits before `init_logging()` runs, so the
output stays clean (see `print_help` in `main.rs`) -- keep it that way if
touching argv handling.

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
  `granted_paths`, `auto_approve`, `max_auto_steps`), not scoped per-project.
  `build_root_note` is the shared per-turn note builder (folder-open state,
  plus each granted path and *why* it was granted) used by both the GUI's
  `send_message` and `headless.rs`, so they can't drift apart.
- `rules.rs` — `rules.md`, read *before* the system prompt every turn (see
  `send_message`): the mechanical/protocol stuff (command format, quoting,
  confirmation behavior, sudo handling, etc.) that a user customizing
  `system_prompt` shouldn't have to also carry or risk breaking.
- `chat_log.rs` — appends a flat, human-readable mirror of the GUI
  conversation (including collapsed "thinking" steps) to
  `<app-config-dir>/last-chat.log`, cleared at the start of every GUI launch.
  For inspecting a session without driving the actual window.
- `headless.rs` — `llm-assistant <folder> <message>` runs one turn (and any
  commands it leads to) without the GUI, printing to stdout and exiting.
  Mirrors `ui/main.js`'s orchestration logic; anything that would need a
  confirmation click just gets reported and stops the loop instead of
  running unattended.
- `llm.rs` — POSTs to `<endpoint>` in OpenAI `/chat/completions` shape; works
  against both Ollama and LM Studio without a vendor SDK. Sends
  `Authorization: Bearer <api_key>` when one is configured (LM Studio can be
  set to require it). On a non-2xx response or unparseable body, the error
  includes the raw response text rather than just the status code.
- `main.rs` — Tauri commands and app state (the selected root `PathBuf` and
  the in-flight LLM request's `AbortHandle`, both behind a `Mutex`; root
  optionally preloaded from a CLI argument via `resolve_cli_root`).
  `activate_root` is the shared "ensure `.temp-trash` exists" step used by
  the picker, CLI-arg startup, and headless mode. `pick_and_set_root` opens
  the folder dialog via the **non-blocking** `pick_folder` callback API
  bridged through a `tokio::oneshot` channel, awaited from an `async fn`
  command — calling the blocking variant (`blocking_pick_folder`) from
  inside a command is a known way to deadlock the picker, so don't switch
  back to it. `send_message` spawns the actual `send_chat` call via
  `tokio::spawn` (rather than just awaiting it) specifically so
  `stop_generation` has an `AbortHandle` to cancel — aborting resolves the
  `JoinHandle` in microseconds, it doesn't wait for a timeout. Logging goes
  through `log`/`simplelog` to both stderr and `<app-config-dir>/logs/app.log`
  (`init_logging`, called first thing in `main()`).

**Frontend** (`ui/`) is plain HTML/CSS/JS with no bundler — `tauri.conf.json`
points `frontendDist` straight at `ui/` and sets `app.withGlobalTauri: true`,
so `main.js` calls `window.__TAURI__.core.invoke(...)` directly instead of
importing `@tauri-apps/api`. On load it calls `get_current_root` to pick up a
CLI-preloaded folder without requiring a picker click, and `load_config` to
have `max_auto_steps` etc. available even before any folder is open.

`main.js` owns the propose → classify → (confirm if needed) → execute →
respond loop in `runAssistantTurn`/`handleProposedCommand`/`executeCommand`.
`parseAssistantReply` only treats an explicitly-tagged ` ```sh/```bash/```shell`
fence as a command proposal (a plain fence is just the model showing text,
per `rules.md`) and strips the matched fence from the displayed bubble text
(collapsing the leftover blank lines) since the command shows again in the
output block below. After a command runs (or is denied), the loop
automatically takes another turn so the model actually reacts to the result
instead of the conversation just stopping at raw output — capped at
`maxSteps` (0 = unlimited) via the `depth` parameter threaded through every
call. Every turn that goes on to propose another command is routed into a
lazily-created, collapsed `createThinkingTracker()` disclosure instead of the
main log; the turn that finally answers in plain text (or hits a
sudo/root command, see below) is shown as a normal bubble outside it, so a
one-shot question never grows a "Thinking" section at all. While a turn is
in flight the Send button becomes a real Stop button (`setProcessing`),
calling `stop_generation` to abort the in-flight request server-side, not
just flip a UI flag; `stopRequested` additionally keeps the chain from
starting a new step if Stop is clicked while a command is executing rather
than while generating.

`needsElevatedPrivileges` (checked in `runAssistantTurn` *before* the
thinking-routing decision, using every word in the command, not just the
first — a compound command can have `sudo` as its second word) short-circuits
sudo/su/doas/pkexec straight to an always-visible `appendManualCommand` block
(raw command + Copy button) instead of the doomed confirm-and-fail cycle, and
deliberately does **not** auto-continue — letting the model "try again" is
exactly what caused it to loop re-proposing the same sudo command. The
"always allow" checkbox in the confirm dialog calls `add_auto_approve`, which
is still gated by the same no-shell-metacharacters check inside
`sandbox::is_auto_approved` — a user can whitelist a program, never a
pipe/redirect shape. `renderMarkdown` is a small dependency-free Markdown
renderer (fenced code blocks, inline code, bold, italic) applied to assistant
bubbles only. Every `appendBubble`/`appendOutput`/`appendManualCommand` call
also fires `logToFile`, mirroring exactly what's shown (thinking included) to
`append_chat_log` / `last-chat.log`.

Settings (gear icon) work with no folder open, since config is global, and
has two tabs: General (endpoint/model/temperature/`max_auto_steps`/system
prompt/granted paths/auto-approve) and Rules (`rules.md` editor). Adding a
granted path opens the dedicated `addPathDialog` modal (Browse via
`pick_granted_path` or type a path, plus an optional "what's it for" note)
rather than a cramped inline row — that note isn't just cosmetic, it's folded
into `config::build_root_note` so the model can proactively decide to read a
granted path for a matching task instead of only when the user states the
absolute path themselves.

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
