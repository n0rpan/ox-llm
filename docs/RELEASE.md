# v0.2.1 Rust-only release qualification

The owner checklist is [#763](https://github.com/oxstack/ox-llm/issues/763).
v0.2.1 installs native `oxa`, `oxe-server` and the approved
`local-ai-memlat-boot` utility with four inert MEMLAT templates. Both Rust workspaces must report
component version 0.2.1; the native engine identity contract stays strict.
v0.2.0 remains the legacy install.
The new runtime has no Python controller payload/dependency, `oxc` wrapper,
controller systemd unit, panel, LAN share or Studio. Python is still used for
source builds, licence inventory and tests; those tools are not installed runtime
components. See [native installation and upgrade](NATIVE-INSTALL.md).

`release-build.yml` creates qualification artifacts without publishing anything.
It uses only GitHub-hosted CPU runners, a pinned Fedora image, GCC16 and Rust1.99.1,
read-only permissions and no credentials. ARM keeps the existing Oryon/ARMv8.6
flags; x86_64 keeps the baseline CPU target. No numerical flag or hardware claim
changes here. No weights or vendor SDK are bundled.

## v0.2.1 Rust-only upgrade boundary

The v0.2.1 release target (#763) installs native `oxa` and `oxe-server`, retaining
the approved native MEMLAT utility and four inert templates, with
native cloud/local selection: no Python runtime dependency, Python payload,
`oxc`/diagnostics wrappers, controller daemon or service on port 8092. Packaging
must qualify that exact artifact; historical v0.2.0 recipes are not evidence
that the native release is ready. See [native install and upgrade](NATIVE-INSTALL.md)
for the owner-facing v0.2.0 upgrade procedure.

Keep provider keys in `~/.config/oxc/llm/cloud-secrets.json` and the compatible
ChatGPT token in `~/.config/oxc/llm/chatgpt-auth.json`, under the same user HOME.
Reuse them in place, retaining the refresh lock and private permissions. Do not
require reauthentication, credential export/import, a Python migration command
or deletion of user state merely to upgrade. Invalid/revoked credentials still
need normal owner recovery. Sessions and caches may start cold; credential
compatibility does not promise session, cache or usage-ledger migration.

The release qualification record must identify the v0.2.1 artifact commit and
checksums, native install/default-config checks and exact-artifact no-Python
proof. Existing legacy services/launchers must not be mistaken for the new
runtime. Clean-HOME proofs use synthetic credentials and mocks; an owner's real
upgrade preserves existing private files. No release clearance, publishing or
hardware qualification follows from this note. The sections below describe native v0.2.1 qualification.

## Artifact and package gates

The workflow builds twice in different directories, stages the same native
commands and inert templates, normalises archive metadata and compares complete tarball bytes. The
locked Cargo/licence inventory and CycloneDX SBOM retain shipped file hashes and
explicitly describe their build/test dependency scope. SHA256SUMS covers the
archives and metadata.

#682's qualifier extracts the exact archive and runs the existing isolated
mock-cloud proof against that `oxa`, with source/archive/binary hashes. Missing or
wrong artifact selection fails closed. Calibrated Python markers and exec/connect
traces reject interpreter execution or controller fallback. The ordinary suite
retains default/opt-out, denial, pre-admission, budget and report-coverage controls.
These are model-free synthetic checks, not a real-provider or local-model E2E claim.

A fresh official Arch container builds both recipes, checks checksums/namcap,
installs and removes each package, and compares installed native files with the
qualified payload. Runtime dependencies exclude Python; source build/test tools
may require it. The model-free smoke retains a real loopback `oxe-server` stub
completion with a tokenizer-only fixture. It loads no model weights. Package
removal must leave none of its installed files behind; user data is not deleted.
No package install hook starts/stops a service or reads a credential file.

Both recipes use bundle version0.2.1. The binary recipe consumes the corresponding
architecture tarball; the source recipe consumes the immutable source commit and
its verified checksum. All-zero placeholder checksums permit recipe inspection,
not an actual install/build. Never replace checksum verification with SKIP.

## Owner-only publication

Only the owner creates a tag or publishes a release. Require the checklist's
native cloud/local/sign-in prerequisites, exact-artifact qualification, clean-HOME
real tasks, dogfood pass and laptop acceptance first. #704 remains draft until the
orchestrator releases its E2E hold. The existing protected `release` environment,
public-repository/provenance refusal, pinned actions and human approval stay intact.

The tag workflow uses the qualified combined archives for aarch64 and x86_64;
it publishes no standalone legacy controller, Flash or image payload. Verify the
published archive and SBOM checksums and their attestation against the selected
source/run. No author lane creates tags/releases, changes queue policy or
substitutes a manual binary rebuild for qualified bytes.

The Arch gate runs the same no-Python target against each installed package binary,
including the independent source rebuild. Receipts pin source, package and binary
hashes; the native switch is omitted so the release default itself must select the
gateway. Missing proof inputs fail rather than falling back to a development ELF.
