# LLM Assistant

A Linux desktop app for working with a small local LLM two ways: a sandboxed file assistant confined to one folder, and a persona chat that can send pictures when the moment calls for it.

## Description

Small local models (1–4B, run through LM Studio, Ollama or any OpenAI-compatible server) are unreliable at following formats and easy to talk into mistakes. This app does the heavy lifting itself so they stay useful:

- **File operations.** The model gets a small set of tools (list, read, search, find, write, edit, move, delete, run a command) instead of a raw shell. The app checks every path against the folder you opened, shows a diff before any change, backs up whatever it overwrites or deletes, and reports which files actually changed. It doesn't rely on the model's word for that. Shell commands still run when needed, inside a [`bubblewrap`](https://github.com/containers/bubblewrap) sandbox that can only see the folder and the paths you granted.
- **Persona chat.** You write a character as a plain `.md` file and chat with her across as many saved conversations as you like. After each reply, the app decides whether she sends a photo, rendered through your own [ComfyUI](https://github.com/comfyanonymous/ComfyUI) workflow. It also decides whether the question needs a web search through your [SearXNG](https://github.com/searxng/searxng). The decision comes from short checks the model answers about the conversation and her character card. The app then verifies each answer, so a photo-happy persona sends pictures often, a shy one only when asked, and a hallucinated reason never triggers anything.

## Features

- **Folder-confined tools.** Paths outside the folder, symlinks that escape it, and the app's own config folder are refused. Granted paths can be read-only or read-write.
- **Changes are previewed and backed up.** Edits and overwrites show a diff in the approval dialog. Anything overwritten, moved over or deleted goes to a trash folder outside the working folder. The model must read a file before it may change it.
- **Approval only where it matters.** Reading, listing and searching run immediately. Changes and non-read-only commands wait for you. "Always allow" for a program, and a fade-out after repeated approvals, cut down on prompts.
- **Commands can be stopped and time out.** Output is capped, so `tail -f` or a huge file can't freeze the app.
- **Works with any model.** It uses native tool calling when the model supports it, and otherwise falls back automatically to a simple text format.
- **Situational photos in chat.** She can send one when you ask in any wording, when she says she's sending one, or when her card says she loves selfies and something visual happens. `/pic [hint]` or the camera button always asks directly.
- **One GPU for both.** Optionally unloads the chat model (LM Studio, Ollama) while ComfyUI generates, when both run on the same machine, and frees ComfyUI's memory afterwards.
- **Web search in chat.** Questions about weather, news, prices and other real-world facts are looked up through SearXNG and answered in character.
- **Persona state.** Her outfit, location, activity and mood, plus any stats her card defines, are tracked across the conversation and feed the image prompt.
- **Roleplay formatting.** Replies use the common style, *actions*, "speech" and (thoughts), and each is shown as its own styled block. The narration toggle hides actions.
- **Chat recall.** In a long chat, older messages that no longer fit the model's context are found again automatically (keyword search, plus an optional embedding model) and shown to it with the next message. Optionally searches every chat with the same persona.
- **Attachments.** Images for vision models, text and code files folded into the message.
- **Serve chat over the web.** `--server` runs chat mode headless behind a password-protected HTTP server, sharing the desktop app's sessions.
- **Terminal modes.** One-shot and interactive file-ops runs, plus a persona chat REPL.

## Installation

Download the `.deb` or AppImage from the [releases page](../../releases).

- The `.deb` pulls in `bubblewrap` automatically.
- For the AppImage, install it yourself: `sudo apt install bubblewrap`, or your distro's equivalent.

You also need a local OpenAI-compatible LLM server. [LM Studio](https://lmstudio.ai/) and [Ollama](https://ollama.com/) both work. For photos, add a [ComfyUI](https://github.com/comfyanonymous/ComfyUI) instance; for web search, add a [SearXNG](https://github.com/searxng/searxng) instance with JSON output enabled.

## Building from source

Prerequisites: Rust (stable), `bubblewrap`, and the `webkit2gtk-4.1` and `gtk3` development packages.

```sh
cargo install tauri-cli --version "^2"     # once
cargo tauri build --bundles deb,appimage   # binary + .deb + AppImage
cargo tauri dev                            # run with live reload
cargo test                                 # sandbox tests skip, loudly, where bwrap can't run
```

On very new or rolling-release distros, `--bundles appimage` can fail inside `linuxdeploy`'s `strip`. Prefix the command with `NO_STRIP=true`.

## Usage

**File operations**

1. Click the folder icon, then **Select Folder…**, or pass a folder on the command line.
2. Ask for something, for example "tidy my notes into folders by topic" or "add eggs to the shopping list".
3. Reads run on their own. Approve or deny each change in the dialog; it shows the diff or the command.
4. When the task ends, a card lists every file that was created, changed, moved or deleted.

**Chat**

1. Pick or create a persona, then start a new chat.
2. Talk normally. Use `/pic` (optionally `/pic at the beach`) or the camera button to ask for a photo directly.
3. Set up ComfyUI and SearXNG under Settings. The puzzle icon edits your image style rules and search answer policy.

**Command line**

```sh
llm-assistant                                        # GUI
llm-assistant /path/to/folder                        # GUI with a folder open
llm-assistant /path/to/folder "message"              # one file-ops task, printed, then exit
llm-assistant /path/to/folder --chat                 # interactive file-ops in the terminal
llm-assistant --persona-chat [--persona <name>] [--session <id>]
llm-assistant --list-personas | --list-sessions
llm-assistant --server [--bind <addr>] [--port <n>]  # chat over HTTP, 127.0.0.1:9333 by default
llm-assistant --help | --version
```

**Configuration** lives in `$XDG_CONFIG_HOME/llm-assistant` (usually `~/.config/llm-assistant/`):

| File | Purpose |
| --- | --- |
| `config.toml` | Endpoint, model, API key, temperatures, granted paths, limits. Missing keys take defaults. |
| `rules.md`, `command-rules.md` | Advisory rules for file operations, editable in Settings |
| `personas/*.md` | Character cards |
| `rulesets/*.md` | Image style rules and search answer policy |
| `comfyui.json`, `searxng.json` | Image generation and web search settings |
| `server.json` | `--server` bind, port, `password`, `allowed_hosts` |
| `logs/` | `app.log` plus a mirror of each GUI session |

## FAQ

**Why Linux only?**
The sandbox is built on Linux namespaces through `bubblewrap`. Windows and macOS have no equivalent to port it to.

**The model never uses the tools / never sends a photo.**
Very small models (about 1B) often ignore formats. Try a 3B+ instruct model, or one with native tool support (Qwen 2.5/3, Llama 3.x).

- File operations show a notice when the app falls back to the text format.
- In chat, `/pic` and the camera button always work, whatever the model decides.
- `logs/app.log` records every photo and search decision.

**Can `--server` be reached from other machines?**
Yes, with `--bind 0.0.0.0`. It then refuses to start without a `password` in `server.json`. For a reverse proxy or a hostname, list the name in `allowed_hosts`. File operations are never exposed over HTTP.

**Where do deleted files go?**
Into the trash folder set in Settings → File Operations (by default under the app's config folder). There's one folder per task or command.

---

### Notes

Built and maintained with the help of AI.
