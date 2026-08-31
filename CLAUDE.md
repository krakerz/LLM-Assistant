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

Linux-only, deliberately — the sandbox is built on `bubblewrap` (`bwrap`),
and no Windows/macOS port is planned (namespaces don't exist the same way, so
there's no equivalent boundary to port to). `TODO.md` (gitignored) holds the
planned follow-ups.

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
  do. `ensure_shims` writes fake `rm` and `rmdir` binaries onto a `PATH`
  that's prepended inside the jail, both moving targets into
  `.temp-trash/<timestamp>/<original relative path>` instead of deleting
  them — this runs unconditionally, independent of the read-only/confirmation
  split, so even a user-approved destructive command stays recoverable. The
  shim computes its own `date +%Y%m%d-%H%M%S-%N` timestamp per invocation
  (all targets of one call land in the same batch); without it, `mv -f`
  deleting the same path a second time would silently overwrite the first
  trashed copy with no way to get it back. `rmdir` sharing the same script
  means it no longer fails on a non-empty directory the way real `rmdir`
  would -- it always succeeds and moves the whole thing to trash regardless
  of contents, trading that one signal for the same "nothing is ever truly
  gone" guarantee `rm` already gets. Beyond these two, see `TODO.md` for
  what's still unshimmed (e.g. `mv` overwriting an existing target).
- `context.rs` — keeps a turn inside `AppConfig.max_context_tokens` (0 =
  never trim), used by both `send_message` and `headless.rs`.
  `estimate_tokens` is a deliberate chars/4 approximation, not a real
  tokenizer — every endpoint tokenizes differently, and for a safety margin
  cheap beats exact, which is why the default budget leaves headroom.
  `trim_to_budget` does two things in order, cheapest loss first.
  **Condensing**: a finished auto-continue step — an assistant message whose
  reply contains a ```sh fence, immediately followed by the `[command output,
  exit N]` message it produced — collapses into one entry holding the command
  and the output, throwing away only the assistant's narration around them.
  That narration is the bulk of a long chain (a 10-step chain is 20+
  messages) and the UI already draws exactly this line, collapsing
  intermediate steps into "Thinking…" while final answers stay inline. It
  runs oldest-first and only while over budget, so a short conversation is
  untouched; the *last* pair is never condensed, since that output is
  precisely what the turn is answering. The result message's own first line
  is reused verbatim rather than re-derived, so a `executeSequence` batch
  that failed partway keeps its real per-step exit codes. Long output/command
  text is cut at `CONDENSED_OUTPUT_CHARS`/`CONDENSED_COMMAND_CHARS` with an
  explicit "N more characters condensed away" note. Detection hangs on three
  places spelling the result prefix identically —
  `context::COMMAND_OUTPUT_PREFIX`, `formatCommandFeedback` in `main.js`, and
  `headless.rs` (which builds it from the constant); change one, change all
  three, or condensing silently stops finding anything. **Dropping**: only if
  condensing wasn't enough. Always keeps the first message (the original
  request) and the last (the turn being answered, even if it alone busts the
  budget) and leaves a `TRIM_MARKER` where messages were removed, so the
  model sees an explicit gap rather than an unexplained jump. `send_message`
  returns `condensed` and `dropped` alongside the reply so the UI can say
  either happened — silent trimming is the exact failure this replaces —
  reported separately since condensing loses no facts. Both are strictly
  mechanical, no summarization: see `TODO.md` for why (a bad summary from a
  small local model outranks a dropped turn as a hazard, since it becomes the
  record with no transcript left to check it).
- `paths.rs` — resolves `$XDG_CONFIG_HOME/llm-assistant` (falls back to
  `~/.config/llm-assistant`) for both config and logs. Deliberately not tied
  to the selected folder, since both need to exist before any folder is
  picked.
- `config.rs` — loads/saves `<app-config-dir>/config.toml`. This is one
  global config (endpoint, model, `api_key`, system prompt, temperature,
  `granted_paths`, `auto_approve`, `max_auto_steps`, `disable_builtin_rules`),
  not scoped per-project. `load_or_init` has no in-memory caching -- every
  Tauri command that needs it (`send_message`, `classify_command`, ...)
  calls it fresh, so config and rules are hot-reloaded on the very next turn
  after any edit (Settings, or directly to the file), no restart needed;
  `main.js`'s own `currentConfig` is a separate JS-side display cache that
  needs its own refresh before each new turn to stay in sync (see
  `chatForm`'s submit handler). `build_root_note` is the shared per-turn
  note builder (folder-open state, plus each granted path and *why* it was
  granted) used by both the GUI's `send_message` and `headless.rs`, so they
  can't drift apart. It explicitly states the open working folder is the
  "root"/home context for the session and a granted path is not "root"
  unless named specifically -- added after a real case where a model asked
  to move files "to root folder" targeted a granted path instead (whose own
  note happened to say "source code"), since nothing previously said which
  one "root" meant. The sandbox itself blocked the write regardless (the
  granted path was read-only), so this was a prompt-clarity gap, not a
  sandbox one.
- `rules.rs` — three tiers feeding the system message, in order, built by
  `build_system_rules` (used by both `send_message` and `headless.rs` so
  they can't drift apart): `PROTOCOL_PROMPT` (hardcoded, not a file, not
  user-editable -- the mechanical contract the app's own parsing/execution
  code actually depends on: the fenced-```sh-block format (with a worked
  example -- an early version without one saw a real regression where a
  small local model replied with a bare command as plain text for a simple
  query, which never ran since there was no fence to parse), only the first
  one per reply ever runs, sudo always fails here, `.temp-trash/` is the
  app's own soft-delete area and should be ignored like it's not there, and
  -- prefer writing a multi-step task as one self-contained script in that
  single block and running it once rather than many small commands that can
  leave things half-done if something fails partway), then `rules.md` (general behavior
  -- searching before guessing, re-verifying stale info, honesty about
  uncertainty) and `command-rules.md` (remaining shell mechanics -- quoting,
  confirmation behavior, granted-path absolute paths, `mv`/`cp` argument
  syntax) *unless* `AppConfig.disable_builtin_rules` is set, in which case
  only the protocol goes out -- deliberately kept minimal so the general/
  command files stay purely advisory and safe to discard entirely, whereas
  the protocol can't be turned off since the app's own code assumes it.
  `load_general_or_init`/`load_command_or_init` only write the default the
  *first* time a file doesn't exist -- editing
  `DEFAULT_GENERAL_RULES`/`DEFAULT_COMMAND_RULES` in code has no effect on
  an install that already has the file on disk; the user has to hit
  Settings -> Rules -> "Reset to default" to pick it up. `log_loaded_rules`
  logs the protocol plus the full text of both files (and whether they're
  actually being sent) to `app.log`, called once at startup (`main()`,
  before the headless dispatch so headless gets it too, not inside the load
  functions themselves since those run every turn and would spam the log)
  so a stale edit, a reset that didn't take, or the disable toggle doing the
  wrong thing is visible directly in the log.
  `extract_command` lives here too — the *read* side of the same fence
  contract `PROTOCOL_PROMPT` writes down, shared by `headless.rs` (which runs
  what it returns) and `context.rs` (which uses it to recognize a finished
  step). Only an explicitly-tagged fence counts, and the earliest fence in
  the reply wins rather than the first language that happens to match,
  matching `parseAssistantReply` in `main.js`.
