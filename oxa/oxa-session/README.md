# oxa-session

Session files (pi JSONL v3 tree), the transcript fold (W1) and tree operations. Depends on `oxa-proto`. Used by `oxa-compact`, `oxa-core`, `oxa-rpc`, `oxa-export`, `oxa-interactive`.

| Module | Task | Provides |
|---|---|---|
| `lib.rs` | architect (frozen) | `SessionStore`, `SessionRef`, `SessionHeader`, `SessionMeta` |
| `store.rs` | A3 | `FsStore`, `MemStore`, `encode_entry`/`decode_entry`, `encode_header`/`decode_header`, `session_dir_name`, `new_entry_id` |
| `transcript.rs`, `tree.rs` | A4 | `Transcript`, `RebaseLog`, `Tree` |
| `ops.rs`, `migrate.rs`, `meta.rs` | B14 | fork/clone, names, labels, pi import, picker search/sort |

Entry type names on disk (`EntryBody` variant -> pi `type`; verified against pi `docs/session-format.md` @47e9d74 by the W0 PARITY pass): `Prefix` -> `message` with `role: "system"` + `sections` (oxa adds `toolsJson`, `sha256`); `Msg` -> `message` (roles `user` / `assistant` / `toolResult`; assistant entries add the exact `wire` object, W2); `BashExecution` -> `message` with `role: "bashExecution"`; `Usage` -> `usage`; `Compaction` -> `compaction` (pi's `systemMessage` checkpoint field is not written: the prefix entry is the checkpoint); `ContextEdit` -> `context_edit`; `BranchSummary` -> `branch_summary`; `Custom` -> `custom`; `CustomMessage` -> `custom_message`; `Label` -> `label`; `SessionInfo` -> `session_info`; `ModelChange` -> `model_change`; `ThinkingLevelChange` -> `thinking_level_change`; oxa-only: `Shown` -> `shown`, `ModeChange` -> `mode_change`. A3 checks each name and field against `session-format.md` while implementing and reports a mismatch to the architect instead of changing a name silently.

Contracts
- W1: `Transcript::project` is a pure fold; non-rebasing entries only append; `rebase` logs its cause.
- W2: assistant entries round-trip the streamed strings byte for byte.
- W3: the prefix is an entry and is replayed, never rebuilt, on resume.
- Files: 0600 in 0700, `O_APPEND | O_NOFOLLOW`, symlink refused, torn last line dropped.
- pi tooling can read oxa files: same envelope, same `message` shape; oxa-only fields are additive.
