// ported from pi examples/extensions/input-transform.ts (earendil-works/pi @47e9d74, MIT; see NOTICE)
//
// Input transform: the `input` event intercepts what the user types.
//   ?quick <question>  -> "Respond briefly in 1-2 sentences: <question>"
//   ping               -> "pong" (instant, no model call)
//   time               -> the current time (instant, no model call)
//
// Changes from pi (mechanical): pi -> oxa; the types are removed; `toLocaleString()` is
// `toISOString()` so the output does not depend on the host locale.

export default function (oxa) {
  oxa.on("input", (event, ctx) => {
    // Extension-injected messages are not transformed again.
    if (event.source === "extension") {
      return { action: "continue" };
    }

    // ?quick prefix: a brief answer.
    if (event.text.startsWith("?quick ")) {
      const query = event.text.slice(7).trim();
      if (!query) {
        ctx.ui.notify("Usage: ?quick <question>", "warning");
        return { action: "handled" };
      }
      return { action: "transform", text: `Respond briefly in 1-2 sentences: ${query}` };
    }

    // Instant answers, handled by the extension without a model call.
    if (event.text.toLowerCase() === "ping") {
      ctx.ui.notify("pong", "info");
      return { action: "handled" };
    }
    if (event.text.toLowerCase() === "time") {
      ctx.ui.notify(new Date().toISOString(), "info");
      return { action: "handled" };
    }

    return { action: "continue" };
  });
}
