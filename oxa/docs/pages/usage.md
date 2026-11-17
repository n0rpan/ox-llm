# Using the interface

`oxa` with no print flag opens the terminal interface. It has a transcript, an editor at the bottom, a footer, and selectors for models, sessions, the session tree and settings. This page covers the editor, the keys, the slash commands and the footer. Flags that change the start-up are in [cli.md](cli.md).

## Layout

The default layout is full screen (`--tui-mode fullscreen`, or the `tuiMode` setting). The transcript scrolls above the editor. `--tui-mode regular` keeps the terminal's own scrollback instead: finished blocks are printed once and the editor stays at the bottom.

The transcript shows your messages, the assistant's text, thinking blocks (collapsed or hidden with the keys below), one block per tool call with its output, compaction and branch summaries, bash command output, and notices (warnings and errors from oxa itself).

## The editor

- Enter submits. Shift+Enter or Ctrl+J inserts a new line.
- Up and Down move through the lines of a multi-line entry. At the first or last line they step through earlier and later prompts (history).
- Tab completes. Typing `@` completes file paths in the project.
- The file list opens only on `@`, Tab, or a word that starts with `./`, `../`, `~/` or `/`; never for a quote or an apostrophe in prose. Hidden files are listed when the typed name starts with `.`. Enter sends the text as typed unless you moved the selection with the arrow keys (Tab takes the selected entry).
- Ctrl+- undoes the last edit. Ctrl+Y yanks the last deleted text.
- Pasting a path to an image file attaches it to the next prompt. Ctrl+V pastes an image from the clipboard.

### `@file` and `!cmd`

- `@path` in the editor completes to a path. The path is sent as text, and the model reads the file with its `read` tool.
- On the command line, `@file` arguments attach the file to the first prompt: text files as `<file name="...">` blocks, images as image parts. `oxa -p @notes.md "Summarise this"` is one example.
- `!command` runs a shell command in the project and shows its output in the transcript. The output is sent to the model with your next prompt. The command runs with your own rights and without the bash sandbox. In offline mode it runs through `bubblewrap` with no network, and it is refused when `bubblewrap` cannot be used. See [security.md](security.md#offline-mode).
- `!!command` runs the same way, but its output is not sent to the model.

## Keys

The table lists the default bindings. Every binding can be changed in `~/.oxa/agent/keybindings.json` (see the end of this section). `/hotkeys` shows the bindings in force.

### Editing and input

| Key | Action |
|---|---|
| Enter | submit |
| Shift+Enter, Ctrl+J | new line |
| Up, Down | cursor up and down; history at the ends |
| Left, Right, Ctrl+B, Ctrl+F | move by character |
| Alt+Left, Ctrl+Left, Alt+B | word left |
| Alt+Right, Ctrl+Right, Alt+F | word right |
| Home, Ctrl+A / End, Ctrl+E | line start / line end |
| Backspace | delete backward |
| Delete, Ctrl+D (in the editor) | delete forward |
| Ctrl+W, Alt+Backspace | delete word backward |
| Alt+D, Alt+Delete | delete word forward |
| Ctrl+U / Ctrl+K | delete to line start / line end |
| Ctrl+Y / Alt+Y | yank / cycle the yank ring |
| Ctrl+- | undo |
| Ctrl+] / Ctrl+Alt+] | jump forward / backward to a character |
| Tab | autocomplete |

### Running a turn

| Key | Action |
|---|---|
| Enter while a turn runs | steer: the text is delivered after the running tool calls |
| Alt+Enter | queue a follow-up: delivered when the run finishes |
| Alt+Up | move queued messages back into the editor |
| Escape while a turn runs | abort the turn; queued messages return to the editor |
| Escape twice on an empty editor, idle | open `/tree` (or `/fork`, see `doubleEscapeAction`) |
| Ctrl+C | clear the editor; twice on an empty editor, exit |
| Ctrl+D | exit when the editor is empty |
| Ctrl+Z | suspend to the shell (`fg` resumes) |
| Ctrl+G | edit the prompt in `$VISUAL` / `$EDITOR` (or `externalEditor`) |
| Alt+M | cycle the permission mode: standard, auto, yolo |
| Ctrl+Alt+O | toggle yolo: full access, no prompts, bash without the sandbox |

