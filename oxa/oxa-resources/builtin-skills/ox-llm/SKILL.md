---
name: ox-llm
description: Operate the local ox-llm platform that runs this agent - status, switching models (when to use Flash), logs, the daily health digest, updates and troubleshooting with the oxc CLI. Use when the user asks about the local models, the controller, the engine, memory, speed, or why something on the platform is slow or failing.
---

# ox-llm platform operations

You run on ox-llm: the oxe engine serves the model on this machine and the oxc controller in
front of it exposes one OpenAI-compatible endpoint on loopback (`http://127.0.0.1:8092`). Nothing
leaves the machine. One model is loaded at a time.

## Names (v0.2.0 rename)

v0.2.0 renamed the controller CLI from `local-ai` to `oxc`, its config from `~/.config/local-ai`
to `~/.config/oxc`, its state from `~/.local/state/local-ai` to `~/.local/state/oxc` and the
systemd user units from `local-ai-*` to `oxc-*`. If `command -v oxc` finds nothing, the install
predates v0.2.0: use `local-ai` with the same subcommands and the old paths and unit names.

## Status (read-only, never loads a model)

- `oxc status` - one line: chat model state, image state, free memory. `oxc status --json` for
  every field (selected model, limits, queue).
- `oxc storage` - stored model files and caches with sizes.
- `oxc ledger [--limit N] --json` - the last requests (counts and timings, never prompt text).
- `oxc cache status` - the conversation cache: used, budget, saved conversations.

## Models

- `oxc moe` - Qwen3.6-35B-A3B (128K context).
- `oxc ornith` - Ornith-1.5-35B-A3B, the everyday default (128K context), when it is set up.
- `oxc flash` - Qwen3.8-Flash-Next, the large "think hard" model: experts are streamed from the
  SSD, so it is slower per token and its context is small (16K). Suggest it for a hard,
  self-contained question; stay on the default model for long agentic sessions.
- `oxc flash-cache --pref auto|20|18|16|14` - the Flash expert cache tier in GiB (bigger is
  faster but uses more memory); only when nothing is running.
- `oxc load` / `oxc unload` - load the selected model now / free its memory.
- `oxc mode --mode fast|deep|max` - context mode for the client's next session.

Switching the model reloads the engine (tens of seconds) and the next request prefills the whole
conversation again. Inside oxa, prefer `/model`: it checks that the conversation fits the target
and offers to compact first. Always tell the user before you switch; never switch on your own.

## Running oxa without prompts (YOLO)

`oxa --yolo` (or `/yolo` inside oxa, or Ctrl+Alt+O) auto-approves every permission question, so
a run does not stop to ask. The sandbox and the deny-list of keys and tokens stay on, and writes
to a project's `.oxa` still ask. Suggest the alias `alias yoxa='oxa --yolo'` when the user wants
it quick to start. Never switch YOLO on yourself; it is the user's choice.

## Health and logs

- `oxc digest` - today's health numbers (counts, rates, timings; never prompt text).
  `--day yesterday`, `--status` (the panel line), `--soak` (green days), `--json`.
- `journalctl --user -u oxc-chat -n 100 --no-pager` - the controller and engine log.
- `systemctl --user status oxc-chat` - is the service running.

## Updates

- `oxc update --check` - is a staged release ready (never downloads anything).
- `oxc update [STAGED-RELEASE-DIR] [--no-chat]` - apply a release already staged on this machine.
- `oxc rollback [--check]` - go back to the previous installed release.
- `oxc uninstall` is a dry run; it acts only with `--apply`. Never run `--apply` or
  `--remove-models` unless the user asked for exactly that.

## Troubleshooting

1. `oxc status --json`: is the controller up, which model, is something queued?
2. Offline: `systemctl --user status oxc-chat`, then the journal above.
3. Slow: check `oxc status --json` for the queue, and `free -g` for memory pressure; Flash is
   slow by design at small cache tiers.
4. `oxc cache clear --session ID` drops one saved conversation state; `oxc cache clear --all`
   also clears stored prompts (ask first).
5. A reproducible platform bug: offer the report-issue skill.

## Web search

oxa's `web_search` tool searches the web through Brave Search. It is offered only when a Brave
Search API key is configured: the environment variable `BRAVE_SEARCH_API_KEY`, or the raw key in
`~/.oxa/agent/brave-search.key` (the file must be mode 0600: `chmod 600`; a group- or
world-readable file, a symlink or an empty file is refused). A new key takes effect at the next
session or `/reload`; `/web-search` shows the status. Only the query is sent, and only when the
tool is called; that query leaves the machine. Brave includes up to $20 of free calls per
month as prepaid credits; usage beyond that is billed to the user's prepaid credits (Brave's
pricing and retention terms; check brave.com/search/api). Turn it off with `--no-web-search`,
`OXA_WEB_SEARCH=0` or `"oxa": {"webSearch": {"enabled": false}}`; offline mode (`OXA_OFFLINE=1`,
`--offline`) removes it. "Brave plan lacks LLM Context; using web search" means the key's plan
has no LLM Context endpoint and the plain web search endpoint is used instead. Never ask the user
to paste the key into the chat.

## Configuration

- Controller: `~/.config/oxc/` (`llm/` holds the model configuration). Do not edit it while a
  request is running.
- oxa: `~/.oxa/agent/settings.json`, `models.json` (endpoint), `identity.md` (generated by
  `oxa identity --refresh` at install and update; do not edit by hand).
