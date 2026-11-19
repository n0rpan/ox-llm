# oxa-config

Settings, keybindings, model endpoints, environment and directories. Depends on `oxa-proto` only; everything above the leaf layer depends on it.

| Module | Task | Provides |
|---|---|---|
| `lib.rs` | architect (frozen) | `Dirs`, `Diagnostic` |
| `settings.rs`, `schema.rs` | A11 | `Settings`, `schema`, `validate`, `merge_settings`, `apply_list_patch` |
| `descriptor.rs`, `descriptor/*.rs` | #1489 | The one settings table: one row per key, with kind and default (`.key(kind, default)`); `schema()` is derived from it |
| `keybindings.rs` | B5 | `Keymap`, `ActionDef` |
| `profile.rs` | T5 (wave H) | `ThinkingPolicy` and the tool/context defaults |
| `models.rs`, `env.rs`, `dirs.rs` | B6 | `ModelsConfig`, `resolve_model`, `ENV_TABLE`, `Env`, `dirs_from_env` |

Contracts
- Test isolation (P56): only `Env::from_process` reads the process environment, once, in `main`. All other code takes `&Dirs` / `&Env`. Tests use `Dirs::under(tmp)`.
- A new setting is one descriptor row with `.key(kind, default)` in `src/descriptor/*.rs` (`.hidden()` when it has no menu entry; `descriptor/hidden.rs`), plus its line in `tests/fixtures/settings_table_pin.txt` and a row in `oxa/docs/pages/settings.md`. `schema.rs` holds no key list.
- Configuration never fails: unknown keys, wrong types and unreadable files become `Diagnostic`s.
- Keys are read by pi's dotted names (`settings.u32("compaction.reserveTokens", 16384)`); oxa's own keys are `oxa.*`.
- W3: values are read once per prefix epoch; nothing watches files.
- Defaults that differ from pi [P]: `compaction.keepRecentTokens` 4000; all seven tools on; `app.mode.cycle` = `alt+m`.
