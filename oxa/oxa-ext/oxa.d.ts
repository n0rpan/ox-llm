// oxa extension API type declarations (E1).
// Written from pi's ExtensionAPI / ExtensionContext types (packages/coding-agent/src/core/extensions/types.ts,
// MIT), with `pi` renamed to `oxa`. Omitted in oxa, so not declared here: registerProvider,
// unregisterProvider, registerVirtualModel, unregisterVirtualModel, registerMcpServer,
// unregisterMcpServer, getMcpServers (MCP servers come from mcp.json), ctx.newSession and
// ctx.switchSession (the user's /new and /resume), the before_provider_headers event, and Node /
// npm imports. Calling an omitted API throws an Error with code "unsupported_in_oxa".
//
// This file is a global declaration file (no top-level import or export), so an editor picks it up
// with `/// <reference path="oxa.d.ts" />`.
//
// An extension is a JavaScript module (ES modules, .js or .mjs) whose default export is a
// factory: `export default function (oxa: Oxa) { ... }`. The factory may be async.

/** A JSON value. Everything crossing into an extension is JSON (functions are handles). */
type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };

/** A JSON Schema object describing tool parameters (`type: "object"`). */
type JsonSchema = { type: "object"; properties?: { [key: string]: JsonValue }; required?: string[]; [key: string]: JsonValue | undefined };

type ToolClass = "read" | "write" | "exec" | "net";
type ToolExposure = "direct" | "model-only" | "codemode" | "deferred" | "hidden";
type ThinkingLevel = "off" | "minimal" | "low" | "medium" | "high" | "xhigh";
type DeliveryMode = "steer" | "followUp" | "nextTurn";
type MessageContent = string | Array<{ type: "text"; text: string } | { type: "image"; data: string; mimeType: string }>;
type WidgetPlacement = "aboveEditor" | "belowEditor";

/** Key notation, e.g. "ctrl+shift+k". */
type KeyId = string;

interface Disposable {
  (): void;
}

// ---------------------------------------------------------------------------------------------
// Contexts
// ---------------------------------------------------------------------------------------------

interface ExtensionUIDialogOptions {
  /** Milliseconds before the dialog is dismissed (pi's name). */
  timeout?: number;
}

/**
 * Dialogs answer synchronously in oxa: the handler waits while the dialog is open (its deadline
 * clock is paused), and the call returns the answer itself. pi's dialogs return a promise;
 * `await ctx.ui.select(..)` works in oxa as well, since awaiting a value yields the value.
 */
interface ExtensionUIContext {
  /** Pick one of `options`: the chosen option string, `undefined` when dismissed or timed out. */
  select(title: string, options: string[], opts?: ExtensionUIDialogOptions): string | undefined;
  /** `true` when confirmed; `false` when declined, dismissed or timed out. */
  confirm(title: string, body: string, opts?: ExtensionUIDialogOptions): boolean;
  /** The typed text, `undefined` when dismissed. */
  input(title: string, placeholder?: string, opts?: ExtensionUIDialogOptions): string | undefined;
  /** The edited text, `undefined` when dismissed. */
  editor(title: string, prefill?: string): string | undefined;
  notify(text: string, level?: "info" | "warning" | "error"): void;
  setStatus(key: string, text: string | undefined): void;
  setWidget(key: string, lines: string[] | undefined, options?: { placement?: WidgetPlacement }): void;
  setFooter(lines: string[] | undefined): void;
  setHeader(lines: string[] | undefined): void;
  setTitle(title: string): void;
  setWorkingMessage(text: string | undefined): void;
  setEditorText(text: string): void;
  getEditorText(): string;
  addAutocompleteProvider(provider: (prefix: string) => string[] | Promise<string[]>): void;
  /**
   * A custom component, pi's factory signature: `factory(tui, theme, keybindings, done)`
   * returns the component (or a promise of it): `render(width)` returns lines,
   * `handleInput(key, text)` returns `true` to redraw. Calling `done(value)` closes the
   * component; `custom` then resolves with `value` (`null` when dismissed or headless).
   */
  custom<T = unknown>(
    factory: (tui: OxaTui, theme: OxaTheme, keybindings: OxaKeybindings, done: (value?: T) => void) =>
      | OxaComponent
      | Promise<OxaComponent>,
    options?: { overlay?: boolean },
  ): Promise<T | null>;
  /** pi's `ctx.ui.theme`: the same plain theme the component factories get. */
  readonly theme: OxaTheme;
  /** A custom editor: `factory(tui, theme, keybindings)` returns the component. */
  setEditorComponent(factory: (tui: OxaTui, theme: OxaTheme, keybindings: OxaKeybindings) => OxaComponent): void;
}

