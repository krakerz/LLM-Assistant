# LLM Assistant

A desktop chat app that lets a local LLM act on files in a folder you
choose -- sandboxed, so it can never see or touch anything outside it
without your explicit say-so.

Point it at any OpenAI-compatible endpoint (LM Studio, Ollama, ...), pick a
folder, and chat. There's no fixed tool-call schema: the model proposes a
shell command when it wants to do something, and the app decides whether to
run it automatically or ask you first.

## Why sandboxed, not just "trust the model"

Letting an LLM run arbitrary shell commands is risky by default. Instead of
trying to guess from the command *text* whether it's safe (unreliable --
see [`bwrap(1)`](https://github.com/containers/bubblewrap)), this app makes
"outside the folder" structurally impossible:

- Commands run inside a [`bubblewrap`](https://github.com/containers/bubblewrap)
  sandbox that can only see the selected working folder, plus any paths
  you've explicitly granted -- nothing else on the filesystem. The
  sandboxed shell also gets a fully cleared environment, not the app's own.
- Read-only commands (`ls`, `cat`, `grep`, `find`, `uname`, ...) run
  immediately; anything else -- writes, deletes, pipes/redirects -- shows
  you the raw command plus the model's own explanation and waits for your
  approval.
- Deletions are never permanent: `rm` inside the sandbox is redirected to
  move targets into `.temp-trash/` (preserving their original layout)
  instead of actually deleting them.
- `sudo`/`su`/`doas`/`pkexec` are detected and never run -- a sandboxed
  shell can't gain root here regardless of approval -- so you're shown the
  command to run yourself instead of a prompt that would just fail.

## Features

- **Sandboxed execution** -- see above.
- **Works with or without a folder open** -- chat as a plain assistant, or
  select a folder to enable file/command access.
- **Automatic follow-through** -- after a command runs, the assistant
  automatically responds to the result (e.g. summarizing a file it just
  read) instead of leaving raw output as the last thing shown. Intermediate
  steps collapse into a "Thinking" section so a long back-and-forth doesn't
  clutter the chat; only the final answer stays inline.
- **Granted paths** -- give the assistant read (or read-write) access to
  specific folders outside the working directory, with an optional note on
  *why* so it can use them proactively, and a recursive vs. top-level-only
  toggle.
- **Configurable safety rails** -- how many automatic steps can chain
  before it waits for you, which programs are always auto-approved, and the
  full "working rules" (command format, confirmation behavior, etc.) are
  all editable in Settings, kept separate from your customizable system
  prompt so tweaking one can't silently break the other.
- **CLI modes** -- preload a folder (`llm-assistant <folder>`) or skip the
  GUI entirely (`llm-assistant <folder> <message>`) for scripting and quick
  testing.
- **Debug logging** -- `logs/app.log` plus a plain-text mirror of each GUI
  conversation (`last-chat.log`, cleared every launch, thinking steps
  included) for troubleshooting without having to expand anything in the
  UI.

## Requirements

Linux only for now -- the sandbox is built on `bubblewrap`, which relies on
Linux namespaces with no equivalent on Windows/macOS yet.

- `bubblewrap` (`bwrap`)
- `webkit2gtk-4.1` and `gtk3` (Tauri's runtime dependencies)
- A local OpenAI-compatible LLM server -- [LM Studio](https://lmstudio.ai/)
  or [Ollama](https://ollama.com/) both work

## Building

```sh
cargo install tauri-cli --version "^2"   # once
cargo tauri build --bundles deb          # produces the binary + a .deb
```

`cargo tauri dev` runs it with live reload. See `CLAUDE.md` for the full
command reference, including a known `--bundles appimage` caveat on very
new/rolling-release distros.

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

### Command line

```sh
llm-assistant                              # launch the GUI
llm-assistant /path/to/folder              # launch the GUI with a folder preloaded
llm-assistant /path/to/folder "message"    # headless: run one turn, print the result, exit
llm-assistant --help
```

## Configuration

Everything lives under `$XDG_CONFIG_HOME/llm-assistant` (usually
`~/.config/llm-assistant/`):

| File            | Purpose                                                        |
| --------------- | ---------------------------------------------------------------|
| `config.toml`   | Endpoint, model, API key, temperature, granted paths, etc.     |
| `rules.md`      | The working rules read before your system prompt every turn.   |
| `logs/app.log`  | Internal debug log.                                            |
| `last-chat.log` | Mirror of the last GUI conversation, cleared on each launch.    |

## License

[MIT](LICENSE)

---

Built with the help of AI (Claude Code), and tested against a local LM
Studio setup running a Gemma-based uncensored model. Behavior with other
local models may vary.
