# Native install (v0.2.1)

v0.2.1 installs `oxa`, `oxe-server` and the native `local-ai-memlat-boot` utility. It ships no Python
runtime/controller payload, `oxc` wrapper, panel, LAN share, Studio or controller
systemd unit. Python remains a build/test tool for source packages, not a runtime
dependency. Both `oxa` and the engine must report component version 0.2.1; mixed
component versions are refused. v0.2.0 remains the legacy installation.

Inspect the selected build's package, provenance and checksums before installation:

```sh
sha256sum -c SHA256SUMS --ignore-missing
sudo pacman -U ./ox-llm-bin-VERSION-ARCH.pkg.tar.zst
oxa --version
oxa --help
```

The combined tarball has the same `usr/` layout as the Arch packages. Install it
using the normal system package procedure. Do not run the Python bootstrap or
cloud installer as an additional installation step. No weights, credentials or
account sign-in are bundled or downloaded by installation.

The defaults select `oxa.engine.inProcessCloud` and `oxa.engine.inProcessLocal`.
Explicit user false remains respected. Trusted user settings own model paths,
launch arguments/environment, cloud catalog and budget; project settings cannot
choose executable or credential paths. `oxa` owns the local engine child. There
is no controller endpoint on port 8092 and no controller service to start.

The installed engine path is `/usr/bin/oxe-server`. Put weights at the path named
by the local model entry, for example `~/models/model.gguf`; no download manager
is included. Configure `oxa.local.binary`, per-alias `oxa.local.models` and the
qualified native limits as described in the [native local configuration](https://github.com/oxstack/ox-llm/blob/main/oxa/oxc-local/CONFIGURATION.md).
Cloud catalog and budget configuration live in the user's `oxa.cloud` object.
Missing weights or invalid native configuration must produce a clear error,
never invoke a Python installer or silently fall back to a controller.

## Upgrade from v0.2.0

Stop and disable any legacy controller service you previously enabled before
using the new installation. The package does not stop processes or modify user
systemd configuration automatically; it installs no replacement controller unit.

Keys and the ChatGPT token record are reused in place at the existing private
`~/.config/oxc/llm/cloud-secrets.json` and
`~/.config/oxc/llm/chatgpt-auth.json` locations. Installation does not open, move,
rewrite or print them. It does not perform a sign-in. Keep private file modes;
never put credentials in project settings or command-line arguments.

Legacy controller configuration and warm caches are not imported automatically.
Configure native models and a cloud budget in oxa; caches and sessions may start
cold. Existing user data is not deleted. This upgrade does not promise laptop
cache/latency parity or validate a real provider/model.

## Acceptance boundary

This packaging PR stays draft until the orchestrator accepts the native end-to-end
run. Local/cloud factory and sign-in production prerequisites, release version
qualification and #682's exact-artifact CI must be composed before release. A
mock-cloud proof, staging/unit checks or tokenizer-only stub smoke alone does not
establish real-provider, local-model, hardware or full E2E acceptance.

## MEMLAT templates

The four qualified MEMLAT hook/script templates ship as inert data under
`/usr/share/ox-llm/memlat/`. Installation does not activate hooks, install a
service, change boot defaults or write to the ESP. Any activation or laptop
installation requires the owner’s separate approval.
