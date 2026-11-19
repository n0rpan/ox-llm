# Contributing

This is a lab. We try ideas, measure them honestly and keep the failures. Contributions that come with
a measurement, or that make measuring easier, are the most useful. Good places to start:
[oxc/docs/HELP-WANTED.md](oxc/docs/HELP-WANTED.md).

[AGENTS.md](AGENTS.md) holds the rules for every change: pull requests, CI, what blocks a merge, the
definition of done, the hard rules and the review guidelines. They apply to people and to agents, and
they are not repeated here. [SECURITY.md](SECURITY.md) says how to report a vulnerability. Measurement
details are in [oxc/CONTRIBUTING.md](oxc/CONTRIBUTING.md) (measurement rules, receipts, privacy) and
[engine/CONTRIBUTING.md](engine/CONTRIBUTING.md) (kernels, vendored code, engine tests).

## Measuring engine changes

1. **Declare the class** of a change before measuring it: *lossless* (bit-identical against the
   reference), *reorder* (within the reference's measured noise) or *lossy* (opt-in only, with a quality
   metric and a task check).
2. **Interleave arms** (A B B A), drop the warm-up and compare medians. A change under 3% is noise for
   a decision. These laptops throttle within about 30 s of all-core load.
3. **Production shapes:** real context sizes, head counts and thread counts.
4. **A new engine path starts behind a flag,** with a model-free bitwise or reference test. When every
   measured axis is neutral or better, propose it as the default at once; the owner decides production
   defaults. Product features in `oxa` are on by default with an opt-out setting.
5. **Client-neutral core:** no client-specific code, version pins or byte templates in `engine/`. Client
   policy belongs in the client (`oxa/`) or in a thin optional adapter.
6. **Isolation in tests:** every test runs with a throwaway `HOME` and XDG directories, and never
   touches an installed runtime, its state or a model.

## Privacy and licences

- No model weights, transcripts, prompts, generated images, home paths, user or host names, serials,
  keys or vendor (Qualcomm) SDK, firmware or runtime files in commits or issues. Check a receipt before
  you commit it: `python3 -I tools/ci/scrub_receipts.py --check <file>`.
- Contributions are accepted under the MIT licence. Keep the upstream notices in `engine/vendor/`
  intact and keep `oxa/NOTICE` (pi attribution) current when you port a design or code.

## Tests

On your own machine you may build and test freely; the build limits in AGENTS.md are for the owner's
machines. From the repository root:

```sh
(cd engine && cargo test --release --locked --workspace --exclude qwen4exp)   # engine
(cd oxa && cargo test --locked)                                               # oxa
python3 -I -m unittest discover -s tools/ci/tests                             # CI tooling
```

Linux oxa integration tests require `bubblewrap` and `strace` on `PATH` (Arch:
`sudo pacman -S bubblewrap strace`; Debian/Ubuntu: `sudo apt-get install bubblewrap strace`). Hosted CI
installs both. The native cloud tool-loop test uses `strace -f -e trace=execve` to verify that the real
binary and its descendants never launch Python. A run without the tracer is not a valid substitute, and
a tracer that cannot trace (for example, denied `ptrace`) is fixed, not skipped.

Where you cannot install packages, build upstream `strace` inside the checkout (needs a C compiler,
make, curl, tar and xz). The two tests that need it look for `.local-checks/strace/bin/strace` before
they search `PATH`:

```sh
mkdir -p .local-checks/strace-src
printf '*\n' > .local-checks/.gitignore
curl --fail --location --proto '=https' --tlsv1.2 \
  https://strace.io/files/7.3/strace-7.3.tar.xz \
  -o .local-checks/strace-src/strace-7.3.tar.xz
printf '%s  %s\n' \
  529634c40910895a9a1f3c5c1e65da2ca9ed72fb158dfc4bcc89efdf05e39b72 \
  .local-checks/strace-src/strace-7.3.tar.xz | sha256sum --check &&
tar -xJf .local-checks/strace-src/strace-7.3.tar.xz -C .local-checks/strace-src
prefix="$PWD/.local-checks/strace"
(
  cd .local-checks/strace-src/strace-7.3 &&
  ./configure --prefix="$prefix" --enable-mpers=no --enable-bundled=yes &&
  make -j2 && make install
)
(cd oxa && env -u TMPDIR cargo test -p oxa-cli --test in_process_cloud \
  native_discovery_tool_loop_and_budget_settlement_use_no_python_process)
```

The bundled headers avoid incompatibilities with newer kernel headers, and disabling multi-personality
decoding avoids a 32-bit toolchain; neither changes the `execve` tracing. Keep the test's temporary
directory outside any repository (hence `env -u TMPDIR`): its synthetic credential fixture is subject to
the production refusal of keys inside a repository. `CARGO_TARGET_DIR` may still point inside the
checkout.
