# oxa desktop (concept A "Workbench"): build plan

Status: the plan the desktop slices are built from. The per-slice details live in [`slices.json`](slices.json), next to this file. Written 2026-11-21 against `main` at `5e28e63`. Nothing in this plan was compiled or measured; every number is a target or an estimate, and every claim about GPUI is marked where it must be checked first.

Sources read: the approved design note (concept A, recommendation, one-week prototype), the lead web plan, the kanban plan, `AGENTS.md`, `.github/workflows/checks.yml`, `tools/ci/paths.py`, and in `oxa/`: `oxa-proto/src/ui.rs`, `oxa-interactive/src/{lib,model,app,dialogs,driver,slash,command_registry}.rs`, `oxa-config/src/descriptor*.rs`, `oxa-core/src/owner_confirm.rs`, `oxa-cli/src/{main,askpass,dispatch,lead_observation}.rs`, `oxa-cli/src/workers/registry.rs`, `oxa-session/src/{lib,meta,scan,store}.rs`, `oxa-rpc/src/{client,events}.rs`, `oxa-fake`.

## 0. What the code forces (read this first)

These facts were found in the checkout. Each one changes the plan compared with the design note.

| # | Fact in the code | Consequence |
|---|---|---|
| F1 | `checks.yml` runs `cargo clippy --locked --workspace --all-targets` in `oxa/`, and `tools/ci/paths.py` starts the 90-minute macOS build for any change to `oxa/Cargo.toml` or `oxa/Cargo.lock`. | The desk crates are **not** members of the `oxa` workspace. They live in a nested workspace `oxa/desk/` with its own `Cargo.lock`. GPUI never enters `oxa/Cargo.lock`, the oxa clippy job never compiles GPU code, and headless boxes never build it. Path dependencies stay inside `oxa/`, so the product isolation scan passes. |
| F2 | `paths.py` treats `changelog.d/*.md` as "everything changed", and the merge queue always runs the complete oxa workspace. | Z01 teaches the classifier a `desk` output; Z03 gives desk-only trees a fast path in the queue. Without Z03 every desk PR waits for the whole oxa suite in the queue. |
| F3 | Any path that contains `macos` or `darwin` selects the 90-minute macOS job. | Desk files never carry those words in their names: `platform/mac.rs`, not `macos.rs`. The pre-check rejects a PR that breaks this. |
| F4 | `oxa/Cargo.toml` says manifests and the lock file are architect-owned. | Only wave-0 PRs touch `Cargo.toml` and `Cargo.lock`. Every dependency the later waves need is declared in Z02 and Z13. A slice that needs a new crate stops and reports `needs-dep`. |
| F5 | The broker symlinks `oxa-askpass` to `current_exe()` and checks that the askpass peer is that executable; workers, subagents and the local engine child also re-run `current_exe()`. | `oxa-desk` must **be** an oxa: its `main` is the three-line shim of `oxa-cli/src/main.rs` (askpass entry, `child_entry`, `oxa_cli::run`) plus one branch that opens the window. `oxa-desk --version` equals `oxa --version`. This is checked in CI from Z01 on. |
| F6 | `oxa-interactive` already separates the front end from the session: `SessionCmd`, `WorkerMsg`, `UiRequest`, `AppAction`, `ViewModel`, `apply_event`, `blocks_from_entries`, `worker_loop_with`, `dispatch_slash`, `render_block` are public. `Loop::apply` in `driver.rs` (carrying out an `AppAction`) is private and bound to the terminal. | The desktop reuses the public parts as they are. Z07 moves the terminal-free effects out of `driver.rs` so the desktop does not copy them. |
| F7 | `WORKER_IDLE_TICK` wakes every idle session thread four times a second. | Z06 lets a front end run the worker with no periodic wake. Without it the stop condition "no steady wakeups" fails by design. |
| F8 | `Ui::secret(title, body)` returns a `String` and knows neither the command nor the pid. `UiRequest::Secret` replies with `Option<String>`. No `zeroize` or `mlock` exists in the workspace. | Z08 adds the request context and a zeroised answer type. The sheet can then show the exact command, and the secret has one locked home from key press to the broker's write. |
| F9 | `FsStore::list` reads and decodes every session file (`all_text` for search). | Z09 adds a light index (header line and a bounded tail) and one shared thread order. Cold start with thousands of sessions depends on it. |
| F10 | `oxa-rpc` writes events as JSON (`event_to_json`) but cannot read them back. | Z10 adds `event_from_json`, so a remote session feeds the same reducer as a local one. |
| F11 | The command registry is partial ("legacy command tables remain"): `Surface` is `Cli, Slash, Menu, Web`; share, cloud keys and the board verbs are not declared. | Z04 adds `Surface::Desk` to every declaration. ZL2 declares the verbs the panes call. Slash commands keep going through `dispatch_slash`, the one existing dispatcher. |
| F12 | `oxa_cli::run(argv, env, Stdio)` takes injectable streams and a working directory. | Read-only panes call the versioned JSON commands in process through it (`oxa usage --json`, `oxa models --json`, `oxa status --json`, `oxa work status --json`), and through `ssh` for another host: one code path per command, no second parser of oxa's files. |
| F13 | Neither `lead_observer.rs` (lead-web S4) nor `oxa-work/src/kanban` (kanban K1) is on main. | The lead and board panes are wave 5 and wait for ZL1 and ZL2, which wait for those slices. Waves 1 to 4 do not depend on them. |
| F14 | `AGENTS.md`: branches are `agent/<task-id>-<slug>`, and "the latest Codex review on the exact head decides". | This plan uses `desk/<slice-id>` and an `oxa-review` line written by the review script (section 6). Both are changes of repository policy, so Z00 writes them into `AGENTS.md` first (a policy PR carries its rules). Until Z00 is merged, nothing else starts. |

Owner decisions:

1. Nested workspace `oxa/desk/` instead of members of the oxa workspace (F1). Recommended; `AGENTS.md` states it from Z00 on.
2. `desk/<slice-id>` branches and the `oxa-review` line as the review that counts for desk slices (F14). Decided: oxa writes the review. The review script runs it and, when it wants a security review, starts an independent one itself with oxa and a ChatGPT-plan model; it does not go through the lead (section 6).
3. The queue fast path for desk-only trees (Z03). It changes `.github/`, which is security core. Decided in the Z03 PR.
4. `gpui-component` (third party, Apache-2.0 as far as I know) for the multi-line editor. Decided: use it if it is a trusted source; Z02 checks the publisher, the licence and the pinned version and records them in `gpui-notes.md`. Otherwise the editor is ours, in the kit. The secure field is ours either way.
5. If the gate fails: Slint under its royalty-free desktop licence (terms to be read then). Open until the gate.

## 1. Architecture

### 1.1 Crates

```
oxa/desk/                    nested cargo workspace, own Cargo.lock, profiles copied from oxa/Cargo.toml
  Cargo.toml                 [workspace] members; [workspace.dependencies]; architect-owned
  gate.toml                  thresholds of the prototype gate and of CI (data, read by the probe)
  oxa-desk-core/             store and view models. No GPUI type. Depends on the oxa crates.
  oxa-desk/                  GPUI app. lib (views, kit) + bin `oxa-desk`. Depends on oxa-desk-core and gpui only.
  oxa-desk-probe/            measurement harness, bin `oxa-desk-probe`. Never shipped.
```

