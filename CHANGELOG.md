# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- A chat setting to let the model skip its reasoning before replying, so replies start sooner (Settings → Chat, "Let the model think before replying"; some servers or models ignore it)
- A button in the chat header (next to the narration eye) to show or hide the character's inner thoughts
- File operations remember earlier tasks in the same folder, across sessions: a new task sees the most relevant and the latest past requests with their outcome and date, and a note when a file they changed has changed or disappeared since (Settings → File Operations)
- Text and code files attached in a chat can be recalled later in that chat, after they have scrolled out of the model's context
- Optional query rewriting for chat recall: one short extra model call turns your message into search queries, which helps with loosely phrased questions (Settings → Chat, off by default; can use a separate small model)
- Image size presets: the character picks portrait, landscape or square, and the pixel sizes come from Settings → Image Gen (used when Width and Height are mapped; a fallback preset applies when she picks none)
- A mappable Seed field for image generation: a new random seed per photo, or the previous photo's seed when you ask for a variation of it
- The file-ops trash is emptied of backups older than 30 days or beyond 2 GB in total, oldest first (`trash_max_age_days`, `trash_max_mb` in config.toml; 0 turns a limit off)
- Photo mode "with every reply": the character sends a photo of what she is doing right now with each message (Settings → Chat, replaces the on/off photo checkbox)
- Persona builder: describe a character in a few fields and the model drafts the card section by section; edit, regenerate single sections, preview and save
- Photos work for male characters; a setting (Settings → Chat, "You appear in photos as") or a "User:" line in the card says how you appear in photos
- Personas made with the builder can be reopened in it later ("Edit with builder")
- Built-in Danbooru tag list for the builder: fixes tag spelling and aliases, finds a character's tag and suggests outfits; a folder with your own tag files can replace it (Settings → Chat)
- Optional online lookup for tags the local list doesn't know (off by default)
- The persona builder can look an existing character up through web search and render a test photo before saving

### Changed
- A new, still empty chat can switch persona; the persona is fixed once the first message is sent
- Chat replies are steered toward short, natural conversation; a persona card can override this
- The message a character sends with a photo now reads as her own line about what you asked ("how do I look?"), not as a comment on the picture
- The desktop window now enforces a Content Security Policy, as `--server` already did
- The Image Gen settings tab uses two columns on wide windows: workflow JSON on the left, field mapping and size presets on the right

### Fixed
- Holding a riding crop no longer turns a photo into a sex scene
- Stat fields copied from the card template no longer end up in her state
- Replies no longer repeat the app's photo note
- Stat lines the model echoes into a reply (Outfit:, Mood:, ...) are removed from the chat
- Stat lines in old saved chats are cleaned up too
- A garment she took off no longer reappears in her outfit unless it is mentioned
- A photo "of us" now shows both of you, even without a sex scene
- An original character's name is no longer used as a photo tag
- Photos always carry the negative tags listed as "always" in the image rules, even when the model leaves some out
- Vague prose in photo prompts ("playful glint in eyes", "afternoon sun", a lone "wearing") becomes a real tag or is dropped
- A selfie no longer comes out "from behind" unless you asked for the back view
- Close-up chat photos no longer show her shoes or socks in the frame
- A remark about the last chat photo no longer triggers a new photo, and the character treats glitches in it as a camera mistake
- A reply that stalls with "are you sure?" after a clear request is regenerated once
- Photos no longer mix in other outfits from the character card
- Photos follow her state more closely: clothes she took off stay off, and the picture matches when her reply or activity says she undressed
- Photo prompts turn descriptive phrases into proper tags and drop names, stray sentences and tags the state doesn't back
- Photos at school grounds or other outdoor spots no longer default to an indoor classroom
- Photos follow the framing the user asks for (close-up, full body, from behind)
- Character tags with parentheses (e.g. `asuna (blue archive)`) reach ComfyUI escaped and intact
- The message after a photo no longer invents another photo or repeats her previous reply
- Opening a chat scrolls to the newest message instead of the oldest
- The character's outfit updates when she agrees to change clothes, so photos show the new outfit
- Photos show her current outfit and equipment state without duplicated or garbled outfit tags
- The character always has a place in her state, so photos no longer get a random background
- Stop now also interrupts the "summarize older messages" step, in chat and in file operations
- Reloading the window while a file-ops confirmation is open now stops that task instead of leaving it waiting
- A folder granted without its subfolders no longer lets the file tools into those subfolders, or create new ones there
- Text typed alongside an attached file is no longer treated as part of that file, so recall finds it and chats are titled from it
- Copy/move/truncate commands with a target-directory or size option no longer back up files they don't touch
- "Always allow" choices and granted folders are no longer lost when a Settings dialog that was already open is saved
- `--server`: changing the endpoint without entering a key no longer sends the saved key to the new endpoint
- Clicking "New chat" twice quickly no longer creates two chats
- Generated photos no longer repeat the same scene; prompts follow the current moment and mood
- Editing the workflow JSON in Settings → Image Gen no longer resets every field mapping to "(not mapped)" while the JSON is half-typed; image generation now refuses to run when the positive prompt isn't mapped, instead of rendering without it
## [2.0.0] — 2026-10-18

