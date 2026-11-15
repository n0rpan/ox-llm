# Sessions

A session is the conversation of one project: the prompts, the replies, the tool calls and their results, the compactions, and the settings that were in force when they happened. Sessions are stored as files, form a tree, and can be resumed, forked, labelled and exported.

## Storage

Session files live under the session directory. Its default is `~/.oxa/agent/sessions`. Change it with `--session-dir <dir>`, the `OXA_SESSION_DIR` variable, or the `sessionDir` setting; in that order of precedence.

Inside the session directory, each project has its own sub-directory. The name is the absolute project path with `/` replaced by `-`, wrapped in `--`. A project at `/home/me/proj` keeps its files in `--home-me-proj--/`.

Each session is one file named after the time it started and its id:

```text
2026-11-13T14-00-00-000Z_<session-id>.jsonl
```

The directories are `0700` and the files `0600`. oxa opens files without following symbolic links, appends each entry, and syncs the file at the end of every turn. A last line cut off by a crash is dropped when the file is read again. One oxa writes a session at a time: it holds a lock (`<session>.jsonl.lock` next to the file) while the session is open. A second oxa that opens the same session stops with `session is open in another oxa (pid N); use --fork to continue separately`; nothing is written. `--fork <id>` (or `/fork`) copies the session into a new file you can continue. The lock belongs to the process, so a crash or `kill` never leaves a stale lock; a leftover `.lock` file is harmless.

With `--no-session` nothing is written. The session lives only for that run, and `OXA_SESSION_FILE` is unset for the bash tool.

## File format

A session file is JSON Lines in the same layout as the earlier agent's session format, version 3. The first line is the header:

```json
{"type":"session","version":3,"id":"...","timestamp":"...","cwd":"/home/me/proj"}
```

Every other line is an entry with an `id` and a `parentId`. The parent links make the file a tree: a fork or a branch adds entries under an earlier one. The entry types are:

| Type | Content |
|---|---|
| `message` | a user, assistant or tool result message; `bashExecution` for `!cmd` output; `system` for the frozen prefix (the system prompt and the tool list) |
| `usage` | token counts of one request |
| `compaction` | a summary that replaced older context, with the files it touched |
| `branch_summary` | a summary of a branch left behind by `/tree` |
| `context_edit` | a change made to the context by an extension |
| `custom`, `custom_message` | data written by extensions or by oxa for its own features (for example a repaired tool call) |
| `label`, `session_info` | a label on an entry, and the session name |
| `model_change`, `thinking_level_change`, `mode_change` | settings that changed during the session |
| `shown` | a reply that was shown but not sent, for example one that was aborted |

Assistant entries keep the exact reply that came from the engine, so a resumed session sends the same bytes again. The frozen prefix is an entry too, so a resumed session does not rebuild the system prompt from the current files. A change to skills, context files or the model starts a new prefix epoch and is recorded as one.

Tools that read the earlier format can read these files: the envelope and the message shape are the same, and the fields oxa adds are additional.

## Resuming

| Form | Result |
|---|---|
| `oxa --continue`, `oxa -c` | the most recent session of the current project |
| `oxa --resume`, `oxa -r` | the sessions picker, to choose one |
| `oxa --session <path or id>` | a session by file path, or by full or partial id |
| `oxa --session-id <id>` | the current project's session with that id, created if it does not exist |
| `/resume` (in the interface) | the sessions picker |

A resumed session replays its recorded prefix and settings. A tool call that was still open when the session stopped gets an "interrupted" result, so the model sees that it did not finish.

If the session belongs to another project, oxa asks whether to fork it into the current directory. Answering no ends the command with "Aborted." and exit code 0.

