// ported from pi packages/coding-agent/examples/extensions/plan-mode/index.ts (utils.ts is inlined below:
// one file per extension, F9 owns index.js only)
//
// Changes from pi, all forced by oxa's API (see examples/extensions/README.md):
// - pi.* -> oxa.*; theme colours are dropped (oxa has no ctx.ui.theme); Key.ctrlAlt("p") -> "ctrl+alt+p".
// - the `context` filter is dropped. Removing messages from the history is a rewrite that is not
//   append-safe (W1), so the W-guard would refuse it. Stale plan context stays in the history.
// - before_agent_start carries no messageType in oxa (only user prompts fire it).
// - oxa's agent_end and turn_end carry no messages: the last assistant message is read from
//   ctx.sessionManager.getBranch().

// ---- utils.ts -------------------------------------------------------------------------------

// Destructive commands blocked in plan mode
const DESTRUCTIVE_PATTERNS = [
	/\brm\b/i,
	/\brmdir\b/i,
	/\bmv\b/i,
	/\bcp\b/i,
	/\bmkdir\b/i,
	/\btouch\b/i,
	/\bchmod\b/i,
	/\bchown\b/i,
	/\bchgrp\b/i,
	/\bln\b/i,
	/\btee\b/i,
	/\btruncate\b/i,
	/\bdd\b/i,
	/\bshred\b/i,
	/(^|[^<])>(?!>)/,
	/>>/,
	/\bnpm\s+(install|uninstall|update|ci|link|publish)/i,
	/\byarn\s+(add|remove|install|publish)/i,
	/\bpnpm\s+(add|remove|install|publish)/i,
	/\bpip\s+(install|uninstall)/i,
	/\bapt(-get)?\s+(install|remove|purge|update|upgrade)/i,
	/\bbrew\s+(install|uninstall|upgrade)/i,
	/\bgit\s+(add|commit|push|pull|merge|rebase|reset|checkout|branch\s+-[dD]|stash|cherry-pick|revert|tag|init|clone)/i,
	/\bsudo\b/i,
	/\bsu\b/i,
	/\bkill\b/i,
	/\bpkill\b/i,
	/\bkillall\b/i,
	/\breboot\b/i,
	/\bshutdown\b/i,
	/\bsystemctl\s+(start|stop|restart|enable|disable)/i,
	/\bservice\s+\S+\s+(start|stop|restart)/i,
	/\b(vim?|nano|emacs|code|subl)\b/i,
];

// Safe read-only commands allowed in plan mode
const SAFE_PATTERNS = [
	/^\s*cat\b/,
	/^\s*head\b/,
	/^\s*tail\b/,
	/^\s*less\b/,
	/^\s*more\b/,
	/^\s*grep\b/,
	/^\s*find\b/,
	/^\s*ls\b/,
	/^\s*pwd\b/,
	/^\s*echo\b/,
	/^\s*printf\b/,
	/^\s*wc\b/,
	/^\s*sort\b/,
	/^\s*uniq\b/,
	/^\s*diff\b/,
	/^\s*file\b/,
	/^\s*stat\b/,
	/^\s*du\b/,
	/^\s*df\b/,
	/^\s*tree\b/,
	/^\s*which\b/,
	/^\s*whereis\b/,
	/^\s*type\b/,
	/^\s*env\b/,
	/^\s*printenv\b/,
	/^\s*uname\b/,
	/^\s*whoami\b/,
	/^\s*id\b/,
	/^\s*date\b/,
	/^\s*cal\b/,
	/^\s*uptime\b/,
	/^\s*ps\b/,
	/^\s*top\b/,
	/^\s*htop\b/,
	/^\s*free\b/,
	/^\s*git\s+(status|log|diff|show|branch|remote|config\s+--get)/i,
	/^\s*git\s+ls-/i,
	/^\s*npm\s+(list|ls|view|info|search|outdated|audit)/i,
	/^\s*yarn\s+(list|info|why|audit)/i,
	/^\s*node\s+--version/i,
	/^\s*python\s+--version/i,
	/^\s*curl\s/i,
	/^\s*wget\s+-O\s*-/i,
	/^\s*jq\b/,
	/^\s*sed\s+-n/i,
	/^\s*awk\b/,
	/^\s*rg\b/,
	/^\s*fd\b/,
	/^\s*bat\b/,
	/^\s*eza\b/,
];

