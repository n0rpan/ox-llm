# Opt-in cloud secrets and redaction

This #28 slice reads a configured credential file at request time and redacts
raw streams or decoded JSON. Enable `secrets`; default features are empty and
nothing is installed. `Key` has redacted Debug and no serialization support.
Caller code exposes it only for provider request construction/redaction.

The reader requires an absolute path outside Git/exported source trees, opens
without following the final symlink, and checks the opened descriptor's owner,
regular-file type, private permissions and 64 KiB bound. Keys must be nonempty
printable ASCII (no spaces), at most 8,192 characters. Errors contain only fixed
reasons. Nonblocking open also prevents a special file from hanging the reader.
Parent symlinks follow main's policy. Reads are never cached.

34 contracts match unmodified Python main
`d7f70bb05ed7ef65ea377e8e6cdcee0dab4415aa`: file shape/ownership/modes/symlinks,
source-tree refusal, size/key limits, UTF encodings, legacy numeric extras,
rotation, escaped JSON and split/repeated/partial-stream redaction. Every case
runs in private HOME/network/user namespaces under `/usr/bin/python3`. Synthetic
keys only; owner refusal happens before reading a foreign public metadata file
on a hosted unprivileged runner. No API call, SDK, hardware or real sign-in.

```sh
cargo build --locked --manifest-path oxc-gateway/Cargo.toml --all-features --examples
/usr/bin/python3 -m unittest discover -s oxc-gateway/conformance -v
OXC_SECRETS_CONTRACT="$PWD/oxc-gateway/target/debug/examples/secrets_contract" /usr/bin/python3 -m unittest discover -s oxc-gateway/conformance -v
```

The native example prints booleans/refusals/redacted fragments, never the key or
path. Keep an independent stream redactor for each decoded SSE field; transport
and provider-frame parsing are later slices. Catalog/policy companions #294 and
#297 introduce other opt-in features in this crate; retain all modules when
incorporating the companions. Budget/ledger and upstream routing remain separate.
