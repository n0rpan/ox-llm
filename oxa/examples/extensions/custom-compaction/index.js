// ported from pi packages/coding-agent/examples/extensions/custom-compaction.ts
//
// Changes from pi, forced by oxa's API (see examples/extensions/README.md):
// - O-144: the port sends no model request of its own. pi summarises in the hook with a second
//   model (Gemini Flash). Here the summary is the session's own W5 summary request, carrying this
//   port's instructions: `/compact-all` asks the core to compact with FULL_SUMMARY_INSTRUCTIONS.
//   The hook `session_before_compact` only observes it and answers Continue, so the core's
//   summary path runs and the summary lands as the W5 head.
// - the cut point is the core's (keepRecentTokens); a hook cannot choose "summarise everything".

const FULL_SUMMARY_INSTRUCTIONS = `You are a conversation summarizer. Create a comprehensive summary of this conversation that captures:

1. The main goals and objectives discussed
2. Key decisions made and their rationale
3. Important code changes, file modifications, or technical details
4. Current state of any ongoing work
5. Any blockers, issues, or open questions
6. Next steps that were planned or suggested

Be thorough but concise. The summary will replace the older conversation history, so include all information needed to continue the work effectively.

Format the summary as structured markdown with clear sections.`;

export default function (oxa) {
	oxa.on("session_before_compact", (event, ctx) => {
		if (event.instructions !== FULL_SUMMARY_INSTRUCTIONS) return undefined;
		ctx.ui.notify(
			`Custom compaction: summarizing the conversation (${event.tokensBefore} tokens) with the session model`,
			"info",
		);
		return undefined;
	});

	oxa.registerCommand("compact-all", {
		description: "Compact with a full summary of the conversation (custom-compaction port)",
		handler: async (_args, ctx) => {
			await ctx.compact({ customInstructions: FULL_SUMMARY_INSTRUCTIONS });
		},
	});
}
