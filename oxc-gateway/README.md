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

Twelve policy contracts exercise the current Python refusal stages. Eleven use
the real HTTP controller with owned mocks and a foreign inference lease; the
guest case enters its already-authenticated guest context. The accepted case
reaches exactly one mock, while refusals forward none. Native checks exercise
the same policy decisions.

The outstanding-hold case seeds $0.50 spend and holds a $0.40008 Python
reservation at the owned upstream. A second request must be refused before
another provider call, so dropping the reserved term fails native conformance.
