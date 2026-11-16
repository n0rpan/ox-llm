# Native install (Rust-only MVP)

The default installed product is `oxa`, `oxe` and `oxe-server`. Install a verified
native package or extract its inspected `usr/` payload using the package manager's
normal procedure. Build-time Python tools are not runtime dependencies.

For the Arch package, inspect the selected build's package and checksums before
installation (substitute the actual inspected package filename):

```sh
sha256sum -c SHA256SUMS --ignore-missing
sudo pacman -U ./ox-llm-bin-VERSION-ARCH.pkg.tar.zst
oxa --version
oxa --help
```

The source recipe needs Python to build/stage the package; the installed binary
package does not depend on Python. This draft publishes no release artifact.
Do not run `python -m oxc.cloud_install` or the Python bootstrap as an additional
step of this native install.

The default settings enable `oxa.engine.inProcessCloud` and
`oxa.engine.inProcessLocal`; this retirement PR is their activation point.
Run `oxa` in the project directory. `oxc code` is a compatibility launcher for that
same binary; `oxc plan` selects its read-only file tools. Neither starts a controller.
The local engine is an owned child of the native local path, not a systemd Python
service. No model weights or credentials are bundled or downloaded by package installation.

Local model paths, explicit launch arguments and environment belong in the trusted
user's `oxa.local.models.<alias>` configuration and the native installed layout.
Cloud configuration belongs in the user's `oxa.cloud` object. Project settings
must not choose executable paths or credentials. Missing local models or invalid
native configuration must produce a native configuration error, never start the
retired controller or invoke a Python installer.

Panel integration, LAN share and Studio are disabled by default in this install.
Their compatibility commands return one explanatory line. No Python chat, Studio,
digest, storage-cleanup or optional Flash helper unit is installed or enabled.
The package manager prints a one-line notice; it does not mutate a user account,
start services, open credential files or sign anyone in.

The Python source is still in the repository for the legacy v0.2.0 release. It is
not copied into the default installed runtime. A later deletion PR removes that
source after native end-to-end acceptance, with retained notices and historical
receipts reviewed separately. Python remains available to builders/CI for staging,
license inventory and tests; its use there does not make it an installed controller.

## Activation gate for the retirement draft

The retirement PR must remain DRAFT until the orchestrator confirms the native
end-to-end run passed. Native cloud selection (S7) and local slice-5 selection must
land and be composed before its defaults can be finalized. In particular, an empty
cloud catalog must not prevent local operation; no key or cloud account is needed
for the local path. The accepted run must prove fresh-user native settings, the
owned local engine, real task/tool turns, correct budget/ledger behavior where
applicable, and no Python process or controller-port fallback. Package staging and
stub smoke alone do not establish this acceptance or laptop cache/latency parity.
