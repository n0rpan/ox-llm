Plan item 6 (planning pass 2026-11-16)

## What and why — T3

Add `openai/chatgpt_tokens.rs` to read the Python provider's existing record
(`access_token`, `refresh_token`, `account_id`, numeric Unix `expires_at`).
No Python changes, transport, sign-in, real credentials, or runtime wiring.
The containing `openai` module is already opt-in via `upstream-openai`; this
new submodule additionally requires the existing `secrets` feature. No Cargo
manifest or dependency changes.

Reads refuse symbolic/hard links, nonregular files, other owners, group/other
permissions, and records over 64 KiB. Errors retain neither paths nor input
text; token records have redacted Debug output. Callers must supply a trusted
directory and trusted ancestors; the immediate parent must be owned by the
caller and not writable by group/others. Operations are relative to an opened
directory descriptor.

Freshness requires more than 120 seconds remaining. A stale record takes an
exclusive flock on the persistent sibling `chatgpt-auth.lock` (the Python
provider's lock name), re-reads, and only then invokes an injected refresher.
Independent file opens serialize both threads and processes. Refresher errors
are discarded, not logged or returned. The new record must be fresh and fit
within 64 KiB; it is written to an exclusive 0600 temporary file, fsynced, and
atomically renamed, followed by a directory fsync. Do not delete the lock file.
A directory-fsync error after rename reports failure even though replacement
may already have committed; a subsequent call re-reads the file.

## Verification

- Red first: the new targeted test could not compile before the module existed
  (`unresolved import ... openai::chatgpt_tokens`).
- `cd oxa && cargo fmt` and `cargo fmt --check`.
- `cd oxa && cargo clippy -p oxc-gateway -- -D warnings`.
- `cd oxa && cargo clippy -p oxc-gateway --features upstream-openai,secrets --all-targets -- -D warnings`.
- `cd oxa && cargo test -p oxc-gateway --features upstream-openai,secrets --test chatgpt_tokens`:
  targeted tests only; temporary files, two simultaneous threads and two child
  processes, one refresh, atomic inode replacement and 0600 mode, link/permission/
  size/nonregular-file refusal, exact freshness boundary, error and Debug
  redaction, and preservation of the original on refresh failure.
- CI gate has not run: this branch has not been pushed, as requested.

## Binary-size effect

Default binaries: no added compiled code (the existing `upstream-openai` gate
excludes the module, and no executable is wired to it). Opt-in consumers with
`upstream-openai,secrets` may grow when they call these APIs; that linked-size
delta has not been measured. No dependencies or default features are added.

## Required before merge

**Needs an orchestrator or Fable read before merge: this is a credential path.**
That read has not been performed. Keep the PR draft pending that review and the
project CI gate. The frozen Python controller was read only to establish the
record and lock-file format.

Written by oxa, model `chatgpt/gpt-6.1-sol`.