- `chat_log.rs` — appends a flat, human-readable mirror of the GUI
  conversation (including collapsed "thinking" steps) to a session-scoped
  `<app-config-dir>/logs/chat-<timestamp>.log`, started fresh by `init()` at
  the beginning of every GUI launch (`main()`, before `.run()`). Up to 5 are
  kept, oldest deleted first (hardcoded `MAX_LOG_FILES`) -- unlike the old
  single `last-chat.log` that got wiped on every launch, a session from a
  previous run is still there to check after relaunching. `append()` writes
  to whichever path `init()` picked, held in a `OnceLock` so every
  `append_chat_log` Tauri command call (each its own invocation) lands in
  the same file instead of fragmenting.
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
than while generating. Every place a chain stops short of a real answer --
`stopRequested`, the `maxSteps` cap, or an aborted/failed `send_message`
call in the `catch` block -- also pushes a `[the user stopped this...]`/
`[automatic continuation paused...]`-shaped note into `history`, not just a
UI bubble; without it the model has no idea its last action was cut off and
will otherwise assume on the next turn that whatever it last proposed either
finished or never happened.

`createThinkingTracker()` also tracks the last command that actually ran in
the chain (`recordExecuted`, set in `executeCommand`/`executeSequence` after
a successful `run_command`); `runAssistantTurn` checks a newly-proposed
command against it (`isImmediateRepeat`) and hard-stops, same shape as the
sudo case below, if the model proposes the exact same command again right
after it ran with nothing else having run in between. This is what actually
caught a model getting stuck re-verifying a finished task forever (`ls -F` /
"organization is complete" repeated 20+ times, since `max_auto_steps = 0`
means nothing else would stop it) — a real re-check after an actual change
(`ls -F` again after an `mv`) isn't flagged, since a different command sits
between the two occurrences. `headless.rs` has the same guard
(`last_executed`), for parity.