Dependency rule, checked by `tools/ci/desk_contract.py` in the desk job:

- `oxa-desk-core` may depend on `oxa-proto`, `oxa-config`, `oxa-session`, `oxa-core`, `oxa-interactive`, `oxa-tui` (theme, markdown, fuzzy only), `oxa-rpc`, `oxa-http`, `oxa-cli` (lib, with the features of the release `oxa`), `notify`, `libc`, `serde`, `serde_json`. It may not depend on `gpui`.
- `oxa-desk` may depend on `oxa-desk-core`, `gpui`, the accepted widget crate, and `oxa-cli` **only in `main.rs`** for the shim. Its `src/` (outside `main.rs`) may not name `oxa_core::`, `oxa_session::`, `oxa_cli::`, `std::process::Command` or `std::fs::write`.
- `oxa-desk-probe` may depend on `oxa-desk-core`, `oxa-session`, `oxa-fake`.

How the build is kept out of the default build and the release until the final slice:

| Gate | Where | Flipped by |
|---|---|---|
| Not a member of the oxa workspace | `oxa/desk/Cargo.toml` has its own `[workspace]` | never (it stays separate: headless boxes never build GPU code) |
| Registry entry `Desk` is `cli: Exception("The desktop app is not shipped yet")` | `command_registry.rs` (Z04) | F10 |
| Release workflows do not attach desk artifacts | `release.yml` | F10 (F02 and F03 build them earlier without attaching) |
| The owner tries release candidates | `desk.yml` dispatch `rc` (B06), installed on the laptop by hand | n/a |

There is no cargo feature in a shared crate: a feature in `oxa-cli` would change the oxa lock and build matrix for a launcher of thirty lines. The registry availability is the flag.

One version: `oxa/desk/Cargo.toml` copies the workspace version; a step in the desk job fails when it differs from `oxa/Cargo.toml`, and when a crate that both lock files hold has different versions in them.

### 1.2 How it plugs into the existing crates

| Need | Existing seam | Desk use |
|---|---|---|
| Build a session with all flags, trust, hooks, MCP, limits | `oxa_cli::run` -> `run_tui` in `dispatch.rs` | `oxa_cli::run_with(argv, env, io, &mut dyn Frontend)` (Z05). The desktop passes a front end instead of the TUI. Nothing else of the build path changes. |
| Own the session on a thread, take commands, emit events | `oxa_interactive::worker_loop_with`, `SessionCmd`, `WorkerMsg` | `links::inproc` runs it unchanged on a session thread (B02, C05). |
| Dialogs from the core | `oxa_proto::Ui` -> `UiProxy` -> `UiRequest` | The worker's `UiProxy` is the `Ui`. `UiRequest`s become asks (A10). The desktop does not implement `Ui` a second time. |
| Display state | `ViewModel`, `apply_event`, `blocks_from_entries` | `transcript::model` is a thin wrapper (A05). |
| What a block looks like | `views::render_block(block, &ViewCx) -> Vec<String>` | Called as is; the SGR codes are parsed into spans (A06). Same text as the TUI by construction. |
| Slash commands | `dispatch_slash(line, &SlashCx) -> Option<AppAction>` | Called as is (C03). |
| Carrying out an `AppAction` | private `Loop::apply` | `effects.rs` (Z07) + `actions::exec` (C04). |
| CLI commands and their surfaces | `command_registry::{COMMANDS, entries, Surface}` | `Surface::Desk` (Z04); the palette lists `entries(Surface::Desk)` (C07); `CliPort` runs them (C09). |
| Settings | `descriptor::{descriptors, in_group}`, `descriptor_value::{current, display_value, parse_input, check, search}` | The settings rows are generated from these (D01). No form is written by hand. |
| Thread list | `FsStore`, `SessionHeader.parent_session`, `SessionSort::Threaded` | `FsStore::thread_index` and `thread_order` (Z09) feed the tree (A01, A02). |
| Workers under a thread | `oxa_cli::workers::registry::{load, state, WorkerRecord}` | Read as is (A03, B03, D11). |
| Secrets | `OwnerConfirm::serve` -> `Ui::secret` -> askpass | `Ui::secret_ask` with context and `SecretValue` (Z08); `SecretBuf` (A09); the sheet (A15). |
| Remote session | `oxa --mode rpc`, `RpcClient`, `event_to_json` | `links::rpc` over `ssh` with `event_from_json` (Z10, E02, E03). |
| Lead, board | `oxa lead serve` routes, `/events/live`; kanban store and `oxa lead board --json` | `/frame.json` (ZL1) and the board command (ZL2) through `hosts::lead_http` and `CliPort` (E04 to E09). Local and remote use the same path. |
| Usage, models, status, share | versioned JSON commands | `CliPort` (C09) and the pane models (D05 to D11). |

### 1.3 Threads and event flow

```
 background threads (all block; none polls)                    UI thread (GPUI foreground)
 ------------------------------------------                    ---------------------------
 session thread   worker_loop_with ── WorkerMsg, UiRequest ─┐
 watcher thread   notify (inotify/kqueue) ── file events ───┤
 index loader     thread_index batches (ends when done) ────┤   Feeder ──▶ Inbox ──▶ Store::drain() ──▶ Changed
 host reader      ssh stdout lines / lead live route ───────┤      │                       │
 cli runner       oxa_cli::run output (one shot) ───────────┘      └─ wake() once per       ├─ view A (THREADS)   cx.notify()
                                                                      empty→non-empty       ├─ view B (TRANSCRIPT of thread X)
 user input ── view ── Action ──▶ Store::dispatch() ──▶ LinkCmd / effect / CliPort          └─ others: nothing
```

Rules that make idle cost zero:

- One inbox. `Feeder::send` wakes the UI thread only when the inbox was empty. The pump drains everything, applies it, and notifies only the views whose `Changed` bits are set. A burst from twenty workers is one drain and at most one frame per display refresh.
- No timer anywhere except: one age relabel per minute while the window is visible and focused; one deadline while a timed ask is visible. No cursor blink by default (`oxa.desk.blinkCursor`).
- Hidden or occluded window: inputs still apply; no view is notified; no frame is requested (B07).
- Session threads block on their command channel (Z06). Reader threads block on a pipe or a socket. The watcher blocks in the kernel.
- A view reads the store by range (`rows(range)`), never the whole list. A transcript holds a bounded tail and pages backwards.

### 1.4 Store API (the contract agents code against)

Z13 lands these as real code. Signatures are final; a slice fills bodies in the file it owns. `Arc<str>` keeps rows cheap to clone for the UI.

