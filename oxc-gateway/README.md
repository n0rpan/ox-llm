# Opt-in cloud gateway building blocks

Default features are empty; no listener or installer is enabled. Enable only the
needed modules. Policy stages consume trusted gateway facts; pricing and atomic
settlement are separate work. Python remains the behavior reference.

- `policy`: policy contracts.
- `secrets`: secrets contracts.

Build all examples with `cargo build --locked --manifest-path
oxc-gateway/Cargo.toml --all-features --examples`. Run the source contracts with
`/usr/bin/python3 -m unittest discover -s oxc-gateway/conformance -v`; native
checks set each present `OXC_*_CONTRACT` to its absolute example executable.

Source cases use private HOME/network namespaces and synthetic data. No real
API, credential, SDK material or sign-in is used. Every opt-in module forbids
unsafe code. Preserve all present feature/module/example entries when merging
companion slices. Full cloud-only forwarding/accounting remains incomplete.

The secrets loop contract explicitly supports Python 3.12's generic refusal and
3.13's symbolic-link refusal: it validates one of those two complete messages
before normalizing only that wording. Native uses the symbolic-link message.
The secrets subset passes both Python versions and native; every other error is
compared exactly. Credential values and paths must never appear in errors.

`secrets::KeyFile::prepare` validates the configured location once per config
generation. Keep its Result and surface the static refusal after policy checks;
repository/source-tree paths now have their own safe reason (#331/#266). Parent
links bind to an open descriptor for the approved canonical directory, so
retargeting a link or replacing that directory cannot select a repository key.
The directory descriptor is close-on-exec; no key descriptor is retained.
Reprepare when configuration/location changes. Every
read still opens with no-follow and verifies regular-file type, owner, mode,
size and fresh bytes. Key material is never cached; Debug omits the path.
The parent directory must exist when preparing the location. Create it before
constructing/reloading the configuration; a missing file within it is allowed.
Four filesystem regressions cover parent retargeting/replacement, missing parents,
rotation, mode changes and final symlinks. The 34 source contracts stay unchanged; three native repo refusals
intentionally use the requested specific diagnostic rather than Python's generic
message. All other errors remain exact, except the already validated Python
3.12/3.13 symlink-loop wording.
