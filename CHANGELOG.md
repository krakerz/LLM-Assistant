# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