function isSafeCommand(command) {
	const isDestructive = DESTRUCTIVE_PATTERNS.some((p) => p.test(command));
	const isSafe = SAFE_PATTERNS.some((p) => p.test(command));
	return !isDestructive && isSafe;
}

function cleanStepText(text) {
	let cleaned = text
		.replace(/\*{1,2}([^*]+)\*{1,2}/g, "$1") // Remove bold/italic
		.replace(/`([^`]+)`/g, "$1") // Remove code
		.replace(
			/^(Use|Run|Execute|Create|Write|Read|Check|Verify|Update|Modify|Add|Remove|Delete|Install)\s+(the\s+)?/i,
			"",
		)
		.replace(/\s+/g, " ")
		.trim();

	if (cleaned.length > 0) {
		cleaned = cleaned.charAt(0).toUpperCase() + cleaned.slice(1);
	}
	// Keep full text: the exec message and widget display need the whole step.
	return cleaned;
}

function extractTodoItems(message) {
	const items = [];
	// Take the LAST "Plan:" section: a draft in a reasoning block can precede the visible plan.
	const headers = [...message.matchAll(/\*{0,2}Plan:\*{0,2}\s*\n/gi)];
	if (headers.length === 0) return items;
	const lastHeader = headers[headers.length - 1];
	const planSection = message.slice((lastHeader.index ?? 0) + lastHeader[0].length);
	const numberedPattern = /^\s*(\d+)[.)]\s+(.+)$/gm;

	for (const match of planSection.matchAll(numberedPattern)) {
		const text = match[2].trim().replace(/\*{1,2}$/, "").trim();
		if (text.length > 5 && !text.startsWith("/") && !text.startsWith("-")) {
			const cleaned = cleanStepText(text);
			if (cleaned.length > 3) {
				items.push({ step: items.length + 1, text: cleaned, completed: false });
			}
		}
	}
	return items;
}

function extractDoneSteps(message) {
	const steps = [];
	// Canonical and bracket variants: [DONE:1], [DONE: 1], [DONE 1], [Done:1]
	for (const match of message.matchAll(/\[DONE\s*:?\s*(\d+)\]/gi)) {
		steps.push(Number(match[1]));
	}
	// "Done: 1" without brackets
	for (const match of message.matchAll(/\bDone\s*:\s*(\d+)\b/gi)) {
		steps.push(Number(match[1]));
	}
	// "Step 1 done", "Step 2 is complete", "Step 3 finished"
	for (const match of message.matchAll(/\bStep\s*(\d+)\s+(?:is\s+)?(?:done|complete|completed|finished)\b/gi)) {
		steps.push(Number(match[1]));
	}
	return [...new Set(steps)];
}

function markCompletedSteps(text, items) {
	const doneSteps = extractDoneSteps(text);
	let marked = 0;
	for (const step of doneSteps) {
		const item = items.find((t) => t.step === step && !t.completed);
		if (item) {
			item.completed = true;
			marked++;
		}
	}
	return marked;
}

// Whole-plan completion phrasing, used as a fallback when the model summarizes without [DONE:n].
function allStepsComplete(message) {
	return (
		/(?:all|every)\s+steps?\s+(?:are|is\s+)?(?:done|complete|completed|finished)/i.test(message) ||
		/\b(?:the\s+)?plan\s+(?:is\s+)?(?:complete|completed|done)\b/i.test(message)
	);
}

// ---- helpers over oxa's session branch ------------------------------------------------------

function isAssistantEntry(e) {
	return !!e && e.type === "message" && !!e.message && e.message.role === "assistant";
}

// Text of an assistant message; includes thinking blocks: with reasoning models the [DONE:n] tags
// and plan drafts often land in thinking, not in the visible text.
function getTextContent(message) {
	if (typeof message.content === "string") return message.content;
	if (!Array.isArray(message.content)) return "";
	return message.content
		.map((block) => {
			if (block.type === "text") return block.text;
			if (block.type === "thinking") return block.thinking ?? "";
			return "";
		})
		.join("\n");
}

// The text of the last assistant message of `entries`, or null.
function lastAssistantText(entries) {
	for (let i = entries.length - 1; i >= 0; i--) {
		if (isAssistantEntry(entries[i])) return getTextContent(entries[i].message);
	}
	return null;
}

// ---- the extension --------------------------------------------------------------------------

const PLAN_MODE_TOOLS = ["read", "bash", "grep", "find", "ls", "questionnaire"];
const NORMAL_MODE_TOOLS = ["read", "bash", "edit", "write"];
const PLAN_MODE_DISABLED_TOOLS = new Set(["edit", "write"]);
const PLAN_MANAGED_TOOLS = new Set([...PLAN_MODE_TOOLS, ...NORMAL_MODE_TOOLS]);

export default function planModeExtension(oxa) {
	let planModeEnabled = false;
	let executionMode = false;
	let todoItems = [];
	let toolsBeforePlanMode;

	oxa.registerFlag("plan", {
		description: "Start in plan mode (read-only exploration)",
		type: "boolean",
		default: false,
	});

	function updateStatus(ctx) {
		// Footer status
		if (executionMode && todoItems.length > 0) {
			const completed = todoItems.filter((t) => t.completed).length;
			ctx.ui.setStatus("plan-mode", `📋 ${completed}/${todoItems.length}`);
		} else if (planModeEnabled) {
			ctx.ui.setStatus("plan-mode", "⏸ plan");
		} else {
			ctx.ui.setStatus("plan-mode", undefined);
		}

		// Widget showing todo list
		if (executionMode && todoItems.length > 0) {
			const lines = todoItems.map((item) => {
				const text = item.text.length > 60 ? `${item.text.slice(0, 57)}...` : item.text;
				return item.completed ? `☑ ${text}` : `☐ ${text}`;
			});
			ctx.ui.setWidget("plan-todos", lines);
		} else {
			ctx.ui.setWidget("plan-todos", undefined);
		}
	}

	function uniqueToolNames(toolNames) {
		return [...new Set(toolNames)];
	}

	function getPlanModeTools(activeToolNames) {
		return uniqueToolNames([
			...activeToolNames.filter((name) => !PLAN_MODE_DISABLED_TOOLS.has(name)),
			...PLAN_MODE_TOOLS,
		]);
	}

	function getNormalModeTools(activeToolNames) {
		return uniqueToolNames([
			...NORMAL_MODE_TOOLS,
			...activeToolNames.filter((name) => !PLAN_MANAGED_TOOLS.has(name)),
		]);
	}

	function enablePlanModeTools() {
		if (toolsBeforePlanMode === undefined) {
			toolsBeforePlanMode = oxa.getActiveTools();
		}
		oxa.setActiveTools(getPlanModeTools(toolsBeforePlanMode));
	}

	function restoreNormalModeTools() {
		oxa.setActiveTools(toolsBeforePlanMode ?? getNormalModeTools(oxa.getActiveTools()));
		toolsBeforePlanMode = undefined;
	}

	function persistState() {
		oxa.appendEntry("plan-mode", {
			enabled: planModeEnabled,
			todos: todoItems,
			executing: executionMode,
			toolsBeforePlanMode,
		});
	}

	function togglePlanMode(ctx) {
		planModeEnabled = !planModeEnabled;
		executionMode = false;
		todoItems = [];

		if (planModeEnabled) {
			enablePlanModeTools();
			ctx.ui.notify("Plan mode enabled. Built-in write tools disabled.");
		} else {
			restoreNormalModeTools();
			ctx.ui.notify("Plan mode disabled. Full access restored.");
		}
		updateStatus(ctx);
		persistState();
	}

	oxa.registerCommand("plan", {
		description: "Toggle plan mode (read-only exploration)",
		handler: async (_args, ctx) => togglePlanMode(ctx),
	});

	oxa.registerCommand("todos", {
		description: "Show current plan todo list",
		handler: async (_args, ctx) => {
			if (todoItems.length === 0) {
				ctx.ui.notify("No todos. Create a plan first with /plan", "info");
				return;
			}
			const list = todoItems.map((item, i) => `${i + 1}. ${item.completed ? "✓" : "○"} ${item.text}`).join("\n");
			ctx.ui.notify(`Plan Progress:\n${list}`, "info");
		},
	});

	oxa.registerShortcut("ctrl+alt+p", {
		description: "Toggle plan mode",
		handler: async (ctx) => togglePlanMode(ctx),
	});

	// Block destructive bash commands in plan mode
	oxa.on("tool_call", async (event) => {
		if (!planModeEnabled || event.toolName !== "bash") return;

		const command = typeof event.input?.command === "string" ? event.input.command : "";
		if (!isSafeCommand(command)) {
			return {
				block: true,
				reason: `Plan mode: command blocked (not allowlisted). Use /plan to disable plan mode first.\nCommand: ${command}`,
			};
		}
	});

	// Inject plan/execution context before a user prompt.
	oxa.on("before_agent_start", async () => {
		if (planModeEnabled) {
			return {
				message: {
					customType: "plan-mode-context",
					content: `[PLAN MODE ACTIVE]
