# Guarded fresh build and four native software checks

The first actual public Flash source acquisition [passed](../benchmarks/results/bootstrap-flash-source.json). A compiler run, four software checks and complete model/runtime qualification are separate gates. This page describes the reviewed executors; it does not turn their source-only tests into native acceptance. No command below installs a runtime or enables a profile.

## Dependencies and isolation

Install normal distribution packages for Git, [CMake](https://cmake.org/download/), [Ninja](https://github.com/ninja-build/ninja), [GCC/G++](https://gcc.gnu.org/), Python 3, OpenMP/libgomp, OpenSSL development/runtime files and util-linux `unshare`. Recorded historical versions are GCC 16.1.1, CMake 4.4.4 and Ninja 1.13.2. The explicit Oryon target must be supported. Paths come from the reviewed job's PATH, not a particular user's virtual environment.

One local preflight failed because that laptop's Python environment had no `cmake` launcher in its `bin` directory; the actual CMake lived in the package's `cmake/data/bin` directory. A local PATH override resolved it. **Fresh users need an installed CMake command, not this private environment layout.** A second preflight rejected CMake's normal expansion of `cc`/`c++` to full paths. Executor revision 2 accepts that expansion only when it is the same executable inode as the requested selector resolved from the job PATH, and records effective paths. All other option equality remains exact. Both failures remain failed receipts; neither was a compiler failure or a successful build.

The build copies the passed source tree into a fresh workspace using independent files, audits Git metadata/hardlinks, verifies the exact source again and configures out of tree. It uses Release, one build job and the [explicit source recipes](../bootstrap/source-recipes.json). The Flash recipe preserves repack/KleidiAI OFF and CPU-only streaming. The classic recipe preserves FP16+BF16, KleidiAI/repack/OpenMP/OpenSSL ON; it is not the earlier MTP recipe.

Both configure and compile run under `unshare --user --map-root-user --net`. This denies even CMake `file(DOWNLOAD)` network access; `FETCHCONTENT_FULLY_DISCONNECTED` also stays ON. User/network namespace support is a prerequisite and absence fails closed. No host network setting is changed. Dependencies are acquired explicitly before that namespace, never through an uncontrolled latest fallback.

Classic builds prefetch and SHA-check KleidiAI 1.32.0 and the exact b12189 prebuilt UI. The UI is extracted to the copied source's `tools/ui/dist/index.html` priority path; upstream returns before npm/Hugging Face branches when that path is present. `LLAMA_USE_PREBUILT_UI` remains ON and the embedded browser UI is preserved. Merely setting BUILD_UI OFF would not prevent upstream downloads. [UI metadata pin](../bootstrap/ui-pin.json), [KleidiAI and UI acquisition pins](../bootstrap/source-recipes.json). Archive extraction refuses links/path escapes/special files and bounds member count and expanded size.

## Run a fresh Flash build after source PASS

The [archived Flash build procedure](https://github.com/oxstack/ox-llm/blob/e64b985a2ff6abf51d406471382ed95a18fd161c/oxc/docs/BOOTSTRAP-BUILD.md#run-a-fresh-flash-build-after-source-pass) records the legacy Python build command and its inputs. The command's recorded manifest digest differs from the archived build manifest, so the archived executor refuses that invocation. The [native install and upgrade guide](https://github.com/oxstack/ox-llm/blob/main/docs/NATIVE-INSTALL.md) covers v0.2.x product setup.

The guard requires 8 GiB available RAM at build start and a live 2 GiB reserve, canonical exclusive lease, AC/30% start/20% cutoff, sustained-discharge monitoring and own zero swap. CPU/memory/tasks/deadlines are checked against the dedicated user unit. New global page-outs stop the job; existing global page-ins are recorded. Parent deadline is 2600 seconds inside the 2700-second unit. Combined logs are capped at 16 MiB, all children must be reaped, and actual cgroup peak/events are captured. There is no retry/fallback loop.

A passed `build-result.json` records actual tool versions, effective compiler paths, exact options, dependency identities and new binary SHA256. It remains **`qualified:false`, `models_run:false`, `profiles_enabled:false`**. Build targets include native test executables, but building them does not execute their assertions. Generated version metadata and new toolchains can change binary hashes; do not copy a historical hash into a new profile.

## Run exactly four model-free native checks after build PASS

The [archived four-check procedure](https://github.com/oxstack/ox-llm/blob/e64b985a2ff6abf51d406471382ed95a18fd161c/oxc/docs/BOOTSTRAP-BUILD.md#run-exactly-four-model-free-native-checks-after-build-pass) preserves the legacy Python runner invocation, its hashed manifest and the receipt requirements. The archived runner executes four fixed Flash test binaries without loading model weights.

Use the [native install and upgrade guide](https://github.com/oxstack/ox-llm/blob/main/docs/NATIVE-INSTALL.md) for v0.2.x installations. The historical procedure records build and check evidence for the corresponding inputs.

After all four pass, architecture byte-identity tests, real default MoE/Q8 and optional Dense/Flash/Q4 qualification, OpenCode/browser/image workflow, private installation/rollback and the right-side widget still need their own clean-install acceptance. [Complete finish gate](BOOTSTRAP-PARITY.md).