```rust
// ---- ids.rs -------------------------------------------------------------------------------
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostId(pub Arc<str>);
impl HostId { pub fn local() -> HostId; pub fn is_local(&self) -> bool; }

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThreadId { pub host: HostId, pub session: Arc<str> }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AskId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaneId { Thread, Board, Lead, Workers, Usage, Models, Share, Settings }

// ---- input.rs -----------------------------------------------------------------------------
pub enum Input {
    Threads(ThreadsInput),                         // index batches, file changes, registry changes
    Link(ThreadId, LinkEvent),                     // from any SessionLink
    Ask(ThreadId, oxa_interactive::UiRequest),     // from the session thread's UiProxy
    Panel(HostId, PanelInput),                     // board, lead, workers, usage, models, share
    Host(HostId, HostInput),                       // connection state, latency
    Command(CommandResult),                        // output of a CliPort run
    Settings(SettingsInput),                       // settings file changed
    Visibility(Visibility),                        // Visible { focused } | Hidden
}

#[derive(Clone)]
pub struct Feeder { /* Sender<Input> + wake */ }
impl Feeder { pub fn send(&self, input: Input); }  // never blocks; wakes on empty -> non-empty only

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Changed { pub bits: u32, pub transcripts: Vec<ThreadId> }
impl Changed {
    pub const THREADS: u32 = 1;      pub const TRANSCRIPT: u32 = 1 << 1; pub const ASK: u32 = 1 << 2;
    pub const STATUS: u32 = 1 << 3;  pub const COMPOSE: u32 = 1 << 4;    pub const PALETTE: u32 = 1 << 5;
    pub const SETTINGS: u32 = 1 << 6; pub const BOARD: u32 = 1 << 7;     pub const LEAD: u32 = 1 << 8;
    pub const WORKERS: u32 = 1 << 9; pub const USAGE: u32 = 1 << 10;     pub const MODELS: u32 = 1 << 11;
    pub const SHARE: u32 = 1 << 12;  pub const HOSTS: u32 = 1 << 13;     pub const LAYOUT: u32 = 1 << 14;
    pub fn has(&self, bits: u32) -> bool;
    pub fn touches(&self, thread: &ThreadId) -> bool;
    pub fn merge(&mut self, other: Changed);
}

// ---- store.rs -----------------------------------------------------------------------------
pub struct StoreConfig {
    pub dirs: oxa_config::Dirs,
    pub env: oxa_config::Env,
    pub cwd: PathBuf,
    pub argv: Vec<String>,                          // oxa arguments given to oxa-desk
    pub wake: Arc<dyn Fn() + Send + Sync>,          // posts to the UI thread; the only GPUI coupling
}
pub struct Store { /* private: one sub-store per module */ }
impl Store {
    pub fn new(cfg: StoreConfig) -> Store;
    pub fn feeder(&self) -> Feeder;
    /// Apply every queued input. UI thread only. Cheap when the inbox is empty.
    pub fn drain(&mut self) -> Changed;
    /// The only entry for user intent. Nothing else mutates oxa state.
    pub fn dispatch(&mut self, action: Action) -> Changed;

    pub fn threads(&self) -> &ThreadTree;
    pub fn transcript(&self, id: &ThreadId) -> Option<&Transcript>;
    pub fn status(&self, id: &ThreadId) -> Option<&oxa_interactive::StatusModel>;
    pub fn session(&self, id: &ThreadId) -> Option<&oxa_interactive::SessionInfo>;
    pub fn compose(&self, id: &ThreadId) -> Option<&Compose>;
    pub fn ask(&self) -> Option<&Ask>;              // the one visible ask
    pub fn asks_waiting(&self) -> usize;
    pub fn palette(&self) -> &Palette;
    pub fn picker(&self) -> Option<&Picker>;
    pub fn settings(&self) -> &SettingsModel;
    pub fn board(&self, host: &HostId) -> &Panel<Board>;
    pub fn lead(&self, host: &HostId) -> &Panel<Lead>;
    pub fn workers(&self) -> &Panel<WorkersByHost>;
    pub fn usage(&self) -> &Panel<Usage>;
    pub fn models(&self) -> &Panel<Models>;
    pub fn share(&self) -> &Panel<Share>;
    pub fn hosts(&self) -> &Hosts;
    pub fn layout(&self) -> &Layout;
    pub fn theme(&self) -> &oxa_tui::Theme;
    pub fn perf(&self) -> &Perf;
}

/// A pane's data never goes blank: a failed read keeps the last good value and names the cause.
pub struct Panel<T> { pub value: Option<T>, pub observed_ms: u64, pub cause: Option<Arc<str>>, pub revision: u64 }

// ---- rows.rs (plain data the views render; views take rows, so a view test needs no store) --
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadKind { Session, Fork, Worker, Subagent, Lead, Ask }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState { Idle, Running, NeedsYou, Stalled, Done, Failed }
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadRow {
    pub id: ThreadId, pub depth: u16, pub kind: ThreadKind, pub state: ThreadState,
    pub title: Arc<str>, pub detail: Arc<str>,      // model, branch or first message
    pub host: Option<Arc<str>>, pub pr: Option<u64>, pub modified_ms: u64,
    pub has_children: bool, pub expanded: bool, pub selected: bool,
}
pub struct ThreadTree { /* flat visible rows in shared thread order */ }
impl ThreadTree {
    pub fn len(&self) -> usize;
    pub fn rows(&self, range: std::ops::Range<usize>) -> &[ThreadRow];
    pub fn index_of(&self, id: &ThreadId) -> Option<usize>;
    pub fn selected(&self) -> Option<&ThreadId>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Span { pub text: Arc<str>, pub fg: Option<[u8; 3]>, pub bg: Option<[u8; 3]>,
                  pub bold: bool, pub dim: bool, pub italic: bool, pub underline: bool, pub strike: bool }
#[derive(Clone, Debug, PartialEq)]
pub struct StyledLine { pub spans: Vec<Span> }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind { User, Assistant, Tool, Bash, Notice, Compaction, BranchSummary, Custom, Shown, Worker, Image }
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptRow {
    pub key: u64, pub kind: RowKind, pub lines: Arc<[StyledLine]>,
    pub expandable: bool, pub expanded: bool, pub live: bool,
    pub worker: Option<WorkerCard>, pub image: Option<oxa_interactive::ImageRef>,
}
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerCard { pub thread: ThreadId, pub name: Arc<str>, pub host: Arc<str>, pub model: Arc<str>,
                        pub state: ThreadState, pub pr: Option<u64>, pub tokens: Option<u64> }
pub struct Transcript { /* ViewModel + rendered rows for one width */ }
impl Transcript {
    pub fn revision(&self) -> u64;
    pub fn len(&self) -> usize;
    pub fn rows(&self, range: std::ops::Range<usize>) -> &[TranscriptRow];
    pub fn older_available(&self) -> bool;
    pub fn columns(&self) -> u16;
}

pub struct Ask { pub id: AskId, pub thread: ThreadId, pub thread_title: Arc<str>, pub host: HostId,
                 pub kind: AskKind, pub title: String, pub body: String, pub origin: Option<AskOrigin>,
                 pub deadline_ms: Option<u64> }
pub enum AskKind { Secret, Confirm, ApprovePublication, Select(Vec<String>), Input { placeholder: String },
                   Editor { prefill: String }, Access(oxa_proto::Access),
                   Dangerous { access: oxa_proto::Access, reasons: Vec<String>, keys: Vec<String> } }
pub struct AskOrigin { pub command: String, pub pid: u32, pub tool: String }   // from SecretAsk (Z08)
pub enum AskAnswer { Secret(SecretBuf), Bool(bool), Index(usize), Text(String),
                     Access(oxa_proto::Answer), Danger(oxa_proto::DangerAnswer), Dismiss }

// ---- secret.rs ----------------------------------------------------------------------------
/// One fixed, locked allocation. No Debug, Display, Clone or Serialize. Zero on clear and drop.
pub struct SecretBuf { /* private */ }
impl SecretBuf {
    pub const CAPACITY: usize = 1024;
    pub fn new() -> SecretBuf;                       // never fails; see `locked`
    pub fn locked(&self) -> bool;                    // false = the OS refused mlock; the sheet says so
    pub fn push(&mut self, c: char) -> bool;         // false when full
    pub fn pop(&mut self);
    pub fn clear(&mut self);
    pub fn chars(&self) -> usize;                    // for the dots; the only thing a view may read
    pub fn into_value(self) -> oxa_proto::SecretValue;
}

// ---- links/mod.rs -------------------------------------------------------------------------
pub enum LinkCmd {
    Session(oxa_interactive::SessionCmd),
    Steer { text: String, parts: Vec<oxa_proto::Part> },
    FollowUp { text: String, parts: Vec<oxa_proto::Part> },
    Abort, AbortRetry,
}
pub enum LinkEvent {
    Session(oxa_interactive::SessionInfo),
    Event(oxa_proto::AgentEvent),
    Entries { entries: Vec<oxa_proto::Entry>, reset: bool },   // loaded or appended lines
    Status(oxa_interactive::StatusModel),
    Reopened { entries: Vec<oxa_proto::Entry>, status: oxa_interactive::StatusModel },
    Tree(oxa_session::Tree),
    Done(Result<(), oxa_proto::OxaError>),
    Unsent { text: String, parts: Vec<oxa_proto::Part> },
    Closed { reason: Option<String> },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkCaps { pub interactive: bool, pub secrets: bool }
pub trait SessionLink: Send {
    fn send(&self, cmd: LinkCmd) -> Result<(), oxa_proto::OxaError>;
    fn caps(&self) -> LinkCaps;
    fn close(self: Box<Self>);
}
// three implementations: links::tail (read-only, A08), links::inproc (B02, C05), links::rpc (E03)

// ---- actions/mod.rs -----------------------------------------------------------------------
pub enum Action {
    // view state only
    SelectThread(ThreadId), ToggleExpand(ThreadId), OpenPane(PaneId), Split(SplitOp),
    LoadOlder(ThreadId), SetTranscriptWidth { thread: ThreadId, columns: u16 },
    PaletteQuery(String), PalettePick(usize), PickerPick(usize), CloseOverlay,
    DraftEdit { thread: ThreadId, text: String }, Attach { thread: ThreadId, path: PathBuf },
    // oxa state: each maps to exactly one existing entry point
    Submit { thread: ThreadId, how: Submit },                       // -> LinkCmd (Prompt | Steer | FollowUp | UserBash)
    Abort(ThreadId),                                                // -> LinkCmd::Abort / AbortRetry
    Slash { thread: ThreadId, line: String },                       // -> oxa_interactive::dispatch_slash
    App { thread: ThreadId, action: oxa_interactive::AppAction },   // -> actions::exec (effects.rs)
    Command { host: HostId, id: oxa_interactive::command_registry::CommandId, args: Vec<String> },  // -> CliPort
    AnswerAsk { id: AskId, answer: AskAnswer },                     // -> the UiRequest's reply channel
    SaveSetting { key: String, text: String, project: bool },       // -> parse_input, check, effects::save_setting
    Lifecycle { thread: Option<ThreadId>, op: Lifecycle },          // New | Resume | Fork | Clone | TakeOver | Close
}
pub enum Submit { Enter, AltEnter }
/// What each Action variant is allowed to reach. The contract test reads this table.
pub enum Target { ViewOnly, Link, Slash, AppAction, Registry, Broker, Effect }
pub const ACTION_TARGETS: &[(&str, Target)];
```