In the sessions picker, type to filter. The picker shows how long ago each session was used. Ctrl+P toggles the project path, Ctrl+S the sort order, Ctrl+N the named-only filter, Ctrl+R renames the session, and Ctrl+D deletes it. Ctrl+D moves the session file to the trash, using the `trash` command when it is installed. Without that command the file is deleted. See [What oxa stores](#what-oxa-stores-and-how-to-remove-it) for the other ways to remove sessions.

## Forking and cloning

- `oxa --fork <path or id>` copies a whole session, with all its branches, into a new session in the current directory.
- `/fork` starts a new session from an earlier user message. The history before that message is kept, and the chosen message goes back into the editor so you can send it again with changes. Escape twice on an empty editor opens the same choice when `doubleEscapeAction` is `fork`.
- `/clone` duplicates the current session at its current position.

Forks and clones record their parent in the header (`parentSession`).

## Tree navigation

The session is a tree. The active path is the one from the root to the current leaf. `/tree`, or Escape twice on an empty editor, opens the tree navigator:

- Up and Down move between entries; Enter moves the leaf to the selected entry. The next prompt continues from there.
- When you leave a branch, oxa asks whether to summarise it first. The summary is stored as a `branch_summary` entry and is shown in the new path, so the model keeps what was learned there. `branchSummary.skipPrompt` skips the question.
- Selecting the entry you are already on does nothing and says so.
- Ctrl+Left and Ctrl+Right (or Alt+Left and Alt+Right) fold and unfold a branch segment.
- Ctrl+X copies the text of the selected entry.
- Filters: Ctrl+D shows the default view; Ctrl+T hides tool results; Ctrl+U shows only your messages; Ctrl+L shows only labelled entries; Ctrl+A shows everything. `treeFilterMode` sets the filter the navigator opens with.

Moving the leaf does not change what was sent before. The next request follows a different path, so the engine has to process the new path once. oxa shows an estimate of the tokens involved, and warms the cache in idle time when the engine supports it.

## Labels and names

- Shift+L on a tree entry sets or clears its label. Labels are stored as `label` entries and appear in the tree; the labelled-only filter lists them. Shift+T shows the time of each label.
- `/name <text>` sets the session's display name, and `--name` sets it at start. The name appears in the footer and in the sessions picker.

## Export

- `/export [path]` writes the session as one HTML file. The file contains the whole tree, so every branch can be browsed in a browser, and it works without a network connection. A `.jsonl` path copies the session file instead. Without a path oxa chooses a file name in the current directory.
- `oxa --export <session file> [output]` does the same from the command line and exits. The output is HTML unless the output name ends in `.jsonl`.

Nothing is uploaded. An export is a local file, and you decide where it goes.

## Compaction and sessions

A compaction is an entry in the session, not a rewrite of the file. The older messages stay in the file; the next request sends the summary and the recent messages. See [compaction.md](compaction.md).

## What oxa stores, and how to remove it

oxa writes under the agent directory, `~/.oxa/agent` or the directory named by `OXA_AGENT_DIR`. Session files go to the session directory, which can be somewhere else. Files you export or save yourself are yours, and oxa does not track them.

| What | Where | Removed by `oxa wipe` |
|---|---|---|
| session files | `sessions/`, or the session directory | `--session <id>` removes that session's file. `--all` removes every oxa session file in the directory |
| spill files of one session: the full output of a long bash command or tool, and the full text of a long subagent result | `tmp/<session id>/` | with the session (`--session`), and by `--all` |
| codemode scratch files, shared by all sessions | `tmp/codemode/` | by `--all` only |
| pasted images and other files directly in `tmp/` | `tmp/` | by `--all` only |
| `/bug` reports | `bug-reports/` | no |
| drafts from `oxa report-issue draft` | `issue-drafts/` | no |
| the dump written by `/debug` | `oxa-debug.log` | no |
| extension and MCP logs | `extensions.log`, `mcp.log` (and `mcp.log.1` after rotation) | no |

Nothing in the table above is sent anywhere. `oxa wipe` does not touch the settings, `models.json`, `keybindings.json`, `trust.json`, `mcp.json`, the OAuth token file `mcp-auth.json`, `brave-search.key`, `identity.md`, your skills, prompts, themes or extensions, or any file you exported.

```console
$ oxa wipe --session 2f9c1a7e
$ oxa wipe --session 2f9c1a7e --engine
$ oxa wipe --all --yes
```

- `--session` takes a full session id, or a prefix that matches one session. An ambiguous or unknown prefix removes nothing.
- A session forked or cloned from a removed session keeps its own copy of the messages. `oxa wipe` names it and keeps it; remove it with another `--session`.
- `--all` asks you to type `yes` in a terminal. Without a terminal it needs `--yes`, and without either it removes nothing. It keeps any `.jsonl` file in the session directory that is not an oxa session, and names it.
- `--engine` asks the engine to drop the cached state of the removed session or sessions. It is refused in offline mode, and it needs the engine on a loopback address.
- `--session-dir <dir>` names a session directory other than the default.

`oxa wipe` exits with 0 when everything is done. It exits with 1 when an id matches nothing or several, a confirmation is declined, a file is kept, or the engine state is not cleared. It exits with 2 for a usage error, and for `--all` without a terminal and without `--yes`.

A file that the sessions picker moves to the trash is still on the disk until the trash is emptied.

## Bug reports

`/bug` writes a local report bundle: your description, the oxa version, facts about the terminal, and the number of blocks in the transcript. It contains no transcript text and no environment variables. The bundle is written to `bug-reports/` in the agent directory, and nothing is uploaded. Read it before you attach it to a report by hand.
