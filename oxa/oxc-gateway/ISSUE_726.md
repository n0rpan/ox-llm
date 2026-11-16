# ChatGPT expired-token recovery — partial Rust fix (T3)

Issue #726 was checked against main at `3d9ab17f`. The comparison and legacy
root-cause analysis were posted before implementation:
https://github.com/oxstack/ox-llm/issues/726#issuecomment-6917568372

Codex was read as a behavioural reference at
`openai/codex@662a634e88fcc809d77da393d75572c3d814cb27`; no source was copied.
It checks JWT expiry at auth acquisition (five-minute margin), falls back to
an eight-day refresh age when JWT expiry is unavailable, and performs bounded
401 recovery by reloading same-account credentials and then refreshing. It
persists rotated tokens before updating cached auth, with commit locking/CAS,
and distinguishes permanent from transient refresh failures.

## This change

`openai::chatgpt_tokens::credentials_after_rejection` accepts the record used
by a rejected request. It forces refresh even when the stored expiry appears
fresh. It uses the existing persistent flock, rereads under the lock, reuses
another refresher's fresh replacement, and refuses an account change on either
reload or refresh. The existing checked, private, fsynced atomic replacement
completes before the updated credentials are returned. There is no fallback to
the rejected token after a failed refresh, and errors remain typed/redacted.

The existing provider status card gains an explicit conversion from credential
errors to `SignedOut`. This does not delete credentials on transient failures.
No new prompts, restrictions, dependencies, default features or background jobs
are introduced.

## Deliberately unresolved

Main now includes the ChatGPT HTTP client from PR #680. This merge preserves
its nonblocking `try_credentials` admission alongside the rejected-generation
recovery API. The client still needs to call the recovery entry only for a 401
`token_expired`, retry the request at most once, and publish credential errors
through the status conversion. This patch does not claim to wire that client,
repair the live route, or test an HTTP retry. Loopback tests for the actual
OAuth exchange/401 retry belong with that integration, not a competing
transport implemented in this patch.

The frozen Python route is unchanged by owner instruction. It already acquires
credentials per request, refreshes with a 120-second margin, and raises refresh
errors. Its expiry calculation prefers `expires_in` over JWT `exp`, and its
HTTP error path does not refresh on 401. A misleading later stored expiry can
therefore prevent recovery; the exact observed timing cannot be established
without the original response, and no real credentials were inspected.

## Targeted verification

The new rejection regression first failed with the old freshness-only behaviour:
the original refresh credential was returned instead of the rotated one. The
new status regression initially failed to compile without its conversion API.

Only the token-file integration target and the new status test are run locally.
They exercise per-request expiry checks, forced recovery of a supposedly fresh
record, rotation persisted before return, same-account race coalescing across
two OS processes, account-change refusal and typed refresh failure without
credential loss. Existing private-file, atomicity and failure tests in that
target also run. All fixtures are temporary and synthetic; no provider network,
real sign-in or credential-file discovery is used. Strict default and opt-in
Clippy plus formatting are required. CI has not run (no push requested).

Security-sensitive credential changes require T3 review before merge.
