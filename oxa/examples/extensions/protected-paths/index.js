// ported from pi examples/extensions/protected-paths.ts (earendil-works/pi @47e9d74, MIT; see NOTICE)
//
// Protected paths: block `write` and `edit` calls whose path touches a protected location.
//
// Changes from pi (mechanical): pi -> oxa; the types are removed. A call without a string path is
// tested as the empty string, so a malformed argument does not throw inside the handler (an error
// there would let the call through).

const protectedPaths = [".env", ".git/", "node_modules/"];

export default function (oxa) {
  oxa.on("tool_call", (event, ctx) => {
    if (event.toolName !== "write" && event.toolName !== "edit") {
      return undefined;
    }

    const input = event.input && typeof event.input === "object" ? event.input : {};
    const path = typeof input.path === "string" ? input.path : "";
    const isProtected = protectedPaths.some((p) => path.includes(p));

    if (isProtected) {
      if (ctx.hasUI) {
        ctx.ui.notify(`Blocked write to protected path: ${path}`, "warning");
      }
      return { block: true, reason: `Path "${path}" is protected` };
    }

    return undefined;
  });
}