/** A component: `render(width)` returns lines; `handleInput(key, text)` returns `true` to redraw. */
interface OxaComponent {
  render(width: number): string[];
  handleInput?(key: KeyId, text?: string | null): unknown;
  invalidate?(): void;
}

/** pi's `tui` argument: `requestRender()` during `handleInput` redraws the component. */
interface OxaTui {
  requestRender(): void;
  readonly terminal: { columns: number; rows: number };
}

/** pi's `theme` argument. oxa uses the terminal's colours: every function returns the text. */
interface OxaTheme {
  fg(color: string, text: string): string;
  bg(color: string, text: string): string;
  bold(text: string): string;
  italic(text: string): string;
  underline(text: string): string;
  dim(text: string): string;
  strikethrough(text: string): string;
  inverse(text: string): string;
}

/** pi's `keybindings` argument: the default keys of the `tui.select.*` / `tui.input.*` ids. */
interface OxaKeybindings {
  getKeys(id: string): KeyId[];
  matches(key: KeyId, id: string): boolean;
}

type ExtensionMode = "tui" | "rpc" | "json" | "print";

interface ContextUsage {
  tokens: number;
  contextWindow: number;
  percent: number;
}

/** pi's `CompactionResult`, given to `onComplete`. */
interface CompactionResult {
  summary: string;
  firstKeptEntryId: string;
  tokensBefore: number;
  details?: JsonValue;
}

interface CompactOptions {
  /** pi's name. */
  customInstructions?: string;
  /** Alias of `customInstructions`. */
  instructions?: string;
  /** Called after the compaction succeeded (at its `session_compact`). */
  onComplete?: (result: CompactionResult) => void;
  /** Called when it failed (at its `session_compact_failed`). */
  onError?: (error: Error) => void;
}

interface SessionManagerView {
  getCwd(): string;
  getSessionId(): string;
  getSessionName(): string | undefined;
  getEntries(): JsonValue[];
  getEntry(id: string): JsonValue | undefined;
  getBranch(): JsonValue[];
  getLeafId(): string | null;
  getLeafEntry(): JsonValue | undefined;
}

interface ExtensionContext {
  ui: ExtensionUIContext;
  /** The run mode; use "tui" to guard terminal-only UI such as custom components. */
  readonly mode: ExtensionMode;
  /** Whether dialogs reach a user (true in the TUI and RPC modes). */
  readonly hasUI: boolean;
  cwd: string;
  sessionManager: SessionManagerView;
  /** The thinking level in force. */
  readonly thinkingLevel: ThinkingLevel;
  /** The session's engine model (pi's `ctx.model`, reduced to what one engine has). */
  readonly model: { id: string; provider: "oxe"; contextWindow: number } | undefined;
  /**
   * Run another tool through the full path (policy, hooks, output shaping); pi's
   * `executeTool`. Never throws for tool failures: they come back with `isError`.
   */
  executeTool(name: string, args?: JsonValue): { content: Array<{ type: "text"; text: string }>; isError: boolean };
  getContextUsage(): ContextUsage | undefined;
  isIdle(): boolean;
  hasPendingMessages(): boolean;
  abort(): void;
  /** Start a compaction without waiting for it; the callbacks report the outcome. */
  compact(options?: CompactOptions): void;
  /** The system text of the frozen prefix in force (read-only; it changes at the next epoch, W3). */
  getSystemPrompt(): string;
  shutdown(): void;
  /** Files, through the session's policy and sandbox. A denial rejects with code "denied". */
  fs: {
    readFile(path: string): Promise<string>;
    writeFile(path: string, content: string): Promise<void>;
  };
  /** HTTP through the session's policy. */
  fetch(url: string, init?: { method?: string; headers?: Record<string, string>; body?: string }): Promise<{
    status: number;
    headers: Record<string, string>;
    text(): Promise<string>;
  }>;
}

