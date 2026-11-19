# Native v0.2.1 install and upgrade

v0.2.1 is the Rust-only release target tracked by [#763](https://github.com/oxstack/ox-llm/issues/763).
Its model runtime is `oxa` plus `oxe-server`, with the approved native
`local-ai-memlat-boot` utility and inert MEMLAT templates: no Python runtime, controller
daemon, `oxc` command wrapper or service on port 8092. Release qualification and
publication are separate owner steps; this guide does not declare an unpublished
candidate ready. v0.2.0 remains the legacy install.
Python may still be a source-package build/test tool; it is not an installed
runtime dependency.

## Install verified native artifacts

Use the published v0.2.1 artifact for the machine's architecture and its checksum
manifest from the same release. Inspect the release's source identity and
provenance, then verify the downloaded files:

```sh
sha256sum -c SHA256SUMS
```

An unsigned manifest beside a download detects corruption but does not establish
who published it. Follow the release provenance as well as checking the sums.
Do not use v0.2.0's source/bootstrap installer or a development parser binary as
the native v0.2.1 installer.

For an Arch package, use the verified v0.2.1 package with `sudo pacman -U`.
The combined tarball has the same `usr/` layout as the packages, including
`usr/bin/oxa` and `usr/bin/oxe-server`. First inspect its file list and extract
into a fresh, versioned user-owned directory. Run the native binaries directly
from that directory for the first check; do not overlay the legacy installation
or copy the release's configuration over existing user settings. Keep the
matching `oxa` and `oxe-server` together. A local load rejects an engine whose
build version does not match the agent.

Check the selected `oxa --version` and `oxa --help` before starting a model.
`oxe-server` has no `--version` option: native local loading checks the owned
engine's authenticated `/props` response for the matching `build_info` identity.
The release's model-free stub smoke exercises that identity separately; a file
name alone does not prove a matching build. Use an explicit binary path if the
shell still resolves a legacy install. No Python/controller command, shell
wrapper or new daemon is needed to launch native `oxa`.

## Upgrade from v0.2.0

1. Finish active work and stop the legacy runtime through the mechanism that
   started it; disable a legacy controller service if you previously enabled one.
   Installation does not stop processes or modify user service configuration.
   Do not run its controller alongside the native install as a
   migration step. Keep the old binaries and user data available for rollback;
   a binary replacement is not a data cleanup or uninstall operation.
2. Install the verified native v0.2.1 binaries as above, keeping the same user
   account and HOME for normal use. Check shell aliases, PATH and any explicit
   binary selection so an old `oxc code` launcher or Python wrapper does not
   start the legacy stack. Launch `oxa` directly.
3. Reuse the existing credentials **in place**:

   | Data | Existing path under the same HOME | Upgrade behavior |
   |---|---|---|
   | Provider keys | `~/.config/oxc/llm/cloud-secrets.json` | Native gateway reads the existing provider entries; no key export/import or new key is needed solely for the upgrade. |
   | ChatGPT token record | `~/.config/oxc/llm/chatgpt-auth.json` | Native subscription code uses the compatible record; no fresh sign-in is needed solely for the upgrade. |
   | ChatGPT refresh lock | `~/.config/oxc/llm/chatgpt-auth.lock` | Preserve the existing lock; do not unlink it to force a refresh. |

   Credential files remain private, owner-owned regular files (mode `0600`) in
   trusted directories. Keep them outside the checkout. Do not paste their
   contents into settings, a command line, an issue or a migration report. Do
   not copy another account's token into this account. A missing, expired or
   revoked credential still requires the provider's normal owner recovery; the
   upgrade cannot make an invalid credential valid.
4. Preserve user settings and review the native model configuration. Reusing a
   key does not translate legacy controller configuration, model aliases, caps
   or endpoint overrides automatically. Native cloud selection is the user
   setting `oxa.engine.inProcessCloud`; native local selection uses
   `oxa.engine.inProcessLocal`. Merge the following explicit selectors into
   `settings.json` in the user agent directory (normally
   `~/.oxa/agent/settings.json`, changed by `OXA_AGENT_DIR`), preserving all
   existing routes, budgets and unrelated settings:

   ```json
   {
     "oxa": {
       "engine": {
         "inProcessCloud": true,
         "inProcessLocal": true
       }
     }
   }
   ```

   Installation writes no user settings. Do not rely on absent selectors:
   older binaries default them to `false`, and an existing explicit user
   `false` remains respected even when a package supplies native defaults.
   Project settings cannot choose credential or engine executable paths. The native local model
   configuration is documented in [native local configuration](https://github.com/oxstack/ox-llm/blob/main/oxa/oxc-local/CONFIGURATION.md).
   Use these explicit native selectors and model routes rather than a
   legacy controller URL. Do not point the new agent at port 8092 as an upgrade
   requirement. Keep explicit cloud budgets and provider subscription limits.
5. Existing model weights need not be downloaded again: configure their actual
   absolute paths for native local loading. Missing weights are reported rather
   than downloaded. Local inference still requires a supported machine, memory
   policy and matching engine; replacing the binaries does not qualify hardware.
6. Start a new session for the initial native check. Sessions and caches **may
   start cold**; there is no promise to migrate controller sessions, loaded
   engine state, KV/prefix caches or in-flight requests. Keep old session files
   rather than deleting them. `oxa`'s usual agent/session directory overrides
   still matter; changing HOME or `OXA_AGENT_DIR` changes the visible state.
   A cold cache or absent old session is not a reason to reset credentials.

The runtime boundary changes; the credential paths do not. Shared credential
storage does not imply shared usage ledgers or continuous cache/session state.
Preserve existing accounting records and verify the native ledger and budget
settings before substantial use. Native gateway usage is normally stored under
`~/.oxa/agent/cloud-ledger/` (the agent-directory override changes that location).

## Recovery and scope

If the native check fails, keep the error and version information without
credential contents. Check binary selection, native settings, model paths and
file ownership before attempting owner sign-in. Native ChatGPT sign-in, when
actually needed, is `oxa login chatgpt --experimental` in the owner's terminal;
it is not an automatic upgrade step. Logging out or replacing a token changes
the shared credential state and is not a rollback mechanism.

To return to v0.2.0, stop the native runtime and select the retained legacy
installation explicitly. Do not run both to repair a shared token, restore a
stale token backup over a refreshed record or delete user data to switch versions.

Status as of v0.2.1: LAN sharing is not in the release binaries; the omarchy panel
source is in the repository but is not packaged; Studio/images, the lead layer and a
controller/HTTP daemon are outside v0.2.1. See [release qualification and upgrade boundary](https://github.com/oxstack/ox-llm/blob/main/docs/RELEASE.md#v021-rust-only-upgrade-boundary).

The approved native `local-ai-memlat-boot` utility is retained. The four MEMLAT
hook/script templates are `05-local-ai-memlat-fallback.hook`,
`20-local-ai-memlat-fallback`, `85-local-ai-memlat-rebuild` and
`99-z-local-ai-memlat-rebuild.hook`. They ship as inert data at
`usr/share/ox-llm/memlat/` in the payload: `/usr/share/ox-llm/memlat/` for a system
package, or below the chosen extraction directory for a versioned user tarball.
This upgrade does not activate hooks, install a service, change boot defaults or
write to the ESP. Activation is a separate owner operation.
