# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.18.0] — 2026-09-04

### Changed
- Deleting a persona now moves it into `personas/.trash/` instead of removing it -- a real session lost one to a misclick with no way back
- Deleting a persona now also confirms afterward where it went, not just before
- The `--server` favicon is now a cropped close-up of just the character's face, not the full app icon shrunk down -- much more legible at actual favicon size
- The GUI window now gets the new app icon at runtime, not just the packaged `.deb`/AppImage -- a dev binary run directly has no desktop entry for the window manager to read an icon from at all, so it fell back to a generic default
- Dialog textareas (persona/ruleset editors, Settings' rules/system-prompt fields) now only resize vertically -- dragging one wider used to fight the dialog's own fixed width instead of ever actually changing it

### Fixed
- The state-update turn never actually saw the user's own message, only its own prior reply -- despite its own prompt already saying "given the exchange." Anything the user conveyed themselves (an action or intention wrapped in `//...//`, the same narration convention replies use) was invisible to it entirely. Now included, with explicit "you"/persona-name-means-the-persona, "I"/"me"-means-the-user attribution so it isn't misread backwards
- A real session showed dispatch correctly load a needed ruleset (`web-search`) and then immediately answer "none" instead of actually using it, wasting the round trip. Dispatch now gets one extra, more forceful nudge specifically for that exact moment ("you just loaded this because it applies -- use it now") instead of accepting "none" right after a fresh load. Improves reliability but isn't a full fix -- live-tested against the same real session's model, which still sometimes skips or abandons a tool request even with the nudge; this is a small local model's own decision-making, not something the app can fully guarantee

## [1.17.0] — 2026-09-04

### Changed
- New app icon and `--server` favicon

## [1.16.0] — 2026-09-03

### Added
- Chat mode's turn 1 reply now streams in live (GUI and `--server`), instead of appearing all at once after the full reply is generated
- New Chat setting: "show a reply's own text growing live" (on by default) — turning it off keeps replies masked behind the thinking placeholder like before, without affecting live reasoning display
- New General setting: `--server` login sessions now expire (7 days by default, 0 = never) instead of living until the process restarts

### Changed
- The live streaming view strips `//`/`||` narration markers from what's shown — display only, the stored/dispatched reply is unaffected
- Tabs (Settings, and the "Remembered state" dialog) now highlight the active tab as a full filled chip instead of just an underline, same look in the GUI and `--server`
- The build hash shown in Settings/the startup log is just the git commit hash now, no more `-dirty-<hash>` suffix when the tree has uncommitted changes

### Fixed
- A live reply bubble rendered as an empty padded pill for the moment before the first streamed chunk arrived
- `--server`'s login sessions grew unbounded for the life of the process — no expiry, no logout, nothing ever removed a token once issued. Now expired on a per-check basis plus an hourly sweep for sessions nobody ever presents again

## [1.15.0] — 2026-09-03

### Added
- Chat mode's persistent state is now two-tier: `state.json` (raw source of truth) plus a derived `state.md` summary
- "Remembered state" dialog splits into Raw JSON and Summary tabs

### Changed
- Turn 1's reply returns immediately; dispatch and state-update now run as a background follow-up
- State-update JSON is awaited before dispatch, so image-prompt fences see this turn's fresh state
- "State updated" badge now means "triggered", shown beside the bubble instead of overlaid on it
- Thinking placeholder stays compact until real `<think>` content actually arrives
- Log timestamps use local time instead of UTC
- Dispatch-turn evaluations are now logged at INFO, not just their outcome
- General/command rules log once, lazily, on first operation-mode use instead of on every GUI launch

### Fixed
- Persona stat fields (e.g. Arousal %) could leak into visible narration text
- A resolved "thinking" bubble didn't survive the narration-visibility toggle
- A pending thinking/generating/searching placeholder got orphaned by the same toggle
- Few-shot leakage from the state-update prompt's own example field names
- CLI could drop an in-flight state update if the process exited first
- Stale `.details.remove()` calls left over from the thinking-placeholder refactor

## [1.14.1] and earlier

See git history/tags for details — this changelog was trimmed to just the current release going forward.