interface ExtensionCommandContext extends ExtensionContext {
  /** Whether a dialog can reach the user (false in print and json modes). */
  readonly canPrompt: boolean;
  waitForIdle(): Promise<void>;
  fork(entryId: string): void;
  navigateTree(target: string, options?: { summarize?: boolean; instructions?: string }): void;
  reload(): void;
}

interface ExtensionToolContext extends ExtensionContext {
  /** Whether a dialog from a tool can reach the user (false in print and json modes). */
  readonly canPrompt: boolean;
}

// ---------------------------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------------------------

/** Results an event handler may return. Anything else (or nothing) continues. */
interface ToolCallEventResult {
  block?: boolean;
  reason?: string;
  input?: JsonValue;
}

interface ToolResultEventResult {
  content?: MessageContent;
  isError?: boolean;
  details?: JsonValue;
}

interface ContextEventResult {
  messages?: JsonValue[];
}

interface InputEventResult {
  action: "transform" | "handled" | "continue";
  text?: string;
}

/** A message in event payloads (oxa's wire form plus pi's record fields). `toolCalls[].args`
 * is the argument string as the model sent it (W2), so a `context` handler can return the list
 * unchanged or with messages appended. */
interface OxaMessage {
  role: "system" | "user" | "assistant" | "tool";
  content: string | null;
  reasoning?: string;
  toolCallId?: string;
  toolCalls?: Array<{ id: string; name: string; args: string }>;
  timestamp?: number;
  /** Assistant: `"stop" | "toolUse" | "length" | "aborted" | "error" | "pending"`. */
  stopReason?: string;
  model?: string;
  usage?: { prompt: number; completion: number; cached: number };
  /** Tool results only. */
  toolName?: string;
  isError?: boolean;
  errorMessage?: string;
}

/** pi's tool result object. */
interface ToolResultObject {
  content: Array<{ type: "text"; text: string }>;
  details: JsonValue;
}

/** Every event object carries `type` = the event name. */
interface EventBase<T extends string> {
  type: T;
}

interface ToolCallEvent extends EventBase<"tool_call"> {
  toolCallId: string;
  toolName: string;
  input: JsonValue;
}

interface ToolResultEvent extends EventBase<"tool_result"> {
  toolCallId: string;
  toolName: string;
  input: JsonValue;
  content: MessageContent;
  isError: boolean;
  details?: JsonValue;
}

interface InputEvent extends EventBase<"input"> {
  text: string;
  images?: JsonValue[];
  /** pi's default for prompts, `"interactive"`, in every mode (RPC included). */
  source: "interactive" | "rpc" | "extension";
}

interface ContextEvent extends EventBase<"context" | "context_with_system" | "before_provider_request"> {
  /** `context_with_system`: the system message (the frozen prefix) first; a reply must keep it
   * byte for byte (W3). */
  messages: OxaMessage[];
}

interface BeforeAgentStartBase extends EventBase<"before_agent_start"> {
  /** The system text of the frozen prefix in force (read-only: pi's `systemPromptOptions` are
   * not offered, the prefix changes only at an epoch). */
  systemPrompt: string;
}
/** A run started by a prompt. */
interface BeforeAgentStartUserEvent extends BeforeAgentStartBase {
  messageType: "user";
  prompt: string;
  images?: JsonValue[];
}
/** A run started by `oxa.sendMessage(.., {triggerTurn: true})` while idle. */
interface BeforeAgentStartCustomEvent extends BeforeAgentStartBase {
  messageType: "custom";
  message: { role: "custom"; customType: string; content: string; display: boolean; details: JsonValue };
}
type BeforeAgentStartEvent = BeforeAgentStartUserEvent | BeforeAgentStartCustomEvent;

