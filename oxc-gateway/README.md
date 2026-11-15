# Opt-in Rust cloud gateway policy

This #28 slice supplies safe refusal stages: unavailable credentials, offline,
LAN guests, missing pricing/budget, exhausted spend and unreadable/disabled
ledgers. Enable the `policy` feature; defaults are empty and nothing is installed.
The functions consume trusted gateway facts. They neither read a credential nor
create a reservation or contact a provider. Atomic accounting follows separately.

Twelve contracts match main `bbec7919fd54ddcce6de49c6c33264e50e9a7494`.
Eleven use the unmodified Python HTTP controller in private HOME/network namespaces
with an owned loopback mock and a foreign lease held through stop. Refusals send
no provider request; the accepted case reaches exactly one mock. The guest case
exercises the reference's already-authenticated guest context; live LAN listener
integration remains separate. Native tests exercise the library decision stages.

```sh
cargo build --locked --manifest-path oxc-gateway/Cargo.toml --all-features --examples
/usr/bin/python3 -m unittest discover -s oxc-gateway/conformance -v
OXC_POLICY_CONTRACT="$PWD/oxc-gateway/target/debug/examples/policy_contract" /usr/bin/python3 -m unittest discover -s oxc-gateway/conformance -v
```

Bubblewrap and `/usr/bin/python3` are required. Goldens are captured only with
`test_policy.py --record-python`. Run strict all-target/all-feature Clippy,
formatting and an empty-default build before readying. Companion #294 introduces
catalog metadata for the same library; preserve both features/modules/examples
when incorporating it. Full HTTP composition, secrets, atomic settlement and
upstream protocols remain incomplete. Python behavior stays unchanged.

The outstanding-hold case seeds $0.50 spend and holds a $0.40008 Python
reservation at the owned upstream. The next request is refused before a second
provider call, so dropping the reserved term makes native conformance fail.
