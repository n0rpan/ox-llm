// ported from pi packages/coding-agent/examples/extensions/todo.ts
//
// Changes from pi (mechanical): pi.* -> oxa.*; the types are removed; pi-tui imports come from
// `oxa:tui`; the TypeBox schema is the JSON Schema it compiles to; `renderCall` / `renderResult`
// return lines (oxa's renderer shape) instead of a `Text` component, without theme colours.
//
// Todo Extension - Demonstrates state management via session entries
//
// This extension:
// - Registers a `todo` tool for the LLM to manage todos
// - Registers a `/todos` command for users to view the list
//
// State is stored in tool result details (not external files), which allows
// proper branching - when you branch, the todo state is automatically
// correct for that point in history.

import { matchesKey, truncateToWidth } from "oxa:tui";

/**
 * UI component for the /todos command
 */
class TodoListComponent {
	constructor(todos, theme, onClose) {
		this.todos = todos;
		this.theme = theme;
		this.onClose = onClose;
		this.cachedWidth = undefined;
		this.cachedLines = undefined;
	}

	handleInput(data) {
		if (matchesKey(data, "escape") || matchesKey(data, "ctrl+c")) {
			this.onClose();
		}
	}

	render(width) {
		if (this.cachedLines && this.cachedWidth === width) {
			return this.cachedLines;
		}

		const lines = [];
		const th = this.theme;

		lines.push("");
		const title = th.fg("accent", " Todos ");
		const headerLine =
			th.fg("borderMuted", "─".repeat(3)) + title + th.fg("borderMuted", "─".repeat(Math.max(0, width - 10)));
		lines.push(truncateToWidth(headerLine, width));
		lines.push("");

		if (this.todos.length === 0) {
			lines.push(truncateToWidth(`  ${th.fg("dim", "No todos yet. Ask the agent to add some!")}`, width));
		} else {
			const done = this.todos.filter((t) => t.done).length;
			const total = this.todos.length;
			lines.push(truncateToWidth(`  ${th.fg("muted", `${done}/${total} completed`)}`, width));
			lines.push("");

			for (const todo of this.todos) {
				const check = todo.done ? th.fg("success", "✓") : th.fg("dim", "○");
				const id = th.fg("accent", `#${todo.id}`);
				const text = todo.done ? th.fg("dim", todo.text) : th.fg("text", todo.text);
				lines.push(truncateToWidth(`  ${check} ${id} ${text}`, width));
			}
		}

		lines.push("");
		lines.push(truncateToWidth(`  ${th.fg("dim", "Press Escape to close")}`, width));
		lines.push("");

		this.cachedWidth = width;
		this.cachedLines = lines;
		return lines;
	}

	invalidate() {
		this.cachedWidth = undefined;
		this.cachedLines = undefined;
	}
}

