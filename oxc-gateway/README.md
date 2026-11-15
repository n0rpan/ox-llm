# Opt-in Rust cloud gateway

Default features are empty; nothing is installed or starts a listener. Python is
the behavior reference. This #28 / #289 slice adds `pricing` and `accounting` to
the existing policy, secrets and catalog features.

`Price` validates flat/tiered prices and computes decimal charges for uncached
input, cache reads/writes and output. Direct Anthropic reads default to 0.1x
input; absent write rates reserve 2x. Reservations cover the full input window
and maximum tier/cache rates, plus 25% OpenRouter headroom. The caller must
validate and bound output; zero is refused here too.

Share one `Budget` per day across requests. `reserve` locks spend and outstanding
holds together. Mark forwarded before paid I/O; `finish` retains the hold until
its one writer callback completes. That callback adds private metadata, never
prompts/replies, and must not re-enter the budget. Redact provider strings first.
`append_private` writes mode 0600 under a dedicated mode-0700 directory. Failed
writes or dropped forwarded holds block spending. Missing/failed usage costs the
reservation; cancellation uses final usage or a bounded byte/time estimate that
includes hidden reasoning.

OpenRouter's reported cost wins even when below catalog cost, intentionally
fixing #283 rather than copying Python's remaining maximum-of-the-two defect.
The catalog estimate remains a separate field. `SpendReader` matches main's
incremental #296 reader: skip only an unfinished last line, reject malformed
complete rows, and retain a running total/cursor. Replacement, truncation or a
changed 64-byte cursor tail resets it. Unchanged files require bounded work.

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

```sh
cargo check --locked --manifest-path oxc-gateway/Cargo.toml --no-default-features
cargo test --locked --manifest-path oxc-gateway/Cargo.toml --all-features
cargo build --locked --manifest-path oxc-gateway/Cargo.toml --all-features --examples
/usr/bin/python3 oxc-gateway/conformance/run.py --python
/usr/bin/python3 oxc-gateway/conformance/run.py --native-dir /absolute/cargo-target/debug/examples
```

Forty-seven Python price/settlement contracts and ten native regressions cover
concurrency, partial appends, rewrites, permissions, cache charges and failed
writes. Fixtures use synthetic inputs; source goldens use one case per line.

Private runtime metadata, forwarding/cancel and installation follow separately.
Preserve every present feature/module/example when merging companion slices.

Main’s `secrets` module and its explicit native contracts are preserved. The
symlink-loop case validates the two complete interpreter-specific safe refusals
from Python 3.12/3.13; all other secret errors remain exact. Credentials are read
from owner-only files and are never logged or included in errors.

Cancellation estimates use the maximum cache rate, correcting #283. Three
fixtures retain unchanged Python rows and native expectations derived with its
conservative cost oracle. Final usage stays exact; cap estimates at the hold,
while retaining any known actual charge above it.

The explicit conformance runner selects every current module: 100 cases across
catalog, policy, secrets and pricing. Native mode requires each executable
`<name>_contract` for `test_<name>.py`, failing before any tests if a driver is
missing; source mode clears inherited native overrides. Both the required
checks.yml gateway job and the manual workflow run source/native contracts and
Rust tests with private HOME, cleared environment and private network.

`Catalog::from_config` takes a trusted normalized public-price snapshot keyed
by upstream ID. This keeps file IO outside metadata; valid/no configured router
prices receive the overlay, while invalid configured prices stay invalid. Source
contracts seed an owner-only public cache and make no public network calls.