interface ProjectTrustEvent extends EventBase<"project_trust"> {
  cwd: string;
}
interface ProjectTrustEventResult {
  trusted: "yes" | "no" | "undecided";
  remember?: boolean;
}
interface ResourcesDiscoverEvent extends EventBase<"resources_discover"> {
  cwd: string;
  reason: "startup" | "reload";
}
interface SessionStartEvent extends EventBase<"session_start"> {
  reason: "startup" | "new" | "resume" | "fork";
  previousSessionFile?: string;
}
interface SessionShutdownEvent extends EventBase<"session_shutdown"> {
  /** `quit` also when a front end replaces the session object (`Session::close`). */
  reason: "quit" | "new" | "resume" | "fork";
  targetSessionFile?: string;
}
interface SessionBeforeSwitchEvent extends EventBase<"session_before_switch"> {
  reason: "new" | "resume";
  targetSessionFile?: string;
}
interface SessionBeforeForkEvent extends EventBase<"session_before_fork"> {
  entryId: string;
  position: "at";
}
interface SessionBeforeCompactEvent extends EventBase<"session_before_compact"> {
  reason: "manual" | "threshold" | "overflow";
  customInstructions: string | null;
  /** Same as `customInstructions` (older oxa name). */
  instructions: string | null;
  tokensBefore: number;
  firstKeptEntryId: string;
  willRetry: boolean;
  preparation: { firstKeptEntryId: string; tokensBefore: number };
}
interface SessionCompactEvent extends EventBase<"session_compact"> {
  reason: "manual" | "threshold" | "overflow";
  summary: string;
  firstKeptEntryId: string;
  tokensBefore: number;
  fromExtension: boolean;
  willRetry: boolean;
  /** The model summary failed and a mechanical record replaced it. */
  mechanical?: true;
  /** With `mechanical`: why the model summary failed. */
  summaryError?: string;
}
interface SessionCompactFailedEvent extends EventBase<"session_compact_failed"> {
  reason: "manual" | "threshold" | "overflow";
  errorMessage: string;
  aborted: boolean;
  willRetry: boolean;
  fromExtension: boolean;
}
interface SessionBeforeTreeEvent extends EventBase<"session_before_tree"> {
  preparation: {
    targetId: string;
    oldLeafId: string;
    userWantsSummary: boolean;
    customInstructions: string | null;
    entriesToSummarize: string[];
  };
}
interface SessionTreeEvent extends EventBase<"session_tree"> {
  newLeafId: string;
  oldLeafId: string;
  summaryEntryId: string | null;
  fromExtension: boolean;
}
interface SessionInfoChangedEvent extends EventBase<"session_info_changed"> {
  /** Absent when the name was cleared. */
  name?: string;
}
interface UserBashEvent extends EventBase<"user_bash"> {
  command: string;
  excludeFromContext: boolean;
  cwd: string;
}
interface AgentEndEvent extends EventBase<"agent_end"> {
  /** The messages this run added. */
  messages: OxaMessage[];
}
interface AgentBeforeSettleEvent extends EventBase<"agent_before_settle"> {
  entries: JsonValue[];
  continue: boolean;
  outcome: "completed";
}
/** Only `custom_message` drafts are applied (appended as user messages; the loop runs again). */
interface AgentBeforeSettleEventResult {
  entries?: Array<{ type: "custom_message"; customType?: string; content: MessageContent }>;
}
interface AgentSettledEvent extends EventBase<"agent_settled"> {
  aborted: boolean;
}
interface TurnStartEvent extends EventBase<"turn_start"> {
  turnIndex: number;
  timestamp: number;
}
interface TurnEndEvent extends EventBase<"turn_end"> {
  turnIndex: number;
  message: OxaMessage;
  toolResults: OxaMessage[];
  messageEntryId: string;
  toolResultEntryIds: string[];
  entries: JsonValue[];
  continue: boolean;
  outcome: "completed" | "aborted";
}
interface MessageEvent extends EventBase<"message_start" | "message_end"> {
  message: OxaMessage;
}
interface MessageUpdateEvent extends EventBase<"message_update"> {
  /** The assistant message so far. */
  message: OxaMessage;
  /** pi's delta event: `{type: "text_delta" | "thinking_delta" | "toolcall_delta" | ..., contentIndex, ...}`. */
  assistantMessageEvent: JsonValue;
}
interface AfterProviderResponseEvent extends EventBase<"after_provider_response"> {
  status: number;
  /** Always empty: the engine client does not hand the headers up. */
  headers: { [name: string]: string };
}
interface ProviderStreamEvent extends EventBase<"provider_stream_event"> {
  provider: "oxe";
  api: "openai-completions";
  model: string;
  /** The engine increment: `{type: "text" | "reasoning", delta}`, `{type: "tool_args", ...}`, `{type: "prefill" | "queued", ...}`. */
  data: JsonValue;
}
interface CacheWarmingDecisionEvent extends EventBase<"cache_warming_decision"> {
  wireMessages: number;
}
interface ToolExecutionStartEvent extends EventBase<"tool_execution_start"> {
  toolCallId: string;
  toolName: string;
  args: JsonValue;
}
interface ToolExecutionUpdateEvent extends EventBase<"tool_execution_update"> {
  toolCallId: string;
  toolName: string;
  args: JsonValue;
  partialResult: ToolResultObject;
}
interface ToolExecutionEndEvent extends EventBase<"tool_execution_end"> {
  toolCallId: string;
  toolName: string;
  result: ToolResultObject;
  isError: boolean;
  durationMs?: number;
}
interface ModelSelectEvent extends EventBase<"model_select"> {
  model: string;
  previousModel: string;
  source: "set";
}
interface ThinkingLevelSelectEvent extends EventBase<"thinking_level_select"> {
  level: string;
  previousLevel: string;
}
interface UIPromptEvent extends EventBase<"ui_prompt_start" | "ui_prompt_end"> {
  reason: "ui_prompt";
  kind: "select" | "confirm" | "input" | "editor" | "custom";
  title?: string;
}