Shared-crate additions the contract uses (wave 0):

```rust
// oxa-cli/src/frontend.rs (Z05)
pub trait Frontend {
    fn ask_trust(&mut self, cwd: &str, gated: &[String]) -> Option<oxa_interactive::TrustChoice>;
    fn interactive(&mut self, session: oxa_core::Session, opts: oxa_interactive::InteractiveOpts,
                   pinned: Option<&str>) -> Result<i32, oxa_proto::OxaError>;
}
pub fn run_with(argv: &[String], env: &oxa_config::Env, io: Stdio, frontend: &mut dyn Frontend) -> i32;

// oxa-proto/src/secret.rs and ui.rs (Z08)
pub struct SecretAsk<'a> { pub title: &'a str, pub prompt: &'a str, pub command: &'a str, pub pid: u32, pub tool: &'a str }
pub struct SecretValue { /* zero on drop; no Debug, Display, Clone, Serialize */ }
impl SecretValue { pub fn from_bytes(bytes: &[u8]) -> SecretValue; pub fn expose(&self) -> &[u8]; }
// trait Ui: fn secret_ask(&mut self, ask: &SecretAsk) -> Option<SecretValue> { /* default: self.secret(..) */ }

// oxa-session/src/index.rs (Z09)
pub enum Relation { Root, Fork, Clone, Worker, Subagent }
pub struct ThreadMeta { pub sref: SessionRef, pub cwd: PathBuf, pub parent: Option<String>, pub relation: Relation,
                        pub name: Option<String>, pub first_message: String, pub modified_ms: u64, pub bytes: u64 }
impl FsStore { pub fn thread_index(&self, cwd: Option<&Path>) -> Vec<ThreadMeta>; }
pub fn thread_order(metas: &[ThreadMeta]) -> Vec<(usize, u16)>;       // (index, depth), newest thread first

// oxa-rpc/src/events_decode.rs (Z10)
pub fn event_from_json(v: &serde_json::Value) -> Option<oxa_proto::AgentEvent>;

// oxa-interactive/src/effects.rs (Z07, as merged). `Notice { level, text }`; `Notice::of(result)`
// turns an error into an error notice. The caller owns the `Settings` it reads from.
pub fn save_setting(settings: &mut Settings, dirs: &Dirs, cwd: &Path, key: &str, value: Value, project: bool) -> Result<Notice, OxaError>;
pub fn save_permission_mode(settings: &mut Settings, dirs: &Dirs, cwd: &Path, mode: PermissionMode) -> Result<Notice, OxaError>;
pub fn persist_theme(settings: &mut Settings, dirs: &Dirs, cwd: &Path, name: &str) -> Result<Notice, OxaError>;
pub fn append_always(settings: &mut Settings, dirs: &Dirs, cwd: &Path, keys: &[String]) -> Result<(), OxaError>;
pub fn rename_session(store: &dyn SessionStore, session: &SessionRef, name: &str) -> Result<Notice, OxaError>; // not the open session: that is SessionCmd::SetName
pub fn delete_session(store: &dyn SessionStore, tmp_dir: &Path, open_id: &str, session: &SessionRef) -> Notice;
pub fn export(source: Option<&Path>, session_id: &str, cwd: &Path, path: Option<PathBuf>) -> Export; // Done(Notice) | Html(path) -> SessionCmd::Export
pub fn bug_report(dirs: &Dirs, description: &str, facts: &[String]) -> Result<Notice, OxaError>;
pub fn save_trust(save: &mut TrustSaver, choice: TrustChoice) -> Result<Option<Notice>, OxaError>;
```

### 1.5 The `oxa-desk` binary

