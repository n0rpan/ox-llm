// ported from pi packages/coding-agent/examples/extensions/status-line.ts
//
// Changes from pi (mechanical): pi.* -> oxa.*; the types are removed. oxa's `ctx.ui.theme`
// returns the text unchanged (the terminal's own colours).
//
// Status Line Extension
//
// Demonstrates ctx.ui.setStatus() for displaying persistent status text in the footer.
// Shows turn progress with themed colors.

export default function (oxa) {
	let turnCount = 0;

	oxa.on("session_start", async (_event, ctx) => {
		const theme = ctx.ui.theme;
		ctx.ui.setStatus("status-demo", theme.fg("dim", "Ready"));
	});

	oxa.on("turn_start", async (_event, ctx) => {
		turnCount++;
		const theme = ctx.ui.theme;
		const spinner = theme.fg("accent", "●");
		const text = theme.fg("dim", ` Turn ${turnCount}...`);
		ctx.ui.setStatus("status-demo", spinner + text);
	});

	oxa.on("turn_end", async (_event, ctx) => {
		const theme = ctx.ui.theme;
		const check = theme.fg("success", "✓");
		const text = theme.fg("dim", ` Turn ${turnCount} complete`);
		ctx.ui.setStatus("status-demo", check + text);
	});
}
