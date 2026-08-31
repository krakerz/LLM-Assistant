# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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
