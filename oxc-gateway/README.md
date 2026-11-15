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

## OpenRouter protocol (`openrouter`)

The opt-in protocol layer builds the mandatory ZDR/no-data-collection body,
validates client function tools and output limits, maps reasoning controls and
normalizes JSON/SSE replies. Local engine controls are removed. A request still
needs the caller's policy, credentials and budget checks before forwarding.
`Stream` handles LF/CRLF/CR, comments, multiline data and EOF; it redacts decoded
content/reasoning/refusal/tool arguments separately and also scrubs raw frames.
Tool names are redacted in place and emitted whole, as required by #217/#331.
This intentionally avoids the current reference's name hold-back defect.

Thirty-four Python/native protocol cases use private HOME/network namespaces and
synthetic data, including byte-at-a-time UTF-8/SSE, tools and interleaved choices.
Six native regressions cover whole names before finish, every two-way split of
a secret-bearing tool flow, and ordering of held content before terminal usage.
No HTTP client, paid retry, cancellation or budget/private-metadata composition
is provided by this protocol-only slice; transport follows separately. For a
native run also set `OXC_OPENROUTER_CONTRACT` to the absolute built example path.

EOF flushes held content and arguments even without a finish marker. Keyed
streams require one whole name per choice/tool slot: identical metadata repeats
are suppressed; changing/fragmented names emit a static terminal error instead
of assembling a credential. `is_failed()` requires transport cancellation and
failed settlement. Scalar frames and tail metadata receive decoded redaction.
