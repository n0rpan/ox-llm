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

- Draft PRs run formatting, cheap path controls and all secret, SDK-material, product-isolation
  and receipt scans. Test locally while iterating, then mark the PR ready for review.
- Ready PRs also run clippy, the oxa build and the x86 stub checks. Engine/Flash builds, oxa's
  mandatory-bwrap workspace tests and the lab suite skip via job-level conditions, so all nine
  required check names still report. Documentation links are checked when applicable.
- The native merge queue (`merge_group`) is the full integration gate. Queue runs, pushes to
  main, manual runs and nightly runs execute the full set, including the loopback OpenAI contract,
  regardless of changed paths. Ordinary branch pushes do not duplicate the PR workflow.

Workflows use GitHub-hosted runners and pinned actions. Only PR events cancel superseded runs;
main and merge-group runs are not cancelled in progress. Existing cargo and pip caches contain
no credential files. No real cloud key or paid API is used by tests.