```rust
fn main() {
    oxa_proto::panic::install_hook();
    if oxa_cli::askpass::invoked_as_askpass(std::env::args_os().next().as_deref()) { exit(askpass_main()) }
    if let Some(r) = oxc_local::platform::child_entry(&args_os) { ... }      // as oxa's main does
    let args: Vec<String> = std::env::args().skip(1).collect();
    if oxa_desk::wants_window(&args) { exit(oxa_desk::run(args)) }           // no subcommand, or --desk-*
    exit(oxa_cli::run(&args, &Env::from_process(), Stdio::real()))            // everything else: it is oxa
}
```

`wants_window` is true when the first argument is not a registry CLI path and not a headless flag (`-p`, `--mode`, `--version`, `--help`, `--export`, ...). Children that oxa starts from `current_exe()` therefore behave exactly as under `oxa`.

### 1.6 Views

`oxa-desk` holds one `Entity` around the `Store`. A view subscribes with the `Changed` bits it reads and renders rows. A view never keeps a copy of oxa state, never starts a timer and never spawns a thread. All widgets come from `kit/`.

Left: `views/thread_tree`. Centre: `views/transcript` + `views/block` + `views/compose`, sheets on top (`secret_sheet`, `ask_sheet`), or a full pane. Right: `views/glance`. Overlays: `palette`, `pickers`.

### 1.7 GPUI: what must be verified in Z02 (not known from here)

- The exact crate and version to pin (crates.io `gpui` or a pinned git revision), and its licence text for THIRD_PARTY_NOTICES.
- The Linux renderer in that version and the system packages it needs on Ubuntu 24.04 arm64 (Wayland, xkbcommon, fontconfig, Vulkan loader, a software Vulkan driver for CI).
- That the virtual list elements, the headless test context and the platform input handler exist under the names the cheat sheet will use. The cheat sheet (Z17) is written from code that compiled in CI, not from memory.
- Whether a window can be driven under a headless compositor on a hosted runner. If not, the smoke falls back to the headless test platform and pixel screenshots are dropped (section 2.4).
- Minimum Rust version against the CI toolchain (1.99.1).

## 2. One-product contracts and their CI tests

The desktop is a second front end. These four contracts keep it one, and each is a test in the desk job. Tests protect behaviour; none pins a count or a wording.

### 2.1 Registry coverage (`tests/contract_registry.rs`, `tools/ci/desk_contract.py`)

1. For every `Declaration` in `COMMANDS`: `surfaces.desk` is `Supported` or `Exception(reason)` with a non-empty reason. (Z04 makes this a compile-time fact; the test reads it.)
2. For every declaration with `desk: Supported`: the palette finds it by its path and resolves it to `Action::Command { id, .. }` with that id, and `CliPort` accepts it. Until its slice lands a row may be `Pending("<slice-id>")` in one table in the test; F10 fails if a `Pending` is left.
3. For every name in `INTERACTIVE_COMMANDS`: `Action::Slash` reaches `oxa_interactive::dispatch_slash` and returns the same `AppAction` kind as the TUI test for that command.
4. Nothing outside: `ACTION_TARGETS` lists every `Action` variant with its target, and the lint fails on (a) a dependency or `use` that the rule of 1.1 forbids, (b) `std::process::Command` in desk-core outside `hosts/ssh.rs`, (c) a write to a settings or session file in desk-core outside calls to `effects::` and `oxa_cli::run`, (d) a string table of command names, model names, prices or limits in desk code.

### 2.2 Settings come from the descriptors (`tests/contract_settings.rs`)

1. Every descriptor that is not `hidden` yields exactly one settings row, and the row's label, help, group, range and `Apply` are the descriptor's own values.
2. Every `Control` variant has an editor: the two `match` statements (model in D01, view in D03) have no wildcard arm, so a new control kind fails the build.
3. A typed value is saved only through `parse_input` + `check` + `effects::save_setting`; the test saves one key through the desktop path and through the TUI effect and compares the settings file.
4. A `user_only` key cannot be saved to the project file; a `secret` row never shows its value.

### 2.3 Shared fixture events (`oxa_fake::fixtures`, Z11)

One set of named `AgentEvent` sequences. `oxa-interactive/tests/fixtures_shared.rs` renders them with `frames`. `tests/contract_fixtures.rs` in desk-core feeds the same sequences through `transcript::model` and `transcript::render` and asserts that, block by block, the desktop's span text equals the TUI's `render_block` text with the SGR codes removed. Because both use `apply_event` and `render_block`, this test fails only when someone forks the logic, which is exactly its job.

### 2.4 View snapshots

- **Text snapshots (blocking).** `kit::test` renders a view headless to a text tree (element kind, text, a few layout facts). Each view slice adds snapshots for its rows built by hand from `rows.rs`. Files live next to the test (`tests/snapshots/<view>__<case>.txt`), owned by the slice.
- **Pixel screenshots (advisory).** The desk job starts the real binary with fixture data under a headless compositor and software Vulkan, takes screenshots of the main views in light and dark, and puts them in the job summary. Not a required result: software rendering differs between runner images.
- **Live check (blocking, from Z02).** The real binary opens a window and draws a frame under the headless compositor. This is the desktop's equivalent of the PTY live checks that `AGENTS.md` asks for.

## 3. Wave 0 (serial, before any parallel work)

Authored by the architect (or one strong agent under the architect), not by the implementer pool: these PRs change shared crates, CI and manifests. Merge order is the listed order. Z04 to Z12 own disjoint files and can be written at the same time; they still merge one after another. Z07 (`driver.rs`) and Z08 (`owner_confirm.rs`) each trigger the 90-minute macOS job once.

The task asked for the crates first and the CI job second. They are one PR here (Z01): a nested workspace is invisible to today's CI, so the crates cannot be shown green without the job, and the job has nothing to build without the crates.

### 3.1 The desk CI job (Z01, extended by Z02 and Z03)

- **File:** `.github/workflows/desk.yml`, a reusable workflow (`workflow_call`) with `workflow_dispatch` inputs `mode` (`pr`, `queue-check`, `rc`) and `lock` (regenerate `Cargo.lock` and upload it). It is called from the new `desk` job in `checks.yml`, and `gate` aggregates it. It is not a separate required check: a path-filtered required workflow would hang PRs that do not start it.
- **Trigger paths** (classifier output `desk` in `tools/ci/paths.py`): `oxa/desk/**`, `.github/workflows/desk.yml`, `tools/ci/desk_contract.py`. These paths select `desk` only, not `oxa`. `changelog.d/desk-*.md` and `oxa/docs/pages/desk/**` select nothing beyond the scans and the links check. In the merge queue, any `oxa/` change outside `oxa/desk/` also selects `desk` in `queue-check` mode (Z03).
- **Jobs**, both on `ubuntu-24.04-arm`, hosted, actions pinned by SHA, `permissions: contents: read`, no secrets:
  - `desk check`: `cargo fmt --manifest-path oxa/desk/Cargo.toml --all -- --check`; `python3 -I tools/ci/desk_contract.py`; `cargo clippy --locked --manifest-path oxa/desk/Cargo.toml --workspace --all-targets -- -D warnings`.
  - `desk test`: `cargo nextest run --locked --manifest-path oxa/desk/Cargo.toml --workspace` with the pinned nextest (`tools/ci/install_nextest.sh`); then the headless smoke (from Z02).