/** Every event name oxa keeps, with its payload. */
interface OxaEvents {
  project_trust: ProjectTrustEvent;
  resources_discover: ResourcesDiscoverEvent;
  session_start: SessionStartEvent;
  session_shutdown: SessionShutdownEvent;
  session_before_switch: SessionBeforeSwitchEvent;
  session_before_fork: SessionBeforeForkEvent;
  session_before_compact: SessionBeforeCompactEvent;
  session_compact: SessionCompactEvent;
  session_compact_failed: SessionCompactFailedEvent;
  session_before_tree: SessionBeforeTreeEvent;
  session_tree: SessionTreeEvent;
  session_info_changed: SessionInfoChangedEvent;
  input: InputEvent;
  user_bash: UserBashEvent;
  before_agent_start: BeforeAgentStartEvent;
  agent_start: EventBase<"agent_start">;
  agent_end: AgentEndEvent;
  agent_before_settle: AgentBeforeSettleEvent;
  agent_settled: AgentSettledEvent;
  turn_start: TurnStartEvent;
  turn_end: TurnEndEvent;
  message_start: MessageEvent;
  message_update: MessageUpdateEvent;
  message_end: MessageEvent;
  context: ContextEvent;
  context_with_system: ContextEvent;
  before_provider_request: ContextEvent;
  after_provider_response: AfterProviderResponseEvent;
  provider_stream_event: ProviderStreamEvent;
  cache_warming_decision: CacheWarmingDecisionEvent;
  tool_call: ToolCallEvent;
  tool_result: ToolResultEvent;
  tool_execution_start: ToolExecutionStartEvent;
  tool_execution_update: ToolExecutionUpdateEvent;
  tool_execution_end: ToolExecutionEndEvent;
  model_select: ModelSelectEvent;
  thinking_level_select: ThinkingLevelSelectEvent;
  ui_prompt_start: UIPromptEvent;
  ui_prompt_end: UIPromptEvent;
}

type OxaEventName = keyof OxaEvents;

/** What a handler for `E` may return to change the outcome. */
type OxaEventResult<E extends OxaEventName> = E extends "tool_call"
  ? ToolCallEventResult
  : E extends "tool_result"
    ? ToolResultEventResult
    : E extends "input"
      ? InputEventResult
      : E extends "context" | "context_with_system" | "before_provider_request"
        ? ContextEventResult
        : E extends "project_trust"
          ? ProjectTrustEventResult
          : E extends "agent_before_settle"
            ? AgentBeforeSettleEventResult
            : undefined;

type OxaHandler<E extends OxaEventName> = (
  event: OxaEvents[E],
  ctx: ExtensionContext,
) => OxaEventResult<E> | void | Promise<OxaEventResult<E> | void>;

// ---------------------------------------------------------------------------------------------
// Registration shapes
// ---------------------------------------------------------------------------------------------