Queued steering and follow-up messages show above the editor as `Steering:` and `Follow-up:` lines. `steeringMode` and `followUpMode` (see [settings.md](settings.md)) decide whether the queue is sent one message at a time or all at once.

### Display and models

| Key | Action |
|---|---|
| Ctrl+O | collapse or expand tool output |
| Ctrl+T | collapse or expand thinking blocks |
| Ctrl+X | copy the active selection, or else the last assistant message |
| Ctrl+L | model selector |
| Ctrl+P / Shift+Ctrl+P | next / previous model in the scoped cycle (`--models`, `enabledModels`) |
| Shift+Tab | cycle the thinking level |
| Ctrl+S (in a selector) | save the selection as the default |

### Full-screen transcript

| Key | Action |
|---|---|
| Page Up, Page Down | scroll one page |
| Ctrl+Home, Ctrl+End | top; bottom and follow new output |
| Ctrl+Up, Ctrl+Down | previous / next user message |
| Ctrl+Shift+F | search the rendered transcript |
| Enter, Ctrl+G / Shift+Enter, Ctrl+Shift+G | next / previous match while searching |
| Escape | close the search |

Mouse selection copies on release when `fullscreenCopyOnSelect` is on (the default). The scrollbar and the wheel speed follow `fullscreenScrollbar` and `fullscreenWheelScrollLines`.

### Session tree and sessions picker

| Key | Action |
|---|---|
| Ctrl+D, Ctrl+T, Ctrl+U, Ctrl+L, Ctrl+A | tree filter: default, no tools, user messages only, labelled only, all |
| Ctrl+O / Shift+Ctrl+O | cycle the tree filter forward / backward |
| Shift+L | edit the label of the selected entry |
| Shift+T | show or hide label timestamps |
| Ctrl+Left, Alt+Left / Ctrl+Right, Alt+Right | fold or unfold a branch segment |
| Ctrl+P, Ctrl+S, Ctrl+N | sessions picker: show path, toggle sort, named sessions only |
| Ctrl+R | sessions picker: rename |
| Ctrl+D, Ctrl+Backspace | sessions picker: delete (Backspace variant only with an empty query) |

### Changing bindings

`keybindings.json` maps action ids to keys. Each entry is a key or a list of keys:

```json
{
  "app.model.select": "ctrl+m",
  "app.tools.expand": ["ctrl+o", "alt+o"]
}
```

Unknown ids, invalid keys and a key bound to more than one configured action are reported as warnings at start-up. The action ids are the ones in the tables above, such as `tui.input.submit` or `app.session.tree`.

## Slash commands

Type a slash command at the start of the editor and press Enter. Commands with a selector open it when given no arguments.

