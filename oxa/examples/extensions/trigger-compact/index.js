// ported from pi examples/extensions/trigger-compact.ts (earendil-works/pi @47e9d74, MIT; see NOTICE)
//
// Trigger compact: compact the session when the context crosses 100k tokens (checked after each
// turn), and offer a /trigger-compact command.
//
// Changes from pi (mechanical): pi -> oxa; the types are removed; `ctx.compact` takes
// `customInstructions` as the glue reads it. GAP: the glue does not call `onComplete` / `onError`
// (the compaction runs in the session's run, and its result is not reported back to the handler),
// so the port only announces the start; the completion and failure notices are not available yet.

const COMPACT_THRESHOLD_TOKENS = 100_000;

export default function (oxa) {
  let previousTokens;

  const triggerCompaction = (ctx, customInstructions) => {
    if (ctx.hasUI) {
      ctx.ui.notify("Compaction started", "info");
    }
    ctx.compact({ customInstructions });
  };

  oxa.on("turn_end", (_event, ctx) => {
    const usage = ctx.getContextUsage();
    const currentTokens = usage && typeof usage.tokens === "number" ? usage.tokens : null;
    if (currentTokens === null) {
      return;
    }

    // Compact on the turn that crosses the threshold, not on every turn above it.
    const crossedThreshold =
      previousTokens !== undefined && previousTokens !== null && previousTokens <= COMPACT_THRESHOLD_TOKENS;
    previousTokens = currentTokens;
    if (!crossedThreshold || currentTokens <= COMPACT_THRESHOLD_TOKENS) {
      return;
    }
    triggerCompaction(ctx);
  });

  oxa.registerCommand("trigger-compact", {
    description: "Trigger compaction immediately",
    handler: (args, ctx) => {
      const instructions = args.trim() || undefined;
      triggerCompaction(ctx, instructions);
    },
  });
}