### Added
- File operations now work through real tools (list, read, search, find, write, edit, move, delete, make folder, run command) instead of shell commands only
- Edits and overwrites are shown as a diff before you approve them, and every changed file is backed up to the trash
- An append mode for adding to the end of a file
- A "Changed files" summary at the end of each file-ops task, recorded by the app itself
- Native tool calling for models that support it, with automatic fallback to a text format for models that don't
- Running commands can be stopped, and each one has a time limit (Settings → File Operations)
- The character now decides on her own when to send a photo, based on the conversation and her persona card (a selfie-loving persona sends them often, a shy one only when asked)
- `/pic [hint]` and a camera button to ask for a photo directly
- "Let the character send photos on her own" setting in Settings → Chat
- Stop button for chat replies
- Option to unload the chat model while ComfyUI generates an image and free ComfyUI's memory afterwards, for running both on one GPU (only applies when both are on the same machine)
- `--version`/`-v`
- `--server`: logout, a login lockout after repeated failures, and an `allowed_hosts` list in `server.json`
- Long chats recall relevant earlier messages automatically, using keyword search plus an optional embedding model
- Recall can search every chat with the same persona (Settings → Chat)

### Changed
- File-ops mode refuses to overwrite a file the model hasn't read first, and questions a rewrite that would drop most of a file before asking you
- The file-ops loop runs in the app backend, shared by the GUI and the terminal modes
- `--chat` (file-ops terminal mode) asks before running anything that needs approval
- Chat history is kept by the app itself, so switching chats or sending while an image generates no longer mixes up messages
- Chat replies use the common roleplay style: *actions*, "speech" and (thoughts), shown as separate blocks; older chats still display as before
- Web search is decided the same way as photos, and a search that turns out not to help is dropped quietly
- The character's state now always tracks her outfit, location, activity and mood
- The two ruleset files are now plain "Image style rules" and "Search answer policy" texts
- Unmodified default rule files are upgraded to the new defaults
- Icons use the Lucide set
- Unknown command-line options are now an error instead of being sent as a message
- A config file with only some keys set now loads, with defaults for the rest

### Removed
- The ruleset-request step that chat used before generating an image or searching

### Fixed
- Photo, search and state checks work with thinking models (reasoning is turned off for those short checks)
- Long file-ops tasks no longer lose the current request when the conversation is trimmed
- Commands inside a reasoning model's hidden thinking are no longer run
- `tail -f` or a huge file no longer freezes the app; command output is capped
- Writing a file that contains code blocks no longer truncates it
- ComfyUI errors are reported right away instead of after a 5-minute timeout
- Generated images no longer overwrite each other when two finish in the same second
- Replies with empty content from some servers no longer fail the turn
- A stalled LLM server no longer hangs the app forever

### Security
- `--server` no longer lets a client read arbitrary files or delete directories through crafted image paths or session ids
- `--server` no longer accepts file-ops settings (granted paths, auto-approve) or reveals the API key
- `--server` refuses to start on a non-local address without a password, and a malformed `server.json` is an error instead of disabling login
- Sandbox: shims and read-only grants can no longer be made writable by opening a parent folder; opening `/`, your home folder or the app's own folders is refused

## [1.18.0] — 2026-09-04

### Changed
- Soft-deleted files now move to a location outside the sandboxed folder entirely, instead of a `.temp-trash/` folder inside it that the model kept fetching and listing despite being told to ignore it — the location is configurable in Settings (File Operations tab)
- Trashed files from one approved command now land in a single folder instead of fragmenting into one per individual `rm`/`mv`/etc. sub-command
- When the session record (memory) is on, operation mode now drops a previous completed task's chat history outright once a new task starts, instead of condensing or summarizing it, since the session record already keeps its facts — reduces context usage and how often long sessions need to compact
- Deleting a persona now moves it into `personas/.trash/` instead of removing it -- a real session lost one to a misclick with no way back
- Deleting a persona now also confirms afterward where it went, not just before
- The `--server` favicon is now a cropped close-up of just the character's face, not the full app icon shrunk down -- much more legible at actual favicon size
- The GUI window now gets the new app icon at runtime, not just the packaged `.deb`/AppImage -- a dev binary run directly has no desktop entry for the window manager to read an icon from at all, so it fell back to a generic default
- Dialog textareas (persona/ruleset editors, Settings' rules/system-prompt fields) now only resize vertically -- dragging one wider used to fight the dialog's own fixed width instead of ever actually changing it
- Every dialog now has a viewport-safe max-height with its own scrollbar (previously only Settings did) -- a long model-generated explanation in the command-approval dialog had no cap at all, growing the dialog taller than the screen with the actual command and Approve/Deny buttons pushed out of reach
- The command-approval dialog is now wider and percentage-driven (`min(90%, 720px)`) instead of stuck at the same fixed 560px every other small dialog uses, and the explanation box scrolls on its own (220px cap) so a long plan doesn't have to be scrolled past just to see the command

### Fixed
- The state-update turn never actually saw the user's own message, only its own prior reply -- despite its own prompt already saying "given the exchange." Anything the user conveyed themselves (an action or intention wrapped in `//...//`, the same narration convention replies use) was invisible to it entirely. Now included, with explicit "you"/persona-name-means-the-persona, "I"/"me"-means-the-user attribution so it isn't misread backwards
- A real session showed dispatch correctly load a needed ruleset (`web-search`) and then immediately answer "none" instead of actually using it, wasting the round trip. Dispatch now gets one extra, more forceful nudge specifically for that exact moment ("you just loaded this because it applies -- use it now") instead of accepting "none" right after a fresh load. Improves reliability but isn't a full fix -- live-tested against the same real session's model, which still sometimes skips or abandons a tool request even with the nudge; this is a small local model's own decision-making, not something the app can fully guarantee
- File-ops mode's Clear and Unmount buttons only ever reset the visible chat -- the session record (what was asked, what ran) is fixed for the whole app process's lifetime and nothing reset it, so the next message after either still carried "here's what you asked before and what I did" context from a conversation that was just wiped from view. Both now also reset that record

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
