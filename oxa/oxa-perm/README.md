# oxa-perm

Permission modes (plan / workspace / ask), the access policy table and project trust. Depends on `oxa-proto`. Used by `oxa-core`, `oxa-cli`, `oxa-interactive`.

| Module | Task | Provides |
|---|---|---|
| `lib.rs` | architect (frozen) | the policy table (crate docs) |
| `policy.rs`, `paths.rs` | A10 | `ModePolicy`, `deny_reasons`, `normalize`, `is_inside`, `is_read_only_command` |
| `trust.rs` | B9 | `TrustStore`, `resolve_trust`, `gated_resources` |

The table in `src/lib.rs` is the acceptance test of A10: one test row per cell. Trust gates what loads; modes gate what tools do.