- **Speed:** `CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`. Cache restore with `actions/cache/restore` under the key of `tools/ci/rust_cache_key.py` with prefix `desk-` over `oxa/desk/Cargo.lock`, the toolchain and the architecture; only main saves, as the other jobs do. The lock changes only in wave-0 PRs, so every slice PR gets a warm cache. Target: a result in **6 minutes or less warm, 15 or less cold**. Each job writes its duration and queue delay to the job summary. If the warm time is above 8 minutes after Z02, fix that before wave 1 starts (smaller test binaries, one test target per crate).
- **macOS:** `cargo check` on a hosted macOS arm64 runner, only when a desk manifest changes, nightly and on dispatch. Not on slice PRs. `dev-build.yml` covers main after F03.

### 3.2 Wave-0 PRs

The wave-0 slices are Z00 to Z17 and the two late ones, ZL1 and ZL2. Files, dependencies, done-when and tests of each are in [`slices.json`](slices.json) (`"wave": 0`). The merge order is the order in that file.

## 4. Waves 1 to 6: the slice DAG

Rules for every slice:

- One PR into main, about 800 changed lines or fewer, from a fresh `origin/main`. No stack, no long-lived branch.
- A slice owns the files listed for it and nothing else. Within a wave no file has two owners (checked by the generator of this plan and again by the pre-check). The stub of every listed file exists since wave 0, so no slice edits `lib.rs`, a `mod.rs`, `store.rs`, a manifest or a workflow.
- Besides its listed files every slice adds one changelog fragment, `changelog.d/desk-<slice-id>.md`, and may add snapshot files under `tests/snapshots/` with its own view name as prefix.
- "Tests" names the behaviour to protect. The agent chooses the test names.
- A wave starts when every slice it depends on is merged. Inside a wave all slices run at once.
- Waves 3 and 4 have no slice-level dependency on each other apart from the ones listed (D01 needs C04; the pane models need C09). When C04 and C09 are merged, the rest of wave 4 can start while wave 3 is still running.

What waves 1 and 2 deliver (the prototype gate):

| Gate item | Slices |
|---|---|
| Window on Wayland arm64 and macOS arm64 | Z02, Z16, B06 |
| Virtualised thread tree with nested workers and subagents, 5,000 sessions | Z09, A01, A02, A03, A12, A19, B08 |
| Live read-only transcript | A04, A05, A06, A07, A08, A13, A14, B01, B03 |
| Secure sudo sheet through the broker | Z08, A09, A10, A15, A16, B02 |
| Twenty fake workers through `oxa-fake` | B04 |
| Measurement harness: idle CPU, wakeups, RSS, cold start, binary size | A11, A18, B05, B07 |

### 4.1 The slices of waves 1 to 6

Every slice is one entry in [`slices.json`](slices.json): id, wave, title, owned files, dependencies, done-when, the behaviour its tests protect and an estimate of changed lines. That file is the only copy; the tracking issues are created from it.

- Wave 1 (A..): models and first views, all parallel.
- Wave 2 (B..): prototype integration and the gate. B05 depends on B04 and B07 of the same wave; it starts when they are merged. B09 is the gate itself and is not a pool slice.
- Wave 3 (C..): compose, asks, palette, session lifecycle.
- Wave 4 (D..): settings and local panes.
- Wave 5 (E..): hosts, lead, board, split views. Starts only when ZL1 and ZL2 are merged, which need lead-web S4 and kanban K1, K2 and K9 on main.
- Wave 6 (F..): packaging, docs, hardening, ship. F01 to F09 run at once. F10 is the final slice and merges last.

## 5. The gate after wave 2

**Who and where.** The owner runs it on the laptop (Snapdragon X, Hyprland) with the B06 release-candidate artifacts. Nothing is compiled on the laptop. The same probe runs on a macOS arm64 machine for the record. B09 writes the numbers into `oxa/docs/design/desk/gate-report.md` and `oxa/docs/pages/desk/budget.md`.

**How.** `oxa-desk-probe gate --out report.json` starts `oxa-desk` itself, drives the scenarios, samples the process from procfs and compares with `oxa/desk/gate.toml`. Rows marked "stop" decide the gate; the others are recorded.

| # | Scenario | Measurement | Threshold | Kind |
|---|---|---|---|---|
| G1 | Cold start: 10 launches of `oxa-desk --desk-exit-after-first-frame` with a 30-thread store, page cache warm | Wall time from process start to the first presented frame; the 9th of 10 sorted values | **<= 400 ms** | stop |
| G1b | First launch after a reboot | Same | recorded | record |
| G2 | Idle, window visible and focused, 30 threads loaded, nothing streaming, 60 s | CPU: (utime + stime) / 60 s, percent of one core | **<= 0.5 %** | stop |
| G3 | Same run | Wakeups: voluntary context switches per second, all threads | <= 2 per second | record |
| G4 | Idle, window hidden (other workspace), 60 s | Wakeups in 60 s, all threads; frames drawn (`--desk-stats`) | **<= 12 wakeups and 0 frames** ("no steady wakeups") | stop |
| G5 | 30 threads loaded, each transcript opened once | `VmRSS`; PSS from `smaps_rollup` also recorded | **RSS <= 150 MB** | stop |
| G6 | 5,000 synthetic sessions, scroll the tree end to end | Rows built per frame; frame time, 95th percentile | rows <= viewport + overscan; frame time recorded | record |
| G7 | 20 fake workers streaming for 60 s, one of them open | CPU percent; frames per second; time back to idle after the streams end | back to G2 level within 2 s; CPU recorded | record |
| G8 | Binary size of `oxa-desk` (release, stripped) | bytes | recorded (target about 35 MB) | record |
| G9 | Real `sudo -A true` from a session: the sheet names host, thread, command, pid | By hand: type a canary password; then search the session directory, `~/.oxa`, the logs and the journal for the canary | **0 hits**, and `sudo` succeeded | stop |
| G10 | Hyprland check list: fractional scale 1.25 and 1.5, tiling and resize, workspace switch, suspend and resume, keyboard layout switch, no Vulkan errors in the log | By hand | **no bug without a fix we can carry** | stop |
| G11 | macOS arm64: window opens, G1, G2, G5 | Same probe (`ps`-based) | recorded | record |

**Pass.** Continue with wave 3 on GPUI. The thresholds stay in `gate.toml`; F05 turns the ones a hosted runner can measure into the CI perf budget.

**Fail.** Any "stop" row that fails after one round of fixes (one day at most) switches the plan to Slint on the same store crate. What changes:

| Part | On a switch |
|---|---|
| `oxa-desk-core` (all of it), `oxa-desk-probe`, every wave-0 shared-crate PR | **Unchanged.** This is why the store has no GPUI type. |
| Z02, Z15, Z16 | Redone as Z02s (Slint pin, lock, licence note, window, smoke), Z15s (kit as `.slint` components and Rust glue), Z16s (workbench, pump from the same `wake`). The pump contract is the same: wake, drain, notify by `Changed`. |
| Wave-1 view slices A12, A13, A14, A15, A16, A17 and the view half of B03 and B07 | Redone with the same ids and the same done-when under `oxa/desk/oxa-desk-slint/`. Six and a half slices, about two days of pool time. |
| A15 secure field | A key-capturing focus scope that feeds `SecretBuf` and draws dots. Slint's own password input is not used, because it keeps the text in a string we cannot zero. |
| Core slices A01 to A11, A18, A19, B01, B02, B04, B05, B08 | Unchanged. |
| View slices of waves 3 to 6 (C02, C06, C08, C11, C13, C15 view, C16 view, D02, D03, D04 view, D06, D08, D10, D12, D13 view, D14 kit, D15 view, E06, E08, E09, E11, E12 view, F06, F08 view, F09) | Same ids, same inputs and done-when, file paths under `oxa-desk-slint/`, written against the Slint kit and cheat sheet. Not yet started at the gate, so nothing is thrown away. |
| The gate | Run again after the redone slices, with the same `gate.toml`. |
| Decision needed | The Slint licence (owner decision 5) before Z02s. |

