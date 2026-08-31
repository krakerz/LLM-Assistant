# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]
### Added
- Long conversations now **condense finished command steps instead of
  dropping them**, whenever that's enough to fit `max_context_tokens`. The
  biggest consumer of the context window isn't the user's own messages, it's
  auto-continue chains -- a ten-step chain is twenty-plus messages, most of
  which is the assistant narrating what it's about to do. Each finished
  propose -> run step is now folded into a single entry holding just the
  command that ran and the output it produced (long output is cut with an
  explicit note saying how much), oldest first, and only if the turn *still*
  doesn't fit does the existing oldest-first dropping kick in. The step being
  answered right now is never condensed. Reported in the chat and in
  `app.log`, separately from dropping, since nothing factual is lost this
  way. Applies to headless mode too.
  Still strictly mechanical -- the command text and its real output are kept
  verbatim, so this can't invent a fact the way a summary could.

## [1.6.0] - 2026-08-31
### Added
- Context-window management. Every turn previously sent the system block
  plus the entire conversation with nothing ever trimmed, so a long enough
  session would eventually overflow the model's context -- surfacing as an
  endpoint error, or worse, the model quietly losing its earliest turns with
  no indication. The oldest turns are now dropped once a turn would exceed
  `max_context_tokens` (new setting, default 8000, 0 = never trim), with the
  original request and the turn being answered always preserved, an explicit
  gap marker left where messages were removed, and the drop reported both in
  the chat and in `app.log` -- never silently. Token counting is a cheap
  chars/4 estimate rather than a real tokenizer (every endpoint tokenizes
  differently), so the default leaves headroom; set it under your model's
  real limit. Applies to headless mode too.
  Deliberately mechanical dropping only, no summarization for now: a bad
  summary from a small local model is worse than a dropped turn, since it
  becomes the authoritative record with no transcript left to check against.
  See `TODO.md` for the staged plan beyond this.

## [1.5.1] - 2026-08-31
### Fixed
- **The assistant could report work it never did.** After the user denied a
  command twice (so nothing ran at all), it produced a confident, fully
  fabricated summary of a directory reorganization -- listing which files
  had gone into which new folders, none of which existed. Three causes,
  all fixed:
  - Denying a command auto-continued the chain, letting the model keep
    flailing after a deliberate "no". A denial now ends the chain outright
    (same as the sudo case) and says so, and the note the model gets is
    explicit that the command did NOT run and changed nothing.
  - The 1.5.0 wrap-up turn asked the model to say "what was done and what
    the final result is" -- which presupposes success and directly invited
    the fabrication. It now asks for the current state strictly from
    command output actually received, and to plainly say so when commands
    were denied, never ran, or failed.
  - The protocol now forbids reporting any action as done unless a command
    actually ran and its output shows it worked, requires saying plainly
    when something was denied or failed, and asks for a single verifying
    listing (plus which parts succeeded) when a change may have only
    partly landed.

## [1.5.0] - 2026-08-31
### Fixed
- Conversations appeared to cut off mid-task. The model habitually ended a
  reply with "here is the current structure:" and then proposed the *same*
  listing it had just run, purely to display output it already had. The
  repeat guard correctly refused to run it -- but then stopped dead, leaving
  that dangling half-sentence inside a collapsed Thinking box with no answer
  anywhere. Two fixes: the protocol now tells the model that every command's
  output is already shown to it and to the user, so it must never re-run a
  command just to display or confirm something (quote the output you already
  have; when the work is done, say so in plain text). And when the guard does
  fire, the app now takes one final no-command turn so the user always gets a
  real closing answer outside the Thinking group instead of nothing. Verified
  against the real model: it now writes the structure out from the output it
  already had rather than re-running `ls`.
