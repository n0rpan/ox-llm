---
name: oxa-extend
description: How to extend oxa itself - write a skill (SKILL.md), a prompt template, a JavaScript extension (tools, commands, shortcuts, event handlers) or an exec hook, where each file goes and how to keep the prompt cache warm. Use when the user wants to customise or automate oxa.
---

# Extending oxa

oxa follows pi's extension model (MIT, Mario Zechner); most pi documentation applies with `pi`
renamed to `oxa` and `~/.pi/agent` to `~/.oxa/agent`. Project files are read only in a trusted
project (`/trust`).

## Skills

A directory with a `SKILL.md`:

```
---
name: my-skill            # lowercase a-z, 0-9 and hyphens, at most 64 characters
description: What it does and when to use it (at most 1024 characters).
---
Instructions, loaded only when the task matches the description.
```

Where (first wins on a name clash): `--skill PATH`, `~/.oxa/agent/skills/`, `~/.agents/skills/`,
project `.oxa/skills/`, project `.agents/skills/` (up to the repository root), the `skills`
setting, packages, oxa's built-in skills, then the system directory
`/usr/share/omarchy/default/agents/skills`. A skill with the name of a built-in replaces it.
`disable-model-invocation: true` hides a skill from the model; `/skill:NAME` still runs it.

## Prompt templates

`~/.oxa/agent/prompts/NAME.md` or project `.oxa/prompts/NAME.md` becomes `/NAME`. Frontmatter
`description` and `argument-hint` are optional; the body takes `$1`, `$@` and `${1:-default}`.

## Extensions (JavaScript, embedded QuickJS)

`~/.oxa/agent/extensions/NAME.js` (or a directory with `index.js`), project
`.oxa/extensions/`, or `oxa -e PATH`. An ES module whose default export receives the API:

```js
export default function (oxa) {
  oxa.registerTool({
    name: "word_count",
    description: "Count the words of a text",
    parameters: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
    class: "read",
    execute(id, params) {
      return { content: String(params.text.split(/\s+/).filter(Boolean).length) };
    },
  });
  oxa.registerCommand("hello", { description: "Say hello", handler: (args, ctx) => ctx.ui.notify("hello " + args) });
  oxa.on("tool_call", (event, ctx) => { /* return { block: true, reason } to refuse */ });
}
```

No Node or npm imports: use `oxa.exec`, `ctx.fs` and `ctx.fetch` (they go through the session's
permission mode and sandbox). `.ts` files are not loaded. The full API is `oxa-ext/oxa.d.ts` in
the oxa sources. `registerProvider` and virtual models are not supported.

## Exec hooks

The `hooks` setting runs a program at a hook point with a JSON event on stdin; its JSON reply on
stdout can block or change the outcome. Hooks run in the bubblewrap sandbox with a timeout.

## Keep the cache warm

The system prompt, the tool list and the skills index are a frozen prefix: the engine reuses it
across turns only when its bytes do not change. Tools, prompt sections (`addPromptSection`) and
skills take effect at the next epoch (`/reload` or a new session), never mid-turn. Context
changes from `context` handlers must keep the history a byte prefix (append only) unless the
extension declares `export const manifest = { rebase: true }`, which costs a full re-prefill.