The guard doesn't just stop dead, though: in practice it fires precisely
when the work is already *finished* and the model is re-running a listing to
display output it already has, usually having written "here is the current
structure:" first — so bailing out there left the user with a dangling
half-sentence inside a collapsed Thinking box and no answer at all, which
reads as the app cutting off mid-task. So it refuses the command and then
calls `finalAnswerTurn()`, one more `send_message` with a "no command, plain
text only" note appended; whatever comes back is shown as a normal bubble
*outside* `thinking` (the whole point — it's the closing answer), with any
command still in it stripped and never run, so it can't start a new chain.
Deliberately *not* wired into the Stop-button or `maxSteps` paths: Stop means
the user wants it to stop now, not to spend another request, and the cap
already tells them to send another message. Its prompt is also carefully
worded *not* to presuppose success — an earlier version asked for "what was
done and what the final result is", and after two denied commands (nothing
having run at all) the model duly invented a completed reorganization,
naming files and folders that didn't exist. It now asks for the current
state strictly from output actually received.

Denying a command likewise ends the chain rather than auto-continuing — a
denial is a deliberate "no", and continuing let the model keep flailing and
was one of the ingredients in that fabricated report. The note it gets is
emphatic that the command did not run and changed nothing, since a milder
"[the user denied that command]" left it speculating that the command may
have "partially executed". The root cause is addressed in
`PROTOCOL_PROMPT` too (every command's output is already shown to the user
and handed back, so never re-run one just to display/confirm it) — that's
what actually stops the loop; `finalAnswerTurn` is the backstop.

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
pipe/redirect shape. `splitCommandSequence` (used by `requestApproval` to
decide whether the confirm dialog shows a checklist or one opaque block)
splits on bare newlines as well as `&&`/`;`, not just `&&`/`;` -- a model
asked to do several things in sequence often just writes one command per
line with no `&&`. Left as one atomic `sh -c` block, a failure partway
through gets masked, since `sh -c` reports the exit code of the *last* line,
not the first failure; splitting it into a checklist runs each line as its
own step via `executeSequence`, which already stops at the first real
failure and reports it accurately. It still refuses to split (returns
`null`, stays one atomic block) if any line is a shell control-flow keyword
(`for`/`while`/`if`/`do`/`done`/...) or a comment/shebang, since a real
script's lines aren't independently valid commands on their own. `renderMarkdown` is a small dependency-free Markdown
renderer (fenced code blocks, inline code, bold, italic) applied to assistant
bubbles only. Every `appendBubble`/`appendOutput`/`appendManualCommand` call
also fires `logToFile`, mirroring exactly what's shown (thinking included) to
`append_chat_log` / this session's `logs/chat-<timestamp>.log`.

Settings (gear icon) work with no folder open, since config is global, and
has two tabs: General (endpoint/model/temperature/`max_auto_steps`/system
prompt/granted paths/auto-approve) and Rules (`rules.md` editor). Adding a
granted path opens the dedicated `addPathDialog` modal (Browse via
`pick_granted_path` or type a path, plus an optional "what's it for" note)
rather than a cramped inline row — that note isn't just cosmetic, it's folded
into `config::build_root_note` so the model can proactively decide to read a
granted path for a matching task instead of only when the user states the
absolute path themselves.

**CI** (`.github/workflows/autobuild.yml`) is a single workflow triggered
only by pushes to `main` (a PR by itself triggers nothing; no manual tag
pushes either). It reads the version out of `Cargo.toml`, checks via
`git ls-remote` whether that tag already exists remotely, and only then
builds the full bundle and opens a **draft** GitHub release (tag created by
`tauri-apps/tauri-action`, release body pulled from the matching
`CHANGELOG.md` section). A push that didn't bump the version just runs
fmt/clippy/test/build as a sanity check. This means the version bump commit
described above is what actually triggers a release once it reaches `main`
— there's no separate tagging step. Rust build artifacts are cached via
`Swatinem/rust-cache`, apt packages via `actions/cache` keyed on the
workflow file itself.