export default function (oxa) {
	// In-memory state (reconstructed from session on load)
	let todos = [];
	let nextId = 1;

	/**
	 * Reconstruct state from session entries.
	 * Scans tool results for this tool and applies them in order.
	 */
	const reconstructState = (ctx) => {
		todos = [];
		nextId = 1;

		for (const entry of ctx.sessionManager.getBranch()) {
			if (entry.type !== "message") continue;
			const msg = entry.message;
			if (msg.role !== "toolResult" || msg.toolName !== "todo") continue;

			const details = msg.details;
			if (details) {
				todos = details.todos;
				nextId = details.nextId;
			}
		}
	};

	// Reconstruct state on session events
	oxa.on("session_start", async (_event, ctx) => reconstructState(ctx));
	oxa.on("session_tree", async (_event, ctx) => reconstructState(ctx));

	// Register the todo tool for the LLM
	oxa.registerTool({
		name: "todo",
		label: "Todo",
		description: "Manage a todo list. Actions: list, add (text), toggle (id), clear",
		parameters: {
			type: "object",
			properties: {
				action: { type: "string", enum: ["list", "add", "toggle", "clear"] },
				text: { type: "string", description: "Todo text (for add)" },
				id: { type: "number", description: "Todo ID (for toggle)" },
			},
			required: ["action"],
		},

		async execute(_toolCallId, params, _signal, _onUpdate, _ctx) {
			switch (params.action) {
				case "list":
					return {
						content: [
							{
								type: "text",
								text: todos.length
									? todos.map((t) => `[${t.done ? "x" : " "}] #${t.id}: ${t.text}`).join("\n")
									: "No todos",
							},
						],
						details: { action: "list", todos: [...todos], nextId },
					};

				case "add": {
					if (!params.text) {
						return {
							content: [{ type: "text", text: "Error: text required for add" }],
							details: { action: "add", todos: [...todos], nextId, error: "text required" },
						};
					}
					const newTodo = { id: nextId++, text: params.text, done: false };
					todos.push(newTodo);
					return {
						content: [{ type: "text", text: `Added todo #${newTodo.id}: ${newTodo.text}` }],
						details: { action: "add", todos: [...todos], nextId },
					};
				}

				case "toggle": {
					if (params.id === undefined) {
						return {
							content: [{ type: "text", text: "Error: id required for toggle" }],
							details: { action: "toggle", todos: [...todos], nextId, error: "id required" },
						};
					}
					const todo = todos.find((t) => t.id === params.id);
					if (!todo) {
						return {
							content: [{ type: "text", text: `Todo #${params.id} not found` }],
							details: {
								action: "toggle",
								todos: [...todos],
								nextId,
								error: `#${params.id} not found`,
							},
						};
					}
					todo.done = !todo.done;
					return {
						content: [{ type: "text", text: `Todo #${todo.id} ${todo.done ? "completed" : "uncompleted"}` }],
						details: { action: "toggle", todos: [...todos], nextId },
					};
				}

				case "clear": {
					const count = todos.length;
					todos = [];
					nextId = 1;
					return {
						content: [{ type: "text", text: `Cleared ${count} todos` }],
						details: { action: "clear", todos: [], nextId: 1 },
					};
				}

				default:
					return {
						content: [{ type: "text", text: `Unknown action: ${params.action}` }],
						details: {
							action: "list",
							todos: [...todos],
							nextId,
							error: `unknown action: ${params.action}`,
						},
					};
			}
		},

		renderCall(args) {
			let text = `todo ${args.action}`;
			if (args.text) text += ` "${args.text}"`;
			if (args.id !== undefined) text += ` #${args.id}`;
			return [text];
		},

		renderResult(result) {
			const details = result.details;
			if (!details) {
				const text = result.content && result.content[0];
				return [text && text.type === "text" ? text.text : ""];
			}
			if (details.error) return [`Error: ${details.error}`];

			const todoList = details.todos;
			switch (details.action) {
				case "list":
					if (todoList.length === 0) return ["No todos"];
					return [
						`${todoList.length} todo(s):`,
						...todoList.slice(0, 5).map((t) => `${t.done ? "✓" : "○"} #${t.id} ${t.text}`),
					];
				case "add": {
					const added = todoList[todoList.length - 1];
					return [`✓ Added #${added.id} ${added.text}`];
				}
				case "toggle": {
					const text = result.content && result.content[0];
					return [`✓ ${text && text.type === "text" ? text.text : ""}`];
				}
				case "clear":
					return ["✓ Cleared all todos"];
				default:
					return [result.content && result.content[0] ? String(result.content[0].text) : ""];
			}
		},
	});

	// Register the /todos command for users
	oxa.registerCommand("todos", {
		description: "Show all todos on the current branch",
		handler: async (_args, ctx) => {
			if (ctx.mode !== "tui") {
				ctx.ui.notify("/todos requires interactive mode", "error");
				return;
			}

			await ctx.ui.custom((_tui, theme, _kb, done) => {
				return new TodoListComponent(todos, theme, () => done());
			});
		},
	});
}