interface ToolDefinition {
  /** Matches [A-Za-z0-9_-]{1,64}; must not start with "mcp__". */
  name: string;
  label?: string;
  description: string;
  /** JSON Schema of the arguments, `type: "object"`. Validated when the extension loads. */
  parameters: JsonSchema;
  /** Access class: what the ask mode asks for. Default "exec". */
  class?: ToolClass;
  /** Default "direct". */
  exposure?: ToolExposure;
  /**
   * pi's execution mode. Extension tools may open dialogs, so oxa runs them one at a time
   * whatever the value.
   */
  executionMode?: "sequential" | "parallel";
  execute(
    toolCallId: string,
    params: JsonValue,
    signal: AbortSignal,
    onUpdate: (partial: { content: MessageContent }) => void,
    ctx: ExtensionToolContext,
  ): Promise<{ content: MessageContent; isError?: boolean; details?: JsonValue }> | { content: MessageContent; isError?: boolean; details?: JsonValue };
  renderCall?(args: JsonValue, ctx: JsonValue): string[];
  renderResult?(result: JsonValue, options: JsonValue, ctx: JsonValue): string[];
}

interface CommandDefinition {
  description?: string;
  handler(args: string, ctx: ExtensionCommandContext): Promise<void> | void;
  getArgumentCompletions?(prefix: string): string[] | Promise<string[]>;
}

interface FlagDefinition {
  description?: string;
  type: "boolean" | "string";
  default?: boolean | string;
}

interface ShortcutDefinition {
  description?: string;
  handler(ctx: ExtensionContext): Promise<void> | void;
}

interface MessageRenderOptions {
  width: number;
  expanded: boolean;
}

interface MarkdownTransformContext {
  messageType: "user" | "assistant" | "assistant-thinking" | "custom" | "summary";
}

/** pi's `SlashCommandInfo`. */
interface SlashCommandInfo {
  name: string;
  description?: string;
  source: "extension" | "prompt" | "skill";
  sourceInfo: {
    path: string;
    source: string;
    scope: "user" | "project" | "temporary";
    origin: "package" | "top-level";
    baseDir?: string;
  };
}

interface EventBus {
  on(name: string, handler: (data: JsonValue) => void): Disposable;
  emit(name: string, data?: JsonValue): void;
}

// ---------------------------------------------------------------------------------------------
// The factory argument
// ---------------------------------------------------------------------------------------------

interface Oxa {
  // Events
  on<E extends OxaEventName>(event: E, handler: OxaHandler<E>): Disposable;

  // Tools, commands, shortcuts, flags
  registerTool(tool: ToolDefinition): void;
  registerCommand(name: string, options: CommandDefinition): void;
  registerShortcut(key: KeyId, options: ShortcutDefinition): void;
  registerFlag(name: string, options: FlagDefinition): void;
  getFlag(name: string): boolean | string | undefined;

  // Rendering (F3)
  registerMessageRenderer(customType: string, renderer: (message: JsonValue, options: MessageRenderOptions) => string[] | undefined): void;
  registerEntryRenderer(customType: string, renderer: (entry: JsonValue, options: MessageRenderOptions) => string[] | undefined): void;
  registerToolRenderer(resolver: (toolName: string) => { renderCall?: ToolDefinition["renderCall"]; renderResult?: ToolDefinition["renderResult"] } | undefined): void;
  registerMarkdownTransformer(
    transformer: (markdown: string, context: MarkdownTransformContext) => string,
    options?: { messageTypes?: string },
  ): void;

  // Actions
  sendMessage(
    message: { customType: string; content: MessageContent; display: boolean; details?: JsonValue },
    options?: { triggerTurn?: boolean; deliverAs?: DeliveryMode },
  ): void;
  sendUserMessage(content: MessageContent, options?: { deliverAs?: "steer" | "followUp"; expandPromptTemplates?: boolean }): void;
  appendEntry(customType: string, data?: JsonValue): void;
  setSessionName(name: string): void;
  getSessionName(): string | null;
  setLabel(entryId: string, label: string | undefined): void;
  /**
   * Switch to an engine model (an id, or pi's model object with `id`). Throws `not_found` for a
   * model the engine does not serve; otherwise `true`, applied when the run settles (new epoch).
   */
  setModel(model: string | { id: string }): boolean;
  getThinkingLevel(): ThinkingLevel;
  setThinkingLevel(level: ThinkingLevel): void;
  getActiveTools(): string[];
  getAllTools(): Array<{ name: string; description: string; parameters: JsonSchema; source: string; exposure: ToolExposure }>;
  setActiveTools(toolNames: string[]): void;
  /** A copy of the effective settings (defaults < user < trusted project). */
  getSettings(): { [key: string]: JsonValue };
  /** Slash commands invocable through a prompt: extension commands, prompt templates, skills. */
  getCommands(): SlashCommandInfo[];
  addPromptSection(name: string, text: string): void;

