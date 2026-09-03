# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.16.0] — 2026-09-03

### Added
- Chat mode's turn 1 reply now streams in live (GUI and `--server`), instead of appearing all at once after the full reply is generated
- New Chat setting: "show a reply's own text growing live" (on by default) — turning it off keeps replies masked behind the thinking placeholder like before, without affecting live reasoning display

### Changed
- The live streaming view strips `//`/`||` narration markers from what's shown — display only, the stored/dispatched reply is unaffected
- Tabs (Settings, and the "Remembered state" dialog) now highlight the active tab as a full filled chip instead of just an underline, same look in the GUI and `--server`
- The build hash shown in Settings/the startup log is just the git commit hash now, no more `-dirty-<hash>` suffix when the tree has uncommitted changes

### Fixed
- A live reply bubble rendered as an empty padded pill for the moment before the first streamed chunk arrived

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
