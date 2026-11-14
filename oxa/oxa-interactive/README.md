# oxa-interactive

The interactive mode of `oxa`: pi's TUI front end (regular and fullscreen layouts) over
`oxa_core::Session`. Entry point: `oxa_interactive::run(session, opts) -> Result<i32, OxaError>`
(called by `oxa-cli`, D13).

```text
UI thread: Terminal + Renderer + ViewModel + Editor/DialogHost/selectors
   keys -> AppAction -> SessionCmd --mpsc--> worker thread (owns Session)
   WorkerMsg::Event(AgentEvent) <--mpsc--    Session::prompt(.., sink, UiProxy)
   UiRequest <--mpsc-- UiProxy (blocks the worker until answered)
Any thread: oxa_core::Handle::{steer, follow_up, abort, abort_retry}
```

| Module | Owner | Provides |
|---|---|---|
| `lib.rs`, `model.rs`, `views/mod.rs`, `selectors/mod.rs` | architect (frozen, complete) | `ViewModel`, `Block`, `ToolBlock`, `StatusModel`, `Activity`, `ViewSettings`, `SessionCmd`, `WorkerMsg`, `UiRequest`, `AppAction`, `SelectorKind`, `Reopen`, `InteractiveOpts`, `ViewCx`, `render_block`, `Outcome` |
| `app.rs`, `regular.rs` | D1 | `run`, `run_loop`, `worker_loop`, `apply_event`, `blocks_from_entries`, `frames`, `transcript_lines`, `status_from`, `RegularPrinter` |
| `fullscreen.rs` | D2 | `Viewport`, `ScrollbarMode`, `compose_fullscreen` |
| `select.rs`, `search.rs` | D3 | `Selector`, `selection_text`, `highlight_selection`, `TranscriptSearch`, `highlight_matches` |
| `views/messages.rs` | D4 | `render_user`, `render_assistant`, `render_compaction`, `render_branch_summary`, `render_custom`, `render_bash`, `render_shown`, `render_notice` |
| `views/tools.rs` | D5 | `render_tool`, `tool_header`, `tool_result_lines`, `render_diff`, `collapse_lines` |
| `chrome.rs` | D6 | `render_footer`, `render_header`, `activity_text`, `title_text`, `editor_border_token`, `key_hints` |
| `selectors/{model,thinking,theme,settings}.rs` | D7 | `ModelSelector`, `ScopedModelsSelector`, `cycle_model`, `ThinkingSelector`, `cycle_thinking`, `ThemeSelector`, `SettingsSelector`, `setting_items` |
| `selectors/sessions.rs` | D8 | `SessionPicker`, `SessionPick`, `relative_age` |
| `selectors/tree.rs` | D9 | `TreeSelector`, `TreeFilter`, `TreePick`, `ForkPointSelector` |
| `dialogs.rs` | D10 | `UiProxy` (the TUI's `Ui`), `DialogHost`, `access_prompt`, `ask_trust`, `ask_missing_cwd`, `cycle_mode` |
| `slash.rs` | D11 | `dispatch_slash`, `INTERACTIVE_COMMANDS`, `hotkeys_lines`, `session_info_lines`, `engine_lines`, `debug_dump` |
| `external.rs`, `clipboard.rs` | D12 | `edit_externally`, `suspend_process`, `resolve_editor`, `copy_text`, `choose_clip_method`, `read_clipboard_image`, `pasted_paths` |
| `selectors/mcp.rs` | F7 | `McpSelector`, `McpServerRow`, `McpRequest`, `mcp_rows`, `apply_mcp_request` |

## Cross-task contracts (frozen)
1. **One-way data flow.** `AgentEvent` -> `apply_event` -> `ViewModel` -> `render_block` ->
   lines. Views and selectors are pure functions of data + `Theme` + width; none of them calls
   the session, the store or the file system (clipboard/external/app excepted).
2. **Effects are data.** Slash commands, selectors and keys return `AppAction` / `Outcome<T>`;
   only the app loop (D1) carries them out (`SessionCmd` to the worker, settings writes,
   clipboard, export).
3. **The wire is not ours.** Nothing here builds or edits a message. Mode, footer, notices and
   widgets are display only (W3). A reply that failed or was aborted is a `Block::Shown` (W2).
   Tool durations and exit codes come from `ToolOutput::ui` / events, never from wire text (W4).
4. **`Ui` across threads.** The worker's `Ui` is `UiProxy`; an unanswered or dismissed dialog
   is `None` / `Answer::Deny`. The permission prompt text is `access_prompt` (shared with RPC).
5. **Regular mode prints finished blocks once** (`RegularPrinter` + `Renderer::commit`).
6. **Session swaps** go through `InteractiveOpts::reopen` on the worker thread.
7. Keys only through `oxa_config::Keymap`; colours only through `oxa_tui::Theme`.

## Test approach
Snapshots at 80 and 120 columns with `Theme::plain()` (and one themed golden per view);
`run_loop` drills with `oxa_tui::TestTerminal` and scripted `WorkerMsg` / `UiRequest` channels;
no engine and no real terminal anywhere in this crate's tests.

## Consumes
`oxa_core::{Session, Handle, State, Stats}`, `oxa_session::{Tree, SessionMeta, SessionRef,
SessionStore, SessionSort, search_sessions}`, `oxa_config::{Settings, Keymap, Dirs}`,
`oxa_mcp::McpManager`, `oxa_export` (for `/export`), `oxa_tui::*`, `oxa_proto::*`.

## Worker (post-wave-E, lane L4)
- The worker waits at most `WORKER_IDLE_TICK` (50 ms) for a command, then calls
  `Session::serve_host` (no claim when nothing is pending): host requests made while idle are
  served and a turn an extension triggered (`sendMessage(.., {triggerTurn: true})`) runs with
  the events of a prompt.
- A key an extension bound (`SessionInfo::shortcuts`) that has no built-in binding is sent as
  `SessionCmd::Shortcut` (`Session::run_shortcut`); built-in bindings win.
- `WorkerStop` (`worker_loop_with`): the worker's own stop flag; the worker records the handle of
  every session it enters, so quit aborts the current session also during a swap.
- `Activity::Queued` (S2 `engine_queue`): `queued #<pos>` / `queued <wait_s>s`.
- Model switches ask `Session::plan_model_switch` first; over the cap the user picks
  compact first / fork / cancel (`switch_step`, `over_cap_question`, `OVER_CAP_CHOICES`).
