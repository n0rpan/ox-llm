# Sandbox source installation and rollback checkpoint

**Implemented: create-only installation of the staged source payload into an explicit sandbox HOME. Not implemented: live-user installation or complete setup parity.** The entry refuses the current HOME and its live hidden configuration trees. Nothing here starts services, registers a panel, merges providers, downloads/copies weights or enables qualification.

The transaction reuses `bootstrap.stage` and its exact file/directory inventory. It verifies the supplied stage-manifest SHA, target-HOME binding and every staged byte/mode before mutation. This checkpoint accepts at most 8 MiB of source resources: runtime modules, disabled MoE/Q8 configuration, unit files, panel assets and notices. Since v0.2.0 it stages no OpenCode config. Native binaries and model placement remain a later guarded phase.

## Try only in a fresh Work fixture

The [archived sandbox procedure](https://github.com/oxstack/ox-llm/blob/e64b985a2ff6abf51d406471382ed95a18fd161c/oxc/docs/BOOTSTRAP-INSTALL.md#try-only-in-a-fresh-work-fixture) preserves the legacy staging, apply and rollback commands. Its scope is an explicit sandbox HOME. Use the [native install and upgrade guide](https://github.com/oxstack/ox-llm/blob/main/docs/NATIVE-INSTALL.md) for v0.2.x product installation.

The target may already contain unrelated provider/layout/sentinel files, which are not read or replaced. Any existing managed file causes refusal before target writes, even if its bytes happen to match. The sole reuse exception is a valid owner-only canonical lock: its inode and bytes are preserved and it is held through the transaction. New directories are private; existing safe directory modes remain unchanged.

Each managed file is written/fsynced to an owned temporary inode, its identity is journaled, then published with a create-only hard link. A concurrent destination or temporary-name collision is preserved. Descriptor-relative directory traversal rejects symlinks; no existing configuration is replaced and no provider/layout merge occurs.

The archived rollback procedure takes the journal SHA from the **saved installation receipt**.

Rollback acquires the same canonical inode and removes only recorded files whose inode, owner, mode, mtime, size and SHA still match. Edited/replaced files, changed directory pointers, unknown user files and unrelated configuration are preserved. Only empty directories created by this transaction are removed. The canonical lock is deliberately retained once created, including after failure: deleting/replacing that anchor would undermine shared-inode exclusion. A rollback receipt identifies preserved owned paths. A changed journal or busy canonical lock refuses rollback without deleting files.

Caught failures automatically run the same conservative rollback and retain the private journal. Write-ahead publication identities cover published files, but this checkpoint does not claim completed power-loss/SIGKILL recovery; a crash before a temporary inode is recorded can leave an unknown temporary file for review. No recursive cleanup of unknown files is attempted.

## Tests and remaining finish boundary

Eleven focused tests passed using owned temporary fixtures beside the checkout under Work. They cover the actual staged source payload, existing canonical hardlink/bytes/provider/layout/sentinel preservation, all 15 mutation fault points of a tiny fixture, modified/unknown file preservation, busy-lock/hash refusal, enabled-profile rejection, symlink redirection, final-name races and temporary-name collisions. The fault loop mocks fsync to keep I/O small; the successful full source-payload install/rollback exercises normal filesystem writes/fsync. No live HOME, service, panel, native engine, model or user data was changed.

The module returns `qualified:false`, `services_started:false`, `panel_layout_changed:false`. Unit files still contain `%h`; do not launch a sandbox's units with the real user's manager and assume `%h` means the sandbox. Actual host installation, native/model placement, provider merge/launch wiring, right-side widget registration, restart/login behavior and identity-bound model/agent/image/lifecycle acceptance remain open. Fresh Flash proofs also cannot reuse historical private-manifest labels. [Complete parity finish gate](BOOTSTRAP-PARITY.md).
