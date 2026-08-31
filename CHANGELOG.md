# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.2.1] - 2026-08-31
### Fixed
- The `rm` soft-delete shim used `mv -f` into a fixed `.temp-trash/<relative
  path>` destination -- deleting the same path a second time silently
  overwrote the first trashed copy with no way to get it back. Each `rm`
  invocation now gets its own timestamped subfolder
  (`.temp-trash/<timestamp>/<relative path>`), so repeated deletes of the
  same path each stay independently recoverable.

## [1.2.0] - 2026-08-31
### Fixed
- A model that finishes a task can get stuck re-verifying it forever --
  observed in practice as `ls -F` and "the organization is complete" being
  repeated 20+ times in a row with no new information, only stopped by
  manually clicking Stop (`max_auto_steps = 0` means unlimited, so nothing
  else would have). The auto-continue chain now tracks the last command that
  actually ran and hard-stops (same as the sudo case) if the model proposes
  that exact command again immediately with nothing else having run in
  between -- a real re-check after an actual change (e.g. `ls -F` again
  after an `mv`) is unaffected, since a different command sits between the
  two. Applied to both the GUI chain and headless mode.
- Softened the "re-check stale listings" general rule with an explicit
  counter-case (don't re-run a listing you already just got with nothing in
  between to have changed it) -- the plain "re-check if it matters" wording
  was apparently read as "always re-check," contributing to the loop above.
  Existing installs keep whatever's already saved in `rules.md`; use
  Settings -> Rules -> "Reset to default" to pick this up.

### Added
- `app.log` now logs the full text of both loaded rule files once at
  startup (GUI and headless), so a stale edit or a "Reset to default" that
  didn't take is visible directly in the log instead of only inferred from
  behavior.
- The GUI's chat-conversation mirror is no longer a single file wiped on
  every launch -- each launch starts a new `logs/chat-<timestamp>.log`, and
  up to 5 are kept (oldest deleted first), so a previous session's log is
  still there to check after relaunching.

## [1.1.1] - 2026-08-31
### Fixed
- A reply containing more than one ` ```sh ` block only ever ran the first
  (as documented), but the second was silently dropped: left visible in the
  chat looking like a still-pending command that in fact never ran, and the
  model was never told, so it would confidently report the rest as done on
  the next turn even though nothing happened. Now every fence is stripped
  from the display (not just the first), and the model gets an explicit
  note that only the first command ran.
- The API key field was narrower than every other Settings field --
  `input[type="password"]` was missing from the "full width" style rule.

### Added
- Always-allowed programs are now a proper list with a remove button each,
  instead of a read-only comma-separated line.

## [1.1.0] - 2026-08-31
### Added
- The confirmation dialog now splits a compound `&&`/`;`-chained command
  into a checklist of individual steps, each independently approvable --
  check off only the ones you want and they run in order, stopping at the
  first failure. (A pipe, redirect, or command substitution can't be safely
  split apart without changing what it does, so those still show as one
  block, same as before.)
- Working rules split into two files: `rules.md` (general behavior --
  identity, honesty about uncertainty, re-verifying stale info) and
  `command-rules.md` (shell mechanics -- command format, quoting, sudo
  handling, `mv`/`cp` argument syntax), each with its own Settings tab and
  "Reset to default".

### Changed
- Strengthened the `mv`/`cp` guidance with a concrete worked example and
  explicit advice against reaching for a shell loop, after seeing the model
  repeat the same "alternating source/destination pairs" mistake.

## [1.0.1] - 2026-08-31
### Fixed
- A granted path that happens to be an *ancestor* of the working folder
  (e.g. granting `~/src` while `~/src/playground` is the folder open) could
  silently make the working folder read-only too. `bwrap` applies bind
  mounts in argument order, and the granted (read-only) path was being
  bound after the working folder, so its mount covered the working folder's
  read-write one underneath it. Fixed by binding the working folder last,
  so it always wins back its own subtree regardless of what else is
  granted.

## [1.0.0] - 2026-08-31
Initial release.

### Added
- Desktop chat app (Tauri/Rust) for talking to a local LLM -- any
  OpenAI-compatible endpoint, so both LM Studio and Ollama work -- about
  files in a folder you choose. Chat also works with no folder open, as a
  plain assistant; selecting a folder is what enables file/command access.
- Folder-scoped sandbox via `bubblewrap`: commands the assistant proposes
  can only see the selected folder plus explicitly granted paths, nothing
  else on the filesystem. Sandboxed commands run with a fully cleared
  environment (only `PATH`/`TRASH_ROOT` are set).
- No fixed tool-call schema -- the assistant proposes a shell command in a
  single fenced ```sh block. Read-only commands (`ls`, `cat`, `grep`,
  `find`, `uname`, ...) run automatically; anything else needs your
  approval, shown with the assistant's own explanation alongside the raw
  command.
- Soft-delete: `rm` inside the sandbox moves targets into `.temp-trash/`
  (preserving their layout) instead of deleting them, so an approved delete
  stays recoverable.
- `sudo`/`su`/`doas`/`pkexec` are detected and never run -- a sandboxed
  shell can never gain root here -- so you're shown the command to run
  yourself (with a Copy button) instead of a doomed approval prompt.
- Automatic follow-up turns: after a command runs, the assistant
  automatically responds to the result instead of raw output being the
  last thing shown. Intermediate steps collapse into a "Thinking" section;
  only the final answer is shown inline, with a configurable cap
  (`max_auto_steps`, 0 = unlimited) on how many steps can chain
  automatically before waiting for you.
- Real Stop button (cancels the in-flight request itself), Clear-chat, and
  Unmount (drop the folder without restarting).
- Granted paths: read (or read-write) access to specific folders outside
  the working directory, added via a dedicated dialog with an optional
  purpose note the assistant actually sees (so it can use the folder
  proactively, not just when told the absolute path), and a recursive vs.
  top-level-only toggle.
- Global settings at `~/.config/llm-assistant/config.toml` (endpoint,
  model, API key, temperature, granted paths, max automatic steps).
  Working rules (command format, quoting, confirmation behavior, sudo
  handling, etc.) live separately in `rules.md`, read before your
  customizable system prompt, so editing one can't break the other.
- CLI folder preload (`llm-assistant <folder>`), headless mode
  (`llm-assistant <folder> <message>` -- runs one turn, prints the result,
  exits, no GUI) for scripting and testing, and `--help`/`-h`.
- Debug logging (`logs/app.log`) and a plain-text mirror of each GUI
  conversation, thinking steps included (`last-chat.log`, cleared on every
  launch), for troubleshooting.
- Markdown rendering for assistant messages, font size controls, app
  version shown in Settings.
- MIT license.