You are in plan mode - a read-only exploration mode for safe code analysis.

Restrictions:
- Built-in edit and write tools are disabled
- Other currently active tools remain available
- Bash is restricted to an allowlist of read-only commands

Ask clarifying questions using the questionnaire tool.
Use brave-search skill via bash for web research.

Create a detailed numbered plan under a "Plan:" header:

Plan:
1. First step description
2. Second step description
...

Do NOT attempt to make changes - just describe what you would do.`,
					display: false,
				},
			};
		}

		if (executionMode && todoItems.length > 0) {
			const remaining = todoItems.filter((t) => !t.completed);
			const todoList = remaining.map((t) => `${t.step}. ${t.text}`).join("\n");
			return {
				message: {
					customType: "plan-execution-context",
					content: `[EXECUTING PLAN - Full tool access enabled]

Remaining steps:
${todoList}

Execute each step in order.
After completing a step, include a [DONE:n] tag in your response.`,
					display: false,
				},
			};
		}
	});

	// Track progress after each turn
	oxa.on("turn_end", async (_event, ctx) => {
		if (!executionMode || todoItems.length === 0) return;
		const text = lastAssistantText(ctx.sessionManager.getBranch());
		if (text === null) return;

		markCompletedSteps(text, todoItems);
		// Always refresh: cheap, keeps the widget consistent even when nothing matched.
		updateStatus(ctx);
		persistState();
	});

	// Handle plan completion and plan mode UI
	oxa.on("agent_end", async (_event, ctx) => {
		// Check if execution is complete
		if (executionMode && todoItems.length > 0) {
			// Fallback: the model wrapped up without [DONE:n] tags ("All steps complete").
			const lastText = lastAssistantText(ctx.sessionManager.getBranch());
			if (lastText !== null && allStepsComplete(lastText)) {
				for (const t of todoItems) t.completed = true;
			}
			if (todoItems.every((t) => t.completed)) {
				const completedList = todoItems.map((t) => `~~${t.text}~~`).join("\n");
				oxa.sendMessage(
					{ customType: "plan-complete", content: `**Plan Complete!** ✓\n\n${completedList}`, display: true },
					// nextTurn: delivered with the next user prompt instead of triggering an extra turn.
					{ deliverAs: "nextTurn" },
				);
				executionMode = false;
				todoItems = [];
				updateStatus(ctx);
				persistState(); // Save cleared state so resume doesn't restore old execution mode
			}
			return;
		}

		if (!planModeEnabled || !ctx.hasUI) return;

		// Extract todos from last assistant message
		const extractedFrom = lastAssistantText(ctx.sessionManager.getBranch());
		if (extractedFrom !== null) {
			const extracted = extractTodoItems(extractedFrom);
			if (extracted.length > 0) {
				todoItems = extracted;
			}
		}

		if (todoItems.length === 0) return;
		persistState();

		// Show plan steps and prompt for next action
		const todoListText = todoItems.map((t, i) => `${i + 1}. ☐ ${t.text}`).join("\n");
		const planTodoListMessage = {
			customType: "plan-todo-list",
			content: `**Plan Steps (${todoItems.length}):**\n\n${todoListText}`,
			display: true,
		};

		const choice = await ctx.ui.select("Plan mode - what next?", [
			"Execute the plan (track progress)",
			"Stay in plan mode",
			"Refine the plan",
		]);

		if (choice?.startsWith("Execute")) {
			const firstTodoItem = todoItems[0];
			if (!firstTodoItem) return;

			planModeEnabled = false;
			executionMode = true;
			restoreNormalModeTools();
			updateStatus(ctx);
			persistState();

			const remainingList = todoItems.map((t) => `${t.step}. ${t.text}`).join("\n");
			const execMessage = `Execute the plan.

