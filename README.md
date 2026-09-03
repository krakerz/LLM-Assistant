# LLM Assistant

A desktop app for talking to a local LLM two ways: sandboxed file operations
in a folder you choose, or a persona/roleplay chat mode with no folder at
all. Chat mode can also run headless as a small web server, so you can leave
it running and reach it from a browser.

Point it at any OpenAI-compatible endpoint (LM Studio, Ollama, ...) and go.
File-ops mode has no fixed tool-call schema -- the model proposes a shell
command when it wants to do something, and the app decides whether to run it
automatically or ask you first.

## Why sandboxed, not just "trust the model"

Letting an LLM run arbitrary shell commands is risky by default. Instead of
guessing from the command *text* whether it's safe (unreliable -- see
[`bwrap(1)`](https://github.com/containers/bubblewrap)), this app makes
"outside the folder" structurally impossible:

- Commands run inside a [`bubblewrap`](https://github.com/containers/bubblewrap)
  sandbox that can only see the selected folder plus any paths you've
  explicitly granted -- nothing else -- with a fully cleared environment too.
- Read-only commands (`ls`, `cat`, `grep`, `find`, `uname`, ...) run
  immediately; anything else -- writes, deletes, pipes/redirects -- shows you
  the raw command plus the model's explanation and waits for your approval.
- Deletions aren't permanent: `rm` inside the sandbox moves targets into
  `.temp-trash/` (preserving their layout) instead of deleting them.
- `sudo`/`su`/`doas`/`pkexec` are blocked outright -- shown as a command to
  run yourself, not a prompt that would just fail.

## Features

- **Sandboxed execution** -- see above.
- **Two chat surfaces** -- File Operations (above) and a separate
  **Chat mode**: no folder, no commands, just a persona (a freeform `.md`
  character sheet you write or import) and as many independent, permanently
  kept conversations as you want. A persona can track whatever it wants to
  persist for the whole conversation -- RPG-style stats are the common case.
- **Serve chat mode over the web** -- `llm-assistant --server` leaves chat
  mode running headless behind a small, optionally password-protected HTTP
  server, sharing the same config and sessions as the desktop app -- see
  "Serving chat mode over the web" below.
- **Image & document attachment in chat mode** -- attach an image for a
  vision-capable model, or a text/code file to fold into the message.
  Settings' "Test vision support" sends a real test image to confirm the
  model can actually see, since most servers don't expose that as metadata.
- **Shows a model's own reasoning** -- a `<think>` block renders collapsed
  under "Thinking" (off by default; doesn't change whether the model
  reasons, just whether you see it).
- **Works with or without a folder open** -- chat as a plain assistant, or
  select a folder to enable file/command access.
- **Automatic follow-through** -- after a command runs, the assistant
  responds to the result instead of leaving raw output as the last message;
  intermediate steps collapse into "Thinking" so long chains don't clutter
  the log.
- **Granted paths** -- give the assistant read (or read-write) access to
  specific folders outside the working directory, with an optional note on
  *why*, and a recursive vs. top-level-only toggle.
- **Configurable safety rails** -- how many automatic steps can chain before
  it waits for you, which programs auto-approve, and the full "working
  rules" are all editable in Settings, kept separate from your system prompt
  so tweaking one can't silently break the other.
- **CLI modes** -- preload a folder, run one scripted turn headlessly, or
  chat in the terminal (`ollama run`-styled). Chat mode has its own terminal
  REPL too (`--persona-chat`) and the headless web server above (`--server`)
  -- see Command line below.
- **Debug logging** -- `logs/app.log` plus a plain-text mirror of each GUI
  chat-mode conversation (`logs/chat-*.log`, one per launch, up to 5 kept,
  thinking steps included) for troubleshooting without expanding anything
  in the UI.

## Requirements

Linux only for now -- the sandbox is built on `bubblewrap`, which relies on
Linux namespaces with no equivalent on Windows/macOS yet. `--server` mode
never touches the sandbox (chat mode only), but still needs the same build
below since it's the same binary.

- `bubblewrap` (`bwrap`)
- `webkit2gtk-4.1` and `gtk3` (Tauri's runtime dependencies)
- A local OpenAI-compatible LLM server -- [LM Studio](https://lmstudio.ai/)
  or [Ollama](https://ollama.com/) both work

## Building

```sh
cargo install tauri-cli --version "^2"     # once
cargo tauri build --bundles deb,appimage   # produces the binary + a .deb and an AppImage
```

`cargo tauri dev` runs it with live reload. See `CLAUDE.md` for the full
command reference, including a known `--bundles appimage` caveat (`strip`
choking on newer binutils' `.relr.dyn` sections; prefix `NO_STRIP=true`) on
very new/rolling-release distros.

## Usage

1. Launch the app and click **Select Folder…** (or pass a folder on the
   command line, see below).
2. Chat normally. When the assistant wants to run something, it shows a
   one-line explanation plus the command; read-only commands just run,
   anything else waits for **Run it** in the approval dialog.
3. Use the gear icon to configure the endpoint, model, API key, temperature,
   how many automatic steps it can chain, granted paths, and the working
   rules/system prompt.
4. **Unmount** drops the folder and goes back to chat-only mode; **Clear**
   resets the conversation.

### Serving chat mode over the web

`llm-assistant --server --bind 0.0.0.0 --port 9333`, then open
`http://<that machine>:9333` in a browser -- same chat mode, config, and
sessions as the desktop app, no file-ops. Set `password` in `server.json`
first unless the network is trusted; empty serves openly with a warning.

### Command line

```sh
llm-assistant                                          # launch the GUI
llm-assistant /path/to/folder                          # launch the GUI with a folder preloaded
llm-assistant /path/to/folder "message"                # headless: run one turn, print the result, exit
llm-assistant /path/to/folder --chat                   # interactive terminal chat, Ctrl+D/Ctrl+C to exit
llm-assistant --persona-chat [--persona <name>]        # persona chat mode's own REPL, no folder/sandbox
llm-assistant --persona-chat --session <id>            # resume an existing session
llm-assistant --list-personas / --list-sessions        # list personas / existing sessions with their IDs
llm-assistant --server [--bind <addr>] [--port <n>]    # serve chat mode over HTTP, 127.0.0.1:9333 by default
llm-assistant --help
```

## Configuration

Everything lives under `$XDG_CONFIG_HOME/llm-assistant` (usually
`~/.config/llm-assistant/`):

| File               | Purpose                                                       |
| ------------------ | -------------------------------------------------------------|
| `config.toml`      | Endpoint, model, API key, temperature, granted paths, etc.   |
| `rules.md`         | The working rules read before your system prompt every turn. |
| `server.json`      | `--server`'s bind/port defaults and password (empty = open). |
| `logs/app.log`     | Internal debug log.                                           |
| `logs/chat-*.log`  | Mirror of a GUI chat-mode conversation, one per launch, 5 kept. |

## License

[MIT](LICENSE)

---

Built with the help of AI (Claude Code), and tested against a local LM
Studio setup running `gemma-4-e2b-it-uncensored-max`. Behavior with other
local models may vary.