- Same protocol rule also addresses replies that proposed several listing
  commands at once (seen as "That reply included 5 commands; only the first
  one ran") -- it had just run `ls -F A/ D/ N/ S/ T/`, received all of it,
  then proposed five more listings to show each folder separately.

## [1.4.1] - 2026-08-31
### Fixed
- A model asked to move files "to root folder" targeted a granted path
  instead of the working folder itself -- the granted path's own note said
  "source code" and the user's phrasing said "root", and nothing in the
  per-turn prompt said which one "root" actually meant. The sandbox
  correctly blocked the write either way (the granted path was read-only,
  so nothing was at risk), but the model still picked the wrong
  destination. `build_root_note` now explicitly states the open working
  folder is the "root"/home context for the session, and that a granted
  path isn't "root" unless the user names it specifically.

## [1.4.0] - 2026-08-31
### Fixed
- A command output block's badge/Copy button centered vertically across the
  whole block when the command text wrapped onto multiple lines, instead of
  sitting next to the first line -- `align-items: center` on the flex
  summary row.
- The confirmation checklist only split a compound command on `&&`/`;`, not
  bare newlines -- a model writing one command per line (common for a
  sequence of `mkdir`/`mv` steps) got it run as one atomic `sh -c` block
  instead, and a failure partway through could be masked by a later line's
  success (`sh -c` reports the last command's exit code, not the first
  failure). This caused a real ~10-turn confused recovery in one session: a
  `mv` into a not-yet-created folder failed, but the block still reported
  exit 0. Now newlines split into checklist steps too, run one at a time
  with accurate per-step feedback -- unless a line is a shell control-flow
  keyword or comment/shebang, in which case it's kept as one atomic block
  as before, since a real script's lines aren't valid commands on their own.
- When a chain of automatic steps stopped short (Stop button, the
  `max_auto_steps` cap, or an aborted/failed request) only a UI message was
  shown -- the model itself had no record that anything was interrupted, so
  its next reply could wrongly assume the last action either finished or
  never happened. All three cases now also push a note into the actual
  conversation history sent to the model.

### Added
- `rmdir` is now soft-delete shimmed the same way `rm` already was --
  previously it bypassed `.temp-trash` entirely and really removed its
  target (low risk in practice since `rmdir` only ever touches already-empty
  directories, but inconsistent with the rest of the app's "nothing is ever
  truly gone" guarantee). It now always redirects to trash regardless of
  contents, same as `rm`, which does mean it no longer fails on a non-empty
  directory the way real `rmdir` would.

## [1.3.1] - 2026-08-31
### Fixed
- The new hardcoded protocol prompt (1.3.0) dropped the worked example that
  used to show the one-line-explanation-plus-fenced-block format -- a real
  log showed the model regress to replying with a bare command as plain
  text (e.g. just "ls -F") for simple queries, which never actually ran
  since there was no fence to parse. Restored a compact example and an
  explicit "this applies even to a one-off `ls`" note. Verified via headless
  mode across all three rules configurations (protocol only, blank
  general/command files, default) -- every reply now correctly fences its
  command.
- The protocol now explicitly tells the model to ignore `.temp-trash/`
  entirely (it's the app's own soft-delete area, not user content) --
  a real log showed an alphabetize script trying to move it into a lettered
  folder and failing with a "same file" error, adding confusing noise to
  an already-struggling multi-attempt task.
- `max_auto_steps` (and other config) is re-read from disk right before
  every new user-initiated turn in the GUI, not just at startup/after a
  Settings save -- the Rust side already reloaded config and rules fresh on
  every `send_message` call, but the frontend's own cached copy (used for
  the auto-continue step cap) could still be stale after an external edit
  to `config.toml`.

### Verified
- Compared "what is inside?" and a multi-step organize task across
  disable_builtin_rules=true, blank rules.md/command-rules.md, and the
  shipped defaults via headless mode against the real model. Without the
  advisory general/command rules, the model reliably over-explored well
  past what was asked (in one run, proposing an unrequested
  `mv junk.txt .temp-trash/`); with the defaults, it gave one clean answer
  and stopped. The organize task's script -- now written as one
  self-contained unit per the protocol's script guidance -- correctly
  skipped both a subdirectory and `.temp-trash` and ran clean in a single
  confirmation, verified by executing it directly against a copy of the
  test folder.

## [1.3.0] - 2026-08-31
### Added
- A minimal, hardcoded "protocol" is now always sent regardless of any other
  setting: the fenced-```sh-block command format, one command per reply,
  sudo/root always failing here -- and new guidance to write a multi-step
  task as one self-contained script in a single block and run it once,
  rather than proposing many small commands that can leave things half-done
  if something fails partway (this already worked well when a model did it
  on its own; now it's actually suggested). This is the mechanical contract
  the app's own parsing/execution code depends on, so it can't be edited
  away by accident.
- New "Disable built-in General/Command rules" checkbox in Settings ->
  General (`disable_builtin_rules` in config, default off). When on, only
  the hardcoded protocol above plus your own system prompt are sent --
  `rules.md`/`command-rules.md` are skipped entirely (still saved on disk,
  just not sent) for full manual control over behavior.

### Changed
- `rules.md`/`command-rules.md` are trimmed to roughly half their previous
  length. Everything the app itself depends on (command format, one command
  per reply, sudo handling) moved into the new hardcoded protocol above;
  what's left is purely advisory judgment-call guidance (search before
  guessing, re-verify stale listings, quoting, absolute paths for granted
  locations, `mv`/`cp` argument semantics) -- safe to edit or discard
  entirely via the new toggle without breaking the app. Dropped a few
  narrow bullets (OS-detection guidance, "use the user's exact flags") that
  duplicated more general ones already covered elsewhere.

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