## 6. Review protocol (per PR)

**Who reviews.** oxa does. The review script runs an oxa review on the exact head and posts the `oxa-review` comment below. For a slice in the security core, or whenever the script itself wants one, the script also starts an independent security review: a second, clean-context run with oxa and one with a ChatGPT-plan model. It starts them directly; the lead is not involved. An open security review blocks like a P0/P1 finding. The architect reads the verdicts and steps in only on a disagreement or a `needs-dep`.

**Pre-checks (mechanical, done by the orchestrator before a review is asked; a failure goes back to the agent, it is not a review finding):**

1. The desk result in `checks` is green at the PR's head SHA.
2. `gh pr diff <n> --name-only` is inside the slice's files plus its changelog fragment and its own snapshots. No manifest, lock, workflow, `lib.rs`, `mod.rs` or `store.rs`. No path with `macos` or `darwin` in it.
3. Size: warn above 900 changed lines; above 1,200 the slice is cut in two by the architect.
4. The body has `Closes #<issue>`, the plan link, the tests, the CI result at the exact head, the "faster or leaner / replaces" line, and no session link, person name, host name or real path.
5. Labels `desk` and `desk-w<N>` are set. A slice in the security core (A09, A15, A16, B02, D04, E04, every workflow change) also has `two-review`.

**What the review checks, in this order:**

1. **Build truth.** The green run is for this head. The slice's done-when is met by code, not by a stub or a test that cannot fail.
2. **No logic outside the core.** No second parser of session files, registry files or settings files; no command, model, price or limit table; no re-worded status text; no markdown or diff rendering of its own; no dialog flow that bypasses `UiRequest`. In `oxa-desk`: views only read rows and send `Action`s.
3. **Secret path.** A secret lives only in `SecretBuf` and `SecretValue`. It is never in a `String`, a log, a notice, Debug output, the perf report, the clipboard, the accessibility tree, IME text, a snapshot or a fixture. Dismiss never consents. Remote secret asks stay refused.
4. **Idle cost.** No timer, interval, sleep loop, `recv_timeout` loop or `stat` polling. No `notify` that is not tied to a `Changed` bit. No thread that does not block. No work for rows that are not visible. No animation while hidden.
5. **Contracts.** The four tests of section 2 still mean something (no `Pending` row added without a slice id, no wildcard arm added, no lint exclusion).
6. **Docs.** The topic page says what the slice now does, in short plain sentences.

**The comment.** One PR comment per reviewed head. The merge gate reads only the first line:

```
oxa-review head=<full 40-character sha> verdict=pass p0=0 p1=0 p2=N p3=N
```

A blocking review uses the same line with `verdict=block` and the real counts:

```
oxa-review head=<full 40-character sha> verdict=block p0=N p1=N p2=N p3=N
```

Below the first line: one finding per paragraph with severity, file and line, the failure scenario, and the smallest fix. A new push makes the comment stale; the gate compares `head=` with the PR head and waits for a new line.

**When to block.** Only P0 and P1, and only these:

| Block | Severity |
|---|---|
| The build or a required check is broken, or the green result is for another head | P0 |
| A secret reaches a model, a log, a session file, Debug output or leaves `SecretBuf`/`SecretValue` (the block list of `docs/threat-model.md`) | P0 |
| Data loss: a session, registry or settings file is written by desk code outside the shared effects | P0 |
| Logic duplicated outside the core (point 2 above) | P1 |
| An idle-CPU regression (point 4 above) | P1 |
| A dialog path where dismiss, timeout or a closed sheet counts as consent | P1 |

Everything else is P2 or P3: it is listed in the comment, the verdict is `pass`, and the orchestrator files one follow-up issue per P2 with the label `desk-followup`. Do not ask for a split of a PR up to about 800 lines, a rename, a refactor or an extra test without naming the behaviour it protects. A finding that would make oxa less powerful needs a `Restricts:` line and the owner, as `AGENTS.md` says.

## 7. Agent briefs

### 7.1 Brief template for an implementer agent

Fill the `<...>` fields from `slices.json`. The brief is the agent's only context.

```text
You implement one slice of the oxa desktop app in the ox-llm repository. Read AGENTS.md first; its rules apply.

SLICE <id>: <title>   (wave <wave>, about <lines> lines)
Issue: #<issue>   Plan: oxa/docs/design/desk/slices.json (entry "<id>") and build-plan.md
Files you own (touch nothing else):
  <files>
  changelog.d/desk-<id-lowercase>.md   (new; one or two bullets)
Inputs you call (read them before you write): <inputs>
Done when: <done_when>
Behaviour your tests protect: <tests>

Setup
  git fetch origin main
  git worktree add -b desk/<id> ../desk-<id> origin/main
  cd ../desk-<id>

Rules
  1. You cannot compile here. Do not run cargo, rustc or any build. GitHub CI is the compiler.
  2. Read before writing: oxa/desk/oxa-desk-core/src/{ids,rows,input,store}.rs, links/mod.rs, actions/mod.rs,
     the stub of each file you own, and for a view slice oxa/docs/design/desk/gpui-cheatsheet.md and
     oxa/desk/oxa-desk/src/kit/. Use only the kit and the GPUI items in the cheat sheet.
  3. Keep the public signatures of the stubs. Fill bodies. Add private items freely inside your files.
  4. No logic of your own: call the function named under Inputs. No command, model, price or limit lists.
     No parsing of oxa's files by hand. No timers, sleeps or polling. No thread that does not block.
  5. Never edit Cargo.toml, Cargo.lock, .github/, lib.rs, a mod.rs or store.rs. If you need a new dependency,
     a new public item in a shared crate, or another slice's file: stop and report "needs-dep" or "needs-hook".
  6. No file or directory name with "macos" or "darwin" in it.
  7. Synthetic data only: no real paths, host names, e-mail addresses, keys, person names or session links
     in code, tests, docs, commits or the PR.
  8. Short plain sentences in comments and docs. No test that pins a count or a wording.

Publish
  git add <your files>; git commit   (GitHub no-reply e-mail address; no session link or trailer)
  git push -u origin desk/<id>
  gh pr create --base main --head desk/<id> --title "desk <id>: <title>" \
     --label desk --label desk-w<wave> --body-file <body>
  The PR is ready, not a draft (a draft runs only formatting and the scans).
  Body: what and why; "Closes #<issue>"; the plan link; the tests; "CI: <result> at <full sha>";
        "Faster or leaner: <one line>"; "Replaces: <what, or nothing>"; "Security core: yes|no".

Wait for CI (one blocking wait, never a polling loop, never a "still waiting" comment)
  run=$(gh run list --branch desk/<id> --workflow checks --limit 1 --json databaseId -q '.[0].databaseId')
  gh run watch "$run" --exit-status --interval 60
  On failure: gh run view "$run" --log-failed | tail -100 ; fix inside your files; commit; push; wait again.
  To take in main: git merge origin/main (a normal merge). Never rebase, never force-push,
  never push after the PR entered the merge queue.
  At most 6 CI rounds. Then stop.

Report (your last message)
  slice id, PR number, full head sha, CI result at that sha, files changed, changed lines,
  rounds used, and anything you could not do ("needs-dep", "needs-hook", an unclear done-when).
```

