# Opt-in cloud gateway building blocks

Default features are empty; no listener or installer is enabled. Enable only the
needed modules. Policy stages consume trusted gateway facts; pricing and atomic
settlement are separate work. Python remains the behavior reference.

- `cloud-only`: catalog contracts.
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

Main’s `secrets` module and its explicit native contracts are preserved. The
symlink-loop case validates the two complete interpreter-specific safe refusals
from Python 3.12/3.13; all other secret errors remain exact. Credentials are read
from owner-only files and are never logged or included in errors.

`Catalog::from_config` takes a trusted normalized public-price snapshot keyed
by upstream ID. This keeps file IO outside metadata; valid/no configured router
prices receive the overlay, while invalid configured prices stay invalid. Source
contracts seed an owner-only public cache and make no public network calls.
