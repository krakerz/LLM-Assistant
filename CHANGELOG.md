# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] - 2026-08-31
### Added
- Tauri desktop shell with a chat UI for talking to a local Ollama/LM Studio
  endpoint (any OpenAI-compatible `/chat/completions` server).
- Folder-scoped sandboxed command execution via `bwrap`: the LLM's shell
  commands can only see the selected working directory plus any explicitly
  granted paths, not the rest of the filesystem.
- Soft-delete: `rm` is shimmed inside the sandbox to move targets into
  `.temp-trash/` (preserving their relative path) instead of deleting them.
- Read-only commands (`ls`, `cat`, `grep`, `find`, ...) run automatically;
  anything else — writes, deletes, pipes/redirects — requires explicit user
  approval, with an option to always-allow a specific program afterwards.
- Per-folder configuration at `.config/config.toml` (endpoint, model, system
  prompt, temperature, granted paths, auto-approve list).
