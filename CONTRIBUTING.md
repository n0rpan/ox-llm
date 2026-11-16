# Contributing

This is a lab. We try ideas, measure them honestly and keep the failures. Contributions that come with
a measurement, or that make measuring easier, are the most useful. Good places to start:
[oxc/docs/HELP-WANTED.md](oxc/docs/HELP-WANTED.md).

Each product has its own guide with the details:
[oxc/CONTRIBUTING.md](oxc/CONTRIBUTING.md) (measurement rules, receipts, privacy, lab tests) and
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
   `oxc/src`. Client policy belongs in the client (`oxa/`) or in a thin optional adapter.
6. **Isolation in tests:** every test runs with a throwaway `HOME` and XDG directories, and never
   touches an installed runtime, its state or a model.

## Privacy and licences

- No model weights, transcripts, prompts, generated images, home paths, user or host names, serials,
  keys or vendor (Qualcomm) SDK, firmware or runtime files in commits or issues. Export receipts with
  `python3 -I oxc/tools/export_receipt.py export ...`.
- Contributions are accepted under the MIT licence. Keep the upstream notices in `engine/vendor/`
  intact and keep `oxa/NOTICE` (pi attribution) current when you port a design or code.

## Tests

Linux oxa integration tests require `bubblewrap` and `strace` on `PATH` (Arch:
`sudo pacman -S bubblewrap strace`; Debian/Ubuntu: `sudo apt-get install bubblewrap strace`).
The native cloud tool-loop test uses `strace -f -e trace=execve` to verify that the real binary
and its descendants never launch Python; running without the tracer is not a valid substitute.
Hosted CI installs both prerequisites too.

If `bubblewrap` is already available but host package installation is not permitted, build
upstream `strace` inside the checkout instead (requires a C compiler, make, curl, tar and xz).
From the repository root:

```sh
mkdir -p .local-checks/strace-src
printf '*\n' > .local-checks/.gitignore
curl --fail --location --proto '=https' --tlsv1.2 \
  https://strace.io/files/7.3/strace-7.3.tar.xz \
  -o .local-checks/strace-src/strace-7.3.tar.xz
tar -xJf .local-checks/strace-src/strace-7.3.tar.xz -C .local-checks/strace-src
prefix="$PWD/.local-checks/strace"
(
  cd .local-checks/strace-src/strace-7.3 &&
  ./configure --prefix="$prefix" --enable-mpers=no --enable-bundled=yes &&
  make -j2 && make install
)
export PATH="$prefix/bin:$PATH"
strace -V
(cd oxa && cargo test -p oxa-cli --test in_process_cloud \
  native_discovery_tool_loop_and_budget_settlement_use_no_python_process)
```

The bundled headers avoid incompatibilities with newer system kernel headers; disabling
optional multi-personality decoding avoids needing a 32-bit toolchain. Neither changes the
native `execve` tracing used by this test. Keep this `PATH` prefix for subsequent check commands
(and set it again in each new shell); all downloaded sources and build/install output remain
ignored inside `.local-checks/`. An export in a setup shell does not update a separate check
runner's environment. For a runner that starts a fresh shell, pass the prefix with the check
command itself (from the repository root, after building above):

```sh
(cd oxa && env -u TMPDIR PATH="$PWD/../.local-checks/strace/bin:$PATH" \
  cargo test -p oxa-cli --test in_process_cloud \
  native_discovery_tool_loop_and_budget_settlement_use_no_python_process)
```

Use the same explicit `PATH` prefix for any outside check runner that executes this test.
No test is skipped or substituted, and no host installation is changed. A failure to trace (for example, denied `ptrace`) must still be fixed, not ignored.
Keep the test's temporary directory outside any repository: its synthetic credential fixture
is subject to the production repository-key refusal. If `TMPDIR` points inside a checkout,
run the test with `env -u TMPDIR cargo test ...` to use the system temporary directory instead;
`CARGO_TARGET_DIR` may still point inside the checkout for build output.

```sh
cd oxc && ./tools/run_lab_tests.sh                       # lab
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