Remaining steps:
${remainingList}

Start with: ${firstTodoItem.text}
After completing a step, include a [DONE:n] tag in your response.`;
			oxa.sendMessage(planTodoListMessage, { deliverAs: "followUp" });
			oxa.sendMessage(
				{ customType: "plan-mode-execute", content: execMessage, display: true },
				{ triggerTurn: true, deliverAs: "followUp" },
			);
		} else if (choice === "Refine the plan") {
			const refinement = await ctx.ui.editor("Refine the plan:", "");
			if (refinement?.trim()) {
				oxa.sendMessage(planTodoListMessage, { deliverAs: "followUp" });
				oxa.sendUserMessage(refinement.trim(), { deliverAs: "followUp" });
			}
		}
	});

	// Restore state on session start/resume
	oxa.on("session_start", async (_event, ctx) => {
		if (oxa.getFlag("plan") === true) {
			planModeEnabled = true;
		}

		const entries = ctx.sessionManager.getEntries();

		// Restore persisted state
		const planModeEntry = entries
			.filter((e) => e.type === "custom" && e.customType === "plan-mode")
			.pop();

		if (planModeEntry && planModeEntry.data) {
			planModeEnabled = planModeEntry.data.enabled ?? planModeEnabled;
			todoItems = planModeEntry.data.todos ?? todoItems;
			executionMode = planModeEntry.data.executing ?? executionMode;
			toolsBeforePlanMode = planModeEntry.data.toolsBeforePlanMode ?? toolsBeforePlanMode;
		}

		// On resume: re-scan messages to rebuild completion state.
		// Only scan messages AFTER the last "plan-mode-execute" to avoid picking up [DONE:n] from previous plans.
		const isResume = planModeEntry !== undefined;
		if (isResume && executionMode && todoItems.length > 0) {
			let executeIndex = -1;
			for (let i = entries.length - 1; i >= 0; i--) {
				if (entries[i].customType === "plan-mode-execute") {
					executeIndex = i;
					break;
				}
			}

			const texts = [];
			for (let i = executeIndex + 1; i < entries.length; i++) {
				if (isAssistantEntry(entries[i])) texts.push(getTextContent(entries[i].message));
			}
			markCompletedSteps(texts.join("\n"), todoItems);
		}

		if (planModeEnabled) {
			enablePlanModeTools();
		}
		updateStatus(ctx);
	});
}