| Command | What it does |
|---|---|
| `/model [pattern]` | select a model, or switch directly to a matching one |
| `/scoped-models` | choose which models Ctrl+P cycles through |
| `/thinking [level]` | set the thinking level: off, minimal, low, medium, high, xhigh, max |
| `/mode [standard\|auto\|yolo\|plan\|workspace\|ask] [--save]` | set the permission mode; no argument opens the picker; `--save` makes it the default |
| `/auto` | permission mode auto: no prompts; the sandbox and the deny-list stay on |
| `/yolo [on\|off]` | toggle yolo, full access: no prompts, bash without the sandbox, no deny-list or protected paths for file tools |
| `/settings` | open the settings selector |
| `/theme [name]` | choose a theme |
| `/compact [instructions]` | compact the context now, with optional guidance for the summary |
| `/new` | start a new session |
| `/resume` | open the sessions picker |
| `/tree` | navigate the session tree; choose a branch or summarise the one you leave |
| `/fork` | start a new session from an earlier user message |
| `/clone` | duplicate the current session at the current point |
| `/name [name]` | set the session's display name |
| `/session` | show session info and stats |
| `/export [path]` | export the session to HTML (or `.jsonl`) |
| `/copy` | copy the last assistant message to the clipboard |
| `/hotkeys` | show the key bindings in force |
| `/mcp` | list the MCP servers and their state |
| `/trust` | save the trust decision for this project |
| `/engine [status \| load <model>]` | show the engine status or load a model on the engine |
| `/reload` | reload keybindings, extensions, skills, prompts, themes and context files |
| `/debug` | write a dump of the screen and the terminal facts to `oxa-debug.log` in the agent directory |
| `/bug <description>` | write a local bug report bundle with your description and facts about the setup; nothing is uploaded. See [sessions.md](sessions.md#bug-reports) |
| `/web-search` | show the `web_search` status (only when a key is configured) |
| `/help` | show the built-in commands and where the key bindings are listed |
| `/usage` | show the session info and stats, the same as `/session` |
| `/quit`, `/exit` | quit |

Skills (`/skill:name`), prompt templates and extension commands are also typed with a slash. They are listed by autocomplete with the built-ins. `/login`, `/logout` and `/share` are not available in oxa and report an error.

## Footer

The footer has two lines:

- Line 1: the working directory, the git branch in parentheses, and the session name after a bullet.
- Line 2, left: `↑` input tokens and `↓` output tokens for the session, `cache cached/prompt` for the last request, `prefill done/total` while the engine is prefilling, queue position, the context use as a percentage of the context window (`(auto)` when automatic compaction is on), and tokens per second. A `no summary` item appears while the last compaction had no model summary (under `local` oxa then also prints a notice with the number of assistant and tool turns dropped from memory). A `repairs N` item appears after malformed tool calls were repaired ([tools.md](tools.md#repair-turn)). Above 70 % and 90 % the context figure turns yellow and red.
- Line 2, right: the model and the thinking level.

The permission mode is shown on the footer as well. A notice reports each cache miss when `showCacheMissNotices` is on ([compaction.md](compaction.md#cache-misses)).

### The HUD (oxa skin)

With the default `oxa` skin the footer is a single-line HUD instead of two lines. It shows the same status, laid out left to right:

- `OXA` — the brand, always.
- YOLO badge — a flame, `YOLO full access` and the count of auto-approved questions, only in yolo mode.
- The model, with the thinking level and the thinking note.
- `CTX` — a meter and the percentage of the window used, and `used/n_ctx` when the window is known.
- `CACHE` — a meter and the percentage of the last request, when a cache hit is known.
- Decode speed (`tok/s`), only while the engine reports timings.
- The activity badge, always present; `READY` when idle.
- `repairs N` and `no summary`, only when they apply.
- The permission mode (`standard`, `auto`, `yolo`, `plan` or `ask`), always shown, and the profile word when set.

Parts with no data are hidden, and on a narrow terminal parts are dropped in this order until the line fits: the profile word, the decode speed, the cache meter, the `used/n_ctx` figures, `repairs N` / `no summary`, then the context meter shrinks to 5 cells, then the thinking note, the whole `CACHE` part, the thinking level. The permission mode, the model name, the `CTX` percentage, the activity badge and `OXA` are kept; the model name is only cut when nothing else is left.

## Selectors

- Model selector (`/model`, Ctrl+L): the models the engine serves. Enter switches. Ctrl+S saves the choice as `defaultModel`.
- Thinking selector (`/thinking`): the levels the model supports. Ctrl+S saves `defaultThinkingLevel`.
- Settings selector (`/settings`): a list of the common settings.
- Sessions picker (`/resume`): the sessions of the current project, with their age. Type to filter.
- Tree (`/tree`): the session as a tree of branches. See [sessions.md](sessions.md#tree-navigation).
- MCP (`/mcp`): one row per server with its state and exposure. See [mcp.md](mcp.md).

Permission questions appear as dialogs: "Allow once", "Allow for this session" and "Deny". See [security.md](security.md#permission-modes).
