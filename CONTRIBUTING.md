# Contributing

This is a lab. We try ideas, measure them honestly and keep the failures. Contributions that come with
a measurement, or that make measuring easier, are the most useful. Good places to start:
[lab/docs/HELP-WANTED.md](lab/docs/HELP-WANTED.md).

Each product has its own guide with the details:
[lab/CONTRIBUTING.md](lab/CONTRIBUTING.md) (measurement rules, receipts, privacy, lab tests) and
[engine/CONTRIBUTING.md](engine/CONTRIBUTING.md) (kernels, vendored code, engine tests).

## Ground rules

1. **Declare the class** of a change before measuring it: *lossless* (bit-identical against the
   reference), *reorder* (within the reference's measured noise) or *lossy* (opt-in only, with a quality
   metric and a task check).
2. **Interleave arms** (A B B A), drop the warm-up and compare medians. A change under 3% is noise for
   a decision. These laptops throttle within about 30 s of all-core load.
3. **Production shapes:** real context sizes, head counts and thread counts.
4. **New paths ship behind a flag, default off,** with a model-free bitwise or reference test.
5. **Client-neutral core:** no client-specific code, version pins or byte templates in `engine/` or
   `lab/src`. Client policy belongs in the client (`oxa/`) or in a thin optional adapter.
6. **Isolation in tests:** every test runs with a throwaway `HOME` and XDG directories, and never
   touches an installed runtime, its state or a model.

## Privacy and licences

- No model weights, transcripts, prompts, generated images, home paths, user or host names, serials,
  keys or vendor (Qualcomm) SDK, firmware or runtime files in commits or issues. Export receipts with
  `python3 -I lab/tools/export_receipt.py export ...`.
- Contributions are accepted under the MIT licence. Keep the upstream notices in `engine/vendor/`
  intact and keep `oxa/NOTICE` (pi attribution) current when you port a design or code.

## Tests

```sh
cd lab && ./tools/run_lab_tests.sh                       # lab
cd engine && cargo test --release --locked --workspace --exclude qwen4exp   # engine (Flash: see engine/CONTRIBUTING.md)
cd oxa && cargo test --locked                            # oxa
python3 -I -m unittest discover -s tools/release/tests   # release tooling (repository root)
```

Hosted CI runs in three tiers:

- Branch pushes run formatting, clippy and unit tests for changed products; unknown or shared
  tooling paths select all products. Engine pushes exercise the x86_64 stub; oxa pushes run
  workspace library tests; lab pushes run the model-free lab suite.
- Source pull requests (including stacked PRs) and main pushes run the full x86_64/aarch64
  builds, oxa workspace and behavior suite, release-profile tests with required bwrap, lab
  tests and the OpenAI contract against loopback servers.
- Nightly on main and manual runs execute the full set. Docs-only changes run local Markdown
  link checks. Secret, SDK-material, product-isolation and receipt scans run on every event,
  including docs-only changes.

Workflows use GitHub-hosted runners and pinned actions. Branch concurrency cancels superseded
runs. Cargo registry/git/build caches and pip download caches contain no credential files.
The path classifier's unit tests run in the classification job. No real cloud key or paid API
is used by tests.