  /**
   * Run a program, with the session's policy and sandbox. Synchronous in oxa (`await` works).
   * pi's `ExecResult` fields (`stdout`, `stderr`, `code`, `killed`) and the core's (`output`,
   * `exit`, `timedOut`): the output is stdout and stderr interleaved, so `stdout` = `output` and
   * `stderr` is empty; `code` / `exit` is null when the program was killed (`killed` /
   * `timedOut`).
   */
  exec(
    command: string,
    args: string[],
    options?: { cwd?: string; timeout?: number; net?: boolean },
  ): {
    stdout: string;
    stderr: string;
    code: number | null;
    killed: boolean;
    output: string;
    exit: number | null;
    timedOut: boolean;
  };

  /** The inter-extension bus. */
  events: EventBus;
}

/** The `oxa` object passed to the factory; the same object is also the global `oxa`. */
declare const oxa: Oxa;

/** The default export of an extension. */
type ExtensionFactory = (oxa: Oxa) => void | Promise<void>;

// Optional named export `export const manifest = { name?, description?, rebase? }` of an extension
// module: read after the module loads, before the factory's effects are used (see E1 `merge_exported_manifest`).

// ---------------------------------------------------------------------------------------------
// The oxa:tui shim (F2): pi-tui's shape.
// ---------------------------------------------------------------------------------------------

declare module "oxa:tui" {
  export class Text {
    constructor(text: string, paddingX?: number, paddingY?: number);
    setText(text: string): void;
    render(width: number): string[];
  }
  export class Box {
    constructor(paddingX?: number, paddingY?: number);
    addChild(child: { render(width: number): string[] }): void;
    render(width: number): string[];
  }
  export class Container {
    addChild(child: { render(width: number): string[] }): void;
    render(width: number): string[];
  }
  export class Spacer {
    constructor(lines?: number);
    render(width: number): string[];
  }
  export class SelectList {
    constructor(items: Array<{ value: string; label: string; description?: string }>, maxVisible?: number);
    render(width: number): string[];
    handleInput(data: string): void;
  }
  export class Input {
    getValue(): string;
    setValue(value: string): void;
    render(width: number): string[];
    handleInput(data: string): void;
  }
  export interface SettingItem {
    id: string;
    label: string;
    description?: string;
    currentValue: string;
    values?: string[];
    submenu?: (currentValue: string, done: (selectedValue?: string, options?: { navigateTo?: string }) => void) => OxaComponent;
  }
  export class SettingsList {
    constructor(
      items: SettingItem[],
      maxVisible: number,
      theme: {
        label?: (text: string, selected: boolean) => string;
        value?: (text: string, selected: boolean) => string;
        description?: (text: string) => string;
        cursor?: string;
        hint?: (text: string) => string;
      } | undefined,
      onChange: (id: string, newValue: string) => void,
      onCancel: () => void,
      options?: { enableSearch?: boolean },
    );
    updateValue(id: string, newValue: string): void;
    selectItem(id: string): void;
    render(width: number): string[];
    handleInput(key: KeyId, text?: string | null): void;
  }
  export class Editor {
    constructor(tui?: OxaTui, theme?: { borderColor?: (s: string) => string }, options?: unknown);
    onSubmit?: (text: string) => void;
    onChange?: (text: string) => void;
    disableSubmit: boolean;
    getText(): string;
    getLines(): string[];
    getCursor(): { line: number; col: number };
    setText(text: string): void;
    insertTextAtCursor(text: string): void;
    render(width: number): string[];
    handleInput(key: KeyId, text?: string | null): void;
  }
  export function matchesKey(data: string, key: KeyId): boolean;
  export function visibleWidth(text: string): number;
  export function truncateToWidth(text: string, width: number, ellipsis?: string): string;
  export function wrapTextWithAnsi(text: string, width: number): string[];
}
