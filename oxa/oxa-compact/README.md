# oxa-compact

Compaction and branch-summary policy (W5) as pure functions. Depends on `oxa-proto`, `oxa-session`. Used by `oxa-core` (C3 orchestration, C4 navigation).

| Module | Task | Provides |
|---|---|---|
| `lib.rs` | architect (frozen) | re-exports |
| `prompt.rs` | architect (frozen; all summary text) | instruction and wrapper texts |
| `policy.rs`, `summary.rs`, `files.rs` | B1 | `CompactPolicy`, `decide`, `policy_from`, `summary_request`, `first_kept_index`, `head_after`, `file_lists` |
| `branch.rs` | B2 | `branch_entries`, `budget_newest_first`, `branch_summary_request` |

Contracts: after-reply compaction; the summary request is the whole wire + one instruction; the rebased head is `[user:<summary>, tail]`; a prompt is never folded; file lists accumulate. Differences from pi [P]: `keepRecentTokens` 4000; no split-turn double summary (the mid-run path replaces it).
