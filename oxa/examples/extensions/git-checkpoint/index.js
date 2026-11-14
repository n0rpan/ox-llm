// ported from pi examples/extensions/git-checkpoint.ts (earendil-works/pi @47e9d74, MIT; see NOTICE)
//
// Git Checkpoint Extension
//
// Creates git stash checkpoints at each turn so /fork can restore code state.
// When forking, offers to restore code to that point in history.
//
// Changes from pi (mechanical): pi -> oxa; the types are removed.

export default function (oxa) {
  const checkpoints = new Map();
  let currentEntryId;

  // Track the current entry ID when user messages are saved
  oxa.on("tool_result", async (_event, ctx) => {
    const leaf = ctx.sessionManager.getLeafEntry();
    if (leaf) currentEntryId = leaf.id;
  });

  oxa.on("turn_start", async () => {
    // Create a git stash entry before LLM makes changes
    const { stdout } = await oxa.exec("git", ["stash", "create"]);
    const ref = stdout.trim();
    if (ref && currentEntryId) {
      checkpoints.set(currentEntryId, ref);
    }
  });

  oxa.on("session_before_fork", async (event, ctx) => {
    const ref = checkpoints.get(event.entryId);
    if (!ref) return;

    if (!ctx.hasUI) {
      // In non-interactive mode, don't restore automatically
      return;
    }

    const choice = await ctx.ui.select("Restore code state?", [
      "Yes, restore code to that point",
      "No, keep current code",
    ]);

    if (choice?.startsWith("Yes")) {
      await oxa.exec("git", ["stash", "apply", ref]);
      ctx.ui.notify("Code restored to checkpoint", "info");
    }
  });

  oxa.on("agent_settled", async () => {
    // Clear checkpoints after the full agent run completes
    checkpoints.clear();
  });
}