### 7.2 Retrying a failed slice

The orchestrator never sends a message to a running agent (it would fork a copy into the same worktree). A decision goes into a new brief.

| What happened | What the orchestrator does |
|---|---|
| Red after 6 CI rounds | A fresh agent with the same brief plus: "Continue on the existing branch `desk/<id>` (worktree from `origin/desk/<id>`), PR #n. The failing log tail is below." At most 2 fresh agents; then the architect fixes the PR or cuts the slice in two. |
| Pre-check fails (files outside the slice, missing body line, bad path name) | A fresh agent on the same branch with the exact pre-check output. Counts as a retry. |
| `verdict=block` | A fresh agent on the same branch with the P0/P1 findings as its packet: "show each finding is real, fix it with a test, change nothing else". The review script reviews the new head. |
| `needs-dep` or `needs-hook` | The architect lands a small serial wave-0 style PR (manifest, shared crate or contract), then the slice starts again from a fresh `origin/main`. The old PR is closed after an archive tag. |
| Conflict with main | A fresh agent merges `origin/main` into the branch. With file ownership this should only happen when a wave-0 file moved. |
| The merge queue rejects the PR | Re-queue once. A second rejection is treated as "red". |
| The same slice fails twice for a reason in the plan (wrong input, done-when that cannot be met) | The architect corrects `slices.json` in a docs PR, and the slice restarts. |
| Three slices of one wave fail on the same kit or contract gap | Stop the wave, fix the kit or the cheat sheet in one architect PR, restart those slices. |

## 8. Counts, agents and wall time

| Wave | Slices | Implementer pool | Architect | Estimated changed lines |
|---|---|---|---|---|
| 0 | 20 | 0 | 20 | 10,050 |
| 1 | 19 | 19 | 0 | 7,500 |
| 2 | 9 | 7 | 2 | 3,750 |
| 3 | 16 | 16 | 0 | 7,950 |
| 4 | 15 | 15 | 0 | 6,150 |
| 5 | 13 | 13 | 0 | 5,850 |
| 6 | 10 | 6 | 4 | 3,250 |
| **Total** | **102** | **76** | **26** | **44,500** |

Wave 0 holds 20 serial PRs (two of them late, ZL1 and ZL2). Waves 1 to 6 hold 82 slices: 76 for the implementer pool, 6 for the architect (B06, B09, F02, F03, F05, F10).

Expected agent runs: 76 first attempts plus about 23 fresh-agent retries, about 99 implementer runs in total, at most 16 at once. The architect writes 26 PRs; the review script reviews about 109 heads.

Assumptions behind the wall time (none measured yet; Z01 and Z03 record the real CI and queue times and this table is redone from them):

- One pool slice: 15 to 25 minutes to write, 3 CI rounds of about 7 minutes on average (the agent cannot compile, so the first push rarely passes), 5 to 10 minutes of review, about 10 minutes in the merge queue with the desk fast path. About 70 minutes when it goes well; plan with 80.
- About 30 % of slices need one fresh-agent retry.
- The merge queue is the limit if Z03 is not accepted: every desk PR then waits for the complete oxa workspace run.
- GitHub-hosted concurrency: 16 agents pushing at once start about 80 jobs (classifier, scans, rustfmt, links, two desk jobs each). Above the account's job limit they queue; 12 agents is the comfortable number, 16 only for waves 1 and 3.

| Wave | Pool slices | 10 agents | 12 agents | 16 agents | Notes |
|---|---|---|---|---|---|
| 1 | 19 | 3.3 h | 2.7 h | 2.1 h | all at once |
| 2 | 7 | 3.1 h | 3.1 h | 3.1 h | B05 after B04 and B07; then B06 artifacts and the gate (owner, about half a day) |
| 3 | 16 | 2.8 h | 2.3 h | 1.7 h |  |
| 4 | 15 | 2.6 h | 2.2 h | 1.7 h | can overlap wave 3 after C04 and C09 |
| 5 | 13 | 2.3 h | 1.9 h | 1.7 h | waits for ZL1, ZL2 |
| 6 | 6 | 3.2 h | 3.2 h | 3.2 h | F02, F03 and F10 run long CI (release build, macOS about 90 min) |
| **Waves 1 to 6** | **76** | **17 h** | **15 h** | **14 h** | pool wall time, without wave 0 and the gate |
| Wave 0 | 20 architect PRs | 2 to 3 working days | | | serial merges through the full check set; two 90-minute macOS runs |

Calendar: wave 0 is the long pole and is architect work; the gate needs the owner at the laptop for about half a day. The parallel waves themselves fit into two long working days of pool time. A realistic span from Z00 to F10 is 6 to 8 working days if the gate passes, plus about 2 days if it switches to Slint, plus the wait for lead-web S4 and kanban K1/K2/K9 before wave 5.

## 9. Risks and what was not verified

- **GPUI written blind.** The pool cannot compile, and GPUI is pre-1.0 with thin documentation. The kit, the cheat sheet and the row-only views are the mitigation. If more than a third of the wave-1 view slices need the architect, treat that as a gate signal of its own.
- **Two sessions in one process.** `run_with` may meet process-global state (the YOLO switch, SIGTERM ownership, the share listener, environment edits). Z05 proves it or falls back to one in-process session plus `--mode rpc` children; the store API is the same either way.
- **In-process `oxa_cli::run` for JSON commands** on a background thread is assumed safe because the function takes its streams and working directory as arguments. C09 proves it with two calls at once; the fallback is to run the `oxa` shim as a child process.
- **Where subagent and worker sessions record their parent** was not traced to the end. Z09 states it and adds the relation if it is missing.
- **Headless compositor with software Vulkan on a hosted arm64 runner** is assumed to work. If it does not, the live check uses the headless test platform and the CI perf budget (F05) measures only binary size and start-up to first layout.
- **CI duration** of the desk job (6 minutes warm) and of the queue is a target.
- **Lead and board panes** depend on slices of two other plans that are not on main.
- **`gpui-component`** was not evaluated here. Without it C02 grows a text editor and should be cut in two.
- **Text rendering.** Wave 1 shows transcripts in a monospace font at a column width, exactly as the TUI lays them out. Proportional text needs a markdown tree from `oxa-tui` instead of SGR lines; that is a follow-up, not in this plan.
- **Accessibility** of GPUI on Linux was not checked; F06 covers keyboard use and IME only.

## 10. Machine-readable slice DAG

[`slices.json`](slices.json), one slice per line. `who` is not a key of the DAG: architect-owned slices are every wave-0 id (`Z..`) and the ones whose title ends in `[architect]` or `[owner + architect]`. `lines` is an estimate of changed lines.
