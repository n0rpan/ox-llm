# Python port ledger

Release qualification for #951, inspected at main fd337a5f and packaging candidate #824 at f38febd3. This initial ledger records three provider replacements, the MEMLAT port, two retained feature gaps and the explicit installer retirement; it inventories the original 366 Python modules. It authorizes no deletion.

PORTED means the named Rust implementation and its contract tests replace the feature. DROPPED requires an explicit retirement decision and an entry in #946. UNUSED requires caller/shipping evidence. MISSING requires a feature-specific issue before that module can be deleted. Unqualified modules below are deliberately not assigned a guessed status.

| Python module / feature | Status | Rust replacement (crate/module) | Evidence |
|---|---|---|---|
| oxc/src/oxc/runtime/cloud_anthropic.py — Anthropic transport, translation and native tool loop | PORTED | oxa/oxc-gateway/src/anthropic_client.rs; anthropic_request.rs; anthropic_stream.rs | oxa/oxc-gateway/tests/anthropic_client.rs; anthropic_request.rs; anthropic_stream.rs; production consumer: oxa/oxa-cli/src/in_process.rs. Source/test mapping, not live-provider acceptance. |
| oxc/src/oxc/runtime/cloud_openai.py — Responses API-key transport, translation and native tool loop | PORTED | oxa/oxc-gateway/src/openai/client.rs; request.rs; stream.rs | oxa/oxc-gateway/tests/openai_client.rs; oxa/oxa-cli/tests/in_process_cloud.rs; production consumer: oxa/oxa-cli/src/in_process.rs. Source/test mapping, not live-provider acceptance. |
| oxc/src/oxc/runtime/cloud_chatgpt.py — subscription transport, account-pinned continuation, sign-in and refresh | PORTED | oxa/oxc-gateway/src/openai/client.rs; chatgpt_tokens.rs; login.rs; oxa/oxa-cli/src/login_cmd.rs | oxa/oxc-gateway/tests/chatgpt_tokens.rs; openai_client.rs; oxa/oxa-cli/tests/native_chatgpt_login.rs; production consumer: oxa/oxa-cli/src/in_process.rs. Source/test mapping, not live-provider acceptance. |
| oxc/tools/memlat_boot.py — boot-menu/rollback command | PORTED | oxa/oxc-memlat/src/{main,menu,policy,image}.rs | oxa/oxc-memlat/tests/boot.rs ports all 19 Python cases. The actual Rust command replays 28 scenarios and 52 steps against tests/boot-oracle.json, captured with the same synthetic filesystem on both hosted architectures in [run 44196495976](https://github.com/oxstack/ox-llm/actions/runs/44196495976). Replay checks exit, stderr, selected stdout and pre/post ESP file names and bytes by digest. The old command remains subject to packaging, archive and caller gates. |
| oxc/tests/test_memlat_boot.py — all19 synthetic boot cases | PORTED | oxa/oxc-memlat/tests/boot.rs | Explicit port declaration and matching19-case source inventory. No host boot actions. Keep the stronger actual-command differential coverage. |
| oxc/src/oxc/runtime/cloud_prices.py — live public OpenRouter pricing metadata | MISSING | Existing native pricing.rs parses configured prices; that is not a refresh consumer. | Retained feature issue [#628](https://github.com/oxstack/ox-llm/issues/628). Preserve bounded replies, durable last-known data and no request-path fetch before deleting this module. |
| oxc/src/oxc/runtime/lan_share.py — production owner share controls | MISSING | Native owner CLI and local serving in `oxa/oxa-cli/src/share_cmd.rs` and `oxa/oxc-gateway/src/share/native.rs` | [#1216](https://github.com/oxstack/ox-llm/pull/1216) and [#1235](https://github.com/oxstack/ox-llm/pull/1235) cover foreground serving of selected local aliases and bind restrictions in `oxa/oxa-cli/tests/share_keys.rs`. `oxa/oxc-gateway/tests/share_listener.rs` covers guest streaming, usage settlement and revocation. [#1237](https://github.com/oxstack/ox-llm/pull/1237) adds sharing the interactive owner’s native engine. `share_cmd.rs::owner_router_and_listener_use_the_same_local_engine` verifies shared engine ownership, and `share_keys.rs::owner_session_sharing_flags_refuse_noninteractive_and_agent_invocations` verifies owner entry gating. Full [#688](https://github.com/oxstack/ox-llm/issues/688) qualification still needs the owner-enabled cloud route and the remaining end-to-end tailnet and owner controls. |
| oxc/src/oxc/cloud_install.py — Python-only installer/venv/controller daemon | DROPPED | Direct native oxa package/default path; no replacement Python installer. | Owner retirement [#681](https://github.com/oxstack/ox-llm/issues/681); [parked feature index](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947705088). Existing lab wrapper callers still block deletion; not UNUSED. |
| oxc/src/oxc/bootstrap.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_build.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_checks.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_download.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_guard.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_install.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_jobs.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_sources.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_transfer.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |

Deletion gates: packaging must stop staging/smoking Python first (#824); qualify each remaining caller, archive archive/python-oxc, then delete only qualified rows in ledger order. A provider mapping alone does not retire its controller, CLI, Studio, metadata refresh, or legacy tests.

Known callers on the inspected base included packaging/release/{stage,smoke}.py, the Python lab job in .github/workflows/checks.yml, tools/{oxa-contract,openai-contract}/run.sh, tools/gates/tiny_real.sh and related documentation. The MEMLAT differential now uses the captured reference described above. Synthetic path strings in tools/ci/tests must be distinguished from executable imports before changing them.

Native lab transition: existing Go profiles, ledgers, sessions and caps must remain intact. ChatGPT requires its normal protected token reader; no operator copying/reading tokens, implicit re-login or fresh accounting store. Ordinary native use retains the canonical protected token reader. Custom lab HTTP routes remain a separate migration boundary; no credential import or accounting reset is inferred.

## Qualified component receipts and release blockers

[CI run 44093927834](https://github.com/oxstack/ox-llm/actions/runs/44093927834), source 9a1dbaff4e8dd653357bd19e44ab67bb0d3c7167, records passing native Anthropic/OpenRouter tool-loop and settlement controls; both Responses modes with done-only tools; new-process account-pinned ChatGPT continuation; proactive expiry rechecks and refresh rotation; synthetic native PKCE/login/logout; all 19 MEMLAT boot cases and the Python/Rust actual-command differential.

The inspected provider, token, login and MEMLAT implementation blobs in candidate f38febd3 match that tested source. Main fd337a5f additionally changes login_cmd.rs through #889; the older receipt does not qualify that changed input handling. MEMLAT intentionally selects verified STOCK on pending-marker failure, so full failure-path parity with Python is not claimed. These are component receipts, not exact release-artifact or real-provider acceptance. A later documentation-guard queue failure does not waive artifact qualification.

| Remaining release gate | Concrete evidence still required |
|---|---|
| Native packaging, #824 | Main fd337a5f still stages Python controller payload/wrappers and declares runtime Python. Published candidate f38febd3 removes those; complete accepted final artifacts and four-boundary receipts. Python used to build a package is not an installed runtime dependency. |
| Native memory scope, #1010 | Qualify inherited no-swap or attached-scope behavior in the packaged artifact. No obsolete memory-size requirement is inferred. |
| Provider acceptance, #763 | Complete the named clean-home local, API-key and subscription tasks on the accepted artifact. Synthetic component tests do not substitute for those receipts. |
| Repository qualification, #1012 | Clear main documentation/CI coverage defects through the existing writer; no competing CI edit or test relaxation. |

Price refresh (#628) and owner LAN controls (#688) remain MISSING but excluded from the current release. Preserve them; exclusion is not deletion approval.

Remaining deletion users differ from release blockers: controller CI and the two contract runners retain Python mocks, and tiny_real retains benchmark/conversion tooling. Preserve these until their actual consumers are replaced. The native packaging candidate still consumes four inert MEMLAT templates and a license under oxc/. No module is classified UNUSED from a caller search alone.

## Python alert-file retirement audit

At source `7e6e4b04`, the Python alert rows 5–13 in [the tracked triage](security/code-scanning-triage.md) identify **four unique files**, including the extensionless Studio entry point. The supplied operator open-alert export confirms all nine IDs/locations below: 19 total rows, nine Python rows, file SHA-256 `97f9209b44aae1c1f16bf6bd88a818ba6c28706b536c93b415607e94120daeca`. The worker App still lacks alert-read permission; this is snapshot evidence, not a live API query or a new security verdict. These rows authorize no deletion or dismissal.

| File / bounded feature | Recorded alerts | Status | Replacement or retirement evidence; remaining deletion gate |
|---|---|---|---|
| `oxc/src/oxc/runtime/ctl_http.py` — legacy default controller HTTP front | 5 | DROPPED | [#681](https://github.com/oxstack/ox-llm/issues/681) / [#763](https://github.com/oxstack/ox-llm/issues/763) retire the Python controller daemon from the native default; [retirement index](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6955874053). This does not claim that in-process providers implement HTTP Host/Origin/CORS parity. `controller.py` and Python cloud/mock tests still import the front; qualify retained contracts before deleting it. The existing `oxc/tests/test_openai_api.py::KeyAndCors::test_cors_never_echoes_a_line_break_into_a_header` covers the earlier #479 fix, not Rust replacement or a fresh alert verdict. |
| `oxc/src/oxc/runtime/studio` — HTTP/UI/job/image pipeline | 11, 12, 13 | RETIREMENT INDEX PENDING | The [#951 owner decision](https://github.com/oxstack/ox-llm/issues/951#issuecomment-6959811415) retires the web UI for v0.2.x and explicitly removes Studio as a port blocker. [#1196](https://github.com/oxstack/ox-llm/issues/1196) tracks native image generation. A matching #946 retirement entry is still needed for DROPPED. The legacy Studio unit, package-data declaration, `oxc/tools/stage_v020_final.py` and Python Studio tests still consume the script. |
| `oxc/tools/session_replay.py` — HTTP recording, equal-work replay, comparison and summaries | 8, 9, 10 | DEV TOOL | [Retained contract #1119](https://github.com/oxstack/ox-llm/issues/1119). `test_session_replay.py`, `test_session_replay_output_proof.py` and `oxc/docs/HARNESSES.md` retain this workflow. Preserve the recording and replay contracts if the developer workflow is retired later. |
| `oxc/tools/interim/recording_proxy.py` — metadata recorder and listener-ownership helpers | 6, 7 | DEV TOOL | [Retained contract #1120](https://github.com/oxstack/ox-llm/issues/1120). `oxc/tools/interim/real_use_session.py`, `test_interim_adapters.py` and `test_explore1_observer.py` import it. These executable callers retain the developer workflow. |

The operator export's per-alert mapping is:

| Alert | Rule | Severity | Exported location | File disposition |
|---|---|---|---|---|
| 5 | py/http-response-splitting | medium | `oxc/src/oxc/runtime/ctl_http.py:173` | DROPPED default controller front; callers still gate deletion |
| 6 | py/http-response-splitting | medium | `oxc/tools/interim/recording_proxy.py:154` | DEV TOOL #1120 |
| 7 | py/http-response-splitting | medium | `oxc/tools/interim/recording_proxy.py:154` | DEV TOOL #1120 |
| 8 | py/http-response-splitting | medium | `oxc/tools/session_replay.py:370` | DEV TOOL #1119 |
| 9 | py/http-response-splitting | medium | `oxc/tools/session_replay.py:370` | DEV TOOL #1119 |
| 10 | py/http-response-splitting | medium | `oxc/tools/session_replay.py:374` | DEV TOOL #1119 |
| 11 | py/path-injection | high | `oxc/src/oxc/runtime/studio:309` | RETIREMENT INDEX PENDING |
| 12 | py/path-injection | high | `oxc/src/oxc/runtime/studio:309` | RETIREMENT INDEX PENDING |
| 13 | py/path-injection | high | `oxc/src/oxc/runtime/studio:311` | RETIREMENT INDEX PENDING |

MISSING blocks only the named module's deletion. None of these four files is marked PORTED or UNUSED, and none is deleted. The export does not establish any alert fixed or dismissed; all nine are open in that supplied snapshot.

### Installed payload versus build-source archive

Current inspected #824 source is `84f1928bf627ec555e22a59e6f0f9c25fff4f17e`. Its `packaging/release/stage.py` stages three native commands, four inert MEMLAT templates, licenses and the native guide; it does not stage any of the four alert files. That staging blob matches executed source `2f84770eddd55e1443b412b70fa67636c804f7e7` (SHA-256 `8ce2f62c2631d1b089882d194dbb80ec811b63e5096ecccda4b1674d48f66901`).

The [four-boundary artifact receipt](https://github.com/oxstack/ox-llm/pull/824#issuecomment-6954487105) binds runtime tarballs and both Arch variants to **2f84770e**: each inherited proof passed 15/15, native payload/manifests were checked, and Arch runtime dependencies exclude Python. [Run 44121115282](https://github.com/oxstack/ox-llm/actions/runs/44121115282) nevertheless ended FAILURE on separate real-scope setup. This is historical installed-payload evidence, not qualification of the newer 84f1928b artifact or the whole release.

The same release workflow also creates a full `git archive HEAD` build-source tarball. A source-only archive inventory at 84f1928b includes **all four alert files**. Therefore neither “absent from every release artifact” nor “alerts fixed by removal” is established. Fresh exact-head artifact qualification remains with #824; repository-source remediation/removal remains with #951 / #477.

### Current main qualification at `6ecc77a3`

The shipped legacy controller subtree has 94 Python modules and two extensionless
Python entry points under `oxc/src/oxc`. The tables above have evidence for 17 of
those paths, including bounded feature rows and the Studio retirement decision. The
inventory then listed another 78 paths without a disposition. The remaining entry
point, `oxc/src/oxc/runtime/oxc`, is invoked by the package wrapper and needs
per-command evidence. The table is still incomplete.

[#681](https://github.com/oxstack/ox-llm/issues/681) retires the Python
control and diagnostics launchers from the new default. The shipped wrappers
still invoke `runtime/oxc` and `oxc/__main__.py` respectively. The control
launcher dispatches Python subscription login and share commands. The native
sign-in replacement has its own evidence above, and #688 tracks the remaining
share work. The diagnostics launcher dispatches
`doctor`, `export-screen` and `install-plugin`. Preserve the existing
`oxc/tests/test_plugin_install.py` and developer diagnostic callers while
qualifying those contracts and locating the launcher retirement entries in
#946.

#### MEMLAT removal preparation and #1198 packaging handoff

[#1234](https://github.com/oxstack/ox-llm/pull/1234) merged at
`be0e5b82a47bf313704fbcb079ea68d26ddbb333` after
[integration CI](https://github.com/oxstack/ox-llm/actions/runs/44199296898).
The candidate deletion slice is `oxc/tools/memlat_boot.py` and
`oxc/tests/test_memlat_boot.py`. The old test is the only remaining tracked
executable importer of the file. `oxa/oxc-memlat/tests/boot.rs` covers the old
19-case suite. Preserve the captured-oracle replay, its synthetic packaging
sentinel and the separate `oxc/tools/memlat.py` builder. Update the
[legacy manual install instruction](https://github.com/oxstack/ox-llm/blob/6ecc77a3ee5b2ff1beed7b11780b5d3f22d4dc74/oxc/docs/MEMLAT.md#L43)
to point to the packaged Rust command. The four tracked MEMLAT templates
already invoke `/usr/bin/local-ai-memlat-boot`.

- The [stage rule](https://github.com/oxstack/ox-llm/blob/6ecc77a3ee5b2ff1beed7b11780b5d3f22d4dc74/packaging/release/stage.py#L19-L42)
  installs the Rust boot command and excludes the legacy boot script and test
  via the `tools` and `tests` ignore patterns. The same rule still copies the
  Python controller and creates Python wrappers on main.
- The [release archive rule](https://github.com/oxstack/ox-llm/blob/6ecc77a3ee5b2ff1beed7b11780b5d3f22d4dc74/.github/workflows/release-build.yml#L63-L75)
  uses unfiltered `git archive HEAD` for the distributed build source and both
  reproducibility inputs. A tracked archive inventory on this commit includes
  both legacy MEMLAT paths. [#1198](https://github.com/oxstack/ox-llm/pull/1198)
  needs source-archive inventory evidence alongside the installed payload
  inventory. Apply the same source selection to the distributed archive and
  reproducibility inputs, preserving the Rust builder inputs and MEMLAT templates.
  The owner-prepared release-workflow patch already uses a single filtered archive
  for the distributed source and both builds. Qualify that archive at the published
  packaging head.

Source removal remains waiting on accepted packaging and the exact
`archive/python-oxc` pre-removal commit recorded in #946.

### Additional runtime contracts at `48f6e431`

These rows add evidence for eleven runtime paths and the Flash QoS helper.
A PORTED status applies to the named contract. The owner scoped #1196 to native
image generation. Its acceptance excludes legacy editing, uploads, and web
job queues.

| Python module / feature | Status | Evidence and remaining caller gate |
|---|---|---|
| `oxc/src/oxc/runtime/client_disconnect.py` — in-process local cancellation and guest socket EOF | PORTED | `oxa/oxc-local/src/{native,transport,queue}.rs` retains engine exclusion through cancellation. `oxa/oxc-local/tests/transport.rs::cancellation_holds_engine_exclusion_until_process_exit` and `oxa/oxc-gateway/tests/share_listener.rs::disconnect_cancels_a_silent_backend_and_waits_for_its_cleanup` exercise the native boundaries. The legacy HTTP watcher still has its own request-parser, keep-alive and controller comparison tests in `test_client_disconnect.py` and `test_controller_disconnect_http.py`. |
| `oxc/src/oxc/runtime/exec_in_scope.py` — owned local engine zero-swap admission | PORTED | [#1011](https://github.com/oxstack/ox-llm/pull/1011) adds the Rust scope path. `oxa/oxc-local/src/platform.rs` tests inherited and attached cgroups, unavailable systemd and the zero-swap scope request. [#1010](https://github.com/oxstack/ox-llm/issues/1010) retains the exact packaged-artifact proof. The Python wrapper still enforces explicit MemoryMax and a ready/go handshake for the image worker. Preserve `test_flash_memory.py` and `test_image_npu_worker.py` until those callers have a disposition. |
| `oxc/src/oxc/runtime/request_ledger.py` — native paid request settlement and salted identifiers | PORTED | `oxa/oxc-gateway/src/ledger.rs` and `tests/ledger.rs::keyed_identifiers_are_stable_separated_and_no_request_content_is_stored`, `daily_handles_share_admission_and_record_one_row_before_releasing_each_hold`. The legacy module still serves day digests and cache targeting through `health_digest.py`, `ctl_cache.py`, `ctl_proxy_ledger.py`, `ctl_proxy_prep.py` and `ctl_routes.py`. Preserve `test_request_ledger.py` and `test_request_ledger_origin.py` until legacy readers and session hints are qualified. |
| `oxc/src/oxc/runtime/state_cache.py` — owner cache budget and scoped clears | MISSING | Existing cache migration issue [#222](https://github.com/oxstack/ox-llm/issues/222). `ctl_cache.py` handles budget, usage and clears, `ctl_routes.py` validates scoped clears, and `oxe_runtime.py` selects stores. Native backend cache and prefix storage alone do not cover those owner controls. Keep `test_state_cache.py`, `test_prefix_cache.py` and `test_flash_prefix_store.py`. |
| `oxc/src/oxc/runtime/studio_page.py` — embedded Studio web page | RETIREMENT INDEX PENDING | The `studio` script imports `PAGE`; the [#951 owner decision](https://github.com/oxstack/ox-llm/issues/951#issuecomment-6959811415) retires the web UI for v0.2.x. Record the matching #946 entry and resolve the legacy Studio callers before deletion. |
| `oxc/src/oxc/runtime/studio_context.py` — Python Studio namespace adapter | RETIREMENT INDEX PENDING | `studio`, the planners, the worker and the oxi adapter use the shared namespace. The same #951 web UI retirement applies to the adapter. Keep it with its callers until their dispositions and the #946 entry are recorded. |
| `oxc/src/oxc/runtime/studio_pixels.py` — Studio image and mask transforms | RETIREMENT INDEX PENDING | The `studio` caller uses the legacy PNG, mask and crop helpers. `test_studio_crop_edit.py` covers the masked-edit boundary, outside [#1196](https://github.com/oxstack/ox-llm/issues/1196). Record disposition for the Studio-only transforms before removing their source and fixtures. |
| `oxc/src/oxc/runtime/studio_plan_edit.py` — Studio FLUX.2 edit and generation planning | MISSING | `studio` imports the planner; `test_studio_edit.py` and `test_studio_klein.py` cover legacy modes. Carry the generation behavior named in [#1196](https://github.com/oxstack/ox-llm/issues/1196). Editing is outside that scope and needs a retirement entry for this slice. |
| `oxc/src/oxc/runtime/studio_plan_generate.py` — Studio generation, upscale and refine planning | MISSING | Existing product replacement [#1196](https://github.com/oxstack/ox-llm/issues/1196). `studio` imports the planner; `test_studio_upscale.py` exercises the legacy upscale path. Scope the native image behavior and index any retired planning modes. |
| `oxc/src/oxc/runtime/studio_oxi.py` — Studio native image adapter and legacy fallback | MISSING | Existing product replacement [#1196](https://github.com/oxstack/ox-llm/issues/1196). `studio` imports the adapter, and `test_studio_oxi.py` covers route and fallback behavior. Native acceptance explicitly excludes silent fallback. Preserve the legacy test while its caller remains. |
| `oxc/src/oxc/runtime/studio_worker.py` — legacy Studio job and image-worker lifecycle | MISSING | Existing product replacement [#1196](https://github.com/oxstack/ox-llm/issues/1196). `studio` invokes the worker, and `test_studio_handoff.py` exercises the worker boundary. Track any carried image-worker admission and cleanup behavior and any retired web-job orchestration. |
| `oxc/packaging/flash-qos/oxc-flash-qos` — Flash idle-QoS helper | UNQUALIFIED | Main stages this extensionless Python script via the broad controller copy. The legacy unit and Flash stage use it, and `test_flash_qos_helper.py` and `test_flash_qos_install_user.py` retain its contract. [#1198](https://github.com/oxstack/ox-llm/pull/1198) excludes the helper from the installed candidate. Source retirement still needs an explicit feature decision and index entry. |

### Published packaging artifacts at `67b999ec`

[#1198](https://github.com/oxstack/ox-llm/pull/1198) now publishes the filtered
archive rule. [Release run 44206046295](https://github.com/oxstack/ox-llm/actions/runs/44206046295)
built both CPU artifacts from `cca8f4f0cf39b4ef8869880659c6b9bbb1cf9c03`, the
test merge of packaging head `67b999ec` into `0095e879`. Both uploaded artifacts
passed their checksum manifests and contained byte-identical build-source
archives with SHA-256
`5bc840e55d12f119a0f5dad991db6c9a8a6f82b4f02193af5e537fc54f483b45`.
Their file lists exclude `oxc/src`, `oxc/tools`, `oxc/tests`, the Flash QoS
script and all `oxc/**/*.py`. They retain the MEMLAT templates, the native
crate and the captured oracle. Both installed payload tarballs contain the
native MEMLAT command and exclude Python files and the legacy controller
wrappers. The included no-Python receipts bind their payload digests to that
same source commit.

The [Arch job](https://github.com/oxstack/ox-llm/actions/runs/44206046295/job/134480040168)
failed after installing the binary package. Its smoke probe at
[`packaging/release/smoke.py:49`](https://github.com/oxstack/ox-llm/blob/67b999ecbf0b52a401b6df9b85a2b5d1c46071d9/packaging/release/smoke.py#L46-L52)
expects `oxe --help` to exit zero. The existing CLI prints its usage and
exits 2. The package already tests `oxe --version` and the next step tests
`oxe gen --check-args`. Keep the ELF assertion for every binary and use the
existing success probes for `oxe` in the Arch smoke. Arch acceptance and the
exact pre-removal archive are still pending. The CPU artifact inventories
qualify only those two uploaded artifact boundaries.

### Local engine ownership at `5cbccf25`

The native local engine evidence below applies to the named behavior in each
row. The Python controller still owns its public HTTP, mixed backend and
panel contracts. Preserve the listed callers and tests until their own
dispositions are recorded.

| Python module / feature | Status | Evidence and remaining caller gate |
|---|---|---|
| `oxc/src/oxc/runtime/ctl_lifecycle.py` — native child load, readiness and unload | PORTED | [#693](https://github.com/oxstack/ox-llm/pull/693) implements native ownership in `oxa/oxc-local/src/lifecycle.rs`. `tests/lifecycle.rs::load_requires_memory_and_holds_lease_through_readiness_and_unload` proves the bounded contract. The legacy mixin still handles engine-specific adapters, idle QoS and diagnostics for `Manager`. |
| `oxc/src/oxc/runtime/ctl_manager.py` — single owned local child and request lifecycle | PORTED | [#732](https://github.com/oxstack/ox-llm/pull/732) wires the typed factory in `oxa/oxc-local/src/configured.rs`. `oxa/oxa-cli/tests/in_process_local.rs` checks explicit selection and no HTTP fallback on invalid enabled configuration. The Python manager composes the legacy status, model, cache and lease mixins for `controller.py`. |
| `oxc/src/oxc/runtime/ctl_engines.py` — native local launch and child readiness | PORTED | [#686](https://github.com/oxstack/ox-llm/pull/686) maps native launch profiles and [#693](https://github.com/oxstack/ox-llm/pull/693) owns readiness. `oxa/oxc-local/tests/{launch_profile,supervision}.rs` check invalid profiles and failed readiness. `ctl_lifecycle.py` still consumes the mixed adapters. Preserve `test_oxe_production.py` and `test_flash_oxe_production.py`. |
| `oxc/src/oxc/runtime/ctl_queue.py` — bounded FIFO admission and cancellation | PORTED | [#626](https://github.com/oxstack/ox-llm/pull/626), `oxa/oxc-local/src/queue.rs` and `tests/queue.rs::fifo_and_cancellation_do_not_let_newcomers_jump_a_waiter` cover native queue fairness. The controller still exposes the separate early SSE queue contract in `test_controller_queue.py`. |
| `oxc/src/oxc/runtime/ctl_lease.py` — native text-engine lease | PORTED | `oxa/oxc-local/tests/lifecycle.rs::admitted_ticket_covers_cold_load_and_generation_without_a_fifo_gap` exercises queue ownership through cold load and generation. The legacy cross-process image reservation is still exercised by `test_studio_handoff.py`. Native image admission belongs to [#1196](https://github.com/oxstack/ox-llm/issues/1196). |
| `oxc/src/oxc/runtime/ctl_models.py` — native local profile selection without probing | PORTED | `oxa/oxa-cli/tests/in_process_local.rs::native_local_discovery_is_user_only_non_launching_and_allowed_offline` and `oxa/oxc-local/tests/launch_profile.rs` cover typed selection. The Python mixin still reads and writes legacy controller model and context selections. Preserve `test_selection_status.py` and the #222 harness primer boundary. |
| `oxc/src/oxc/runtime/ctl_status.py` — typed native local memory and lifecycle status | PORTED | `oxa/oxc-local/src/native.rs` exercises status for the owned model and refuses unrelated model status. The legacy `/_local/status` route and panel read a larger schema via this mixin. Preserve `test_oxc_status_cli.py` and its controller fixtures. |
| `oxc/src/oxc/runtime/keep_warm.py` — idle unload and memory-pressure stop for the native child | PORTED | `oxa/oxc-local/tests/lifecycle.rs::idle_unload_does_not_interrupt_a_request_and_starts_at_release` and `idle_disabled_keeps_engine_but_not_under_memory_faults` cover the bounded behavior. Legacy PSI some, grace and confirmation semantics remain in `test_keep_warm.py`; native guard samples PSI full. |
| `oxc/src/oxc/runtime/memory_policy.py` — native admission and fail-closed memory stop | PORTED | [#780](https://github.com/oxstack/ox-llm/pull/780) and [#1011](https://github.com/oxstack/ox-llm/pull/1011) manage native allocation budgeting and child scope checks. `oxa/oxc-local/tests/lifecycle.rs` covers unavailable memory and in-flight stop. Legacy Flash caps and the image-worker handshake retain `test_memory_policy.py` and `test_flash_memory.py`. |
| `oxc/src/oxc/runtime/profile_policy.py` — native per-model launch tuning and MTP cutoffs | PORTED | [#767](https://github.com/oxstack/ox-llm/pull/767) and `oxa/oxc-local/tests/launch_profile.rs::model_mtp_settings_override_defaults_without_changing_other_models` cover native launch profiles. `ctl_models.py` and `ctl_cache.py` still use legacy context modes and Flash limits validated by `test_profile_policy.py`. |
| `oxc/src/oxc/runtime/oxe_runtime.py` — native model launch and pre-spawn identity checks | PORTED | [#774](https://github.com/oxstack/ox-llm/pull/774) supplies native binary and weight preflight. `oxa/oxc-local/tests/launch_profile.rs::profile_cannot_override_binding_model_or_alias_through_extra_arguments` covers containment of user launch arguments. Legacy Flash cache and power policy still run through `ctl_engines.py` and `test_flash_oxe_production.py`. |
| `oxc/src/oxc/runtime/exec_no_thp.py` — legacy Flash candidate process wrapper | UNQUALIFIED | `bigmoe_runtime.py` resolves this sibling file to pin the executable and apply the candidate process THP policy. `test_exec_no_thp.py` checks refusal and execution semantics. [#858](https://github.com/oxstack/ox-llm/issues/858) tracks native V-cache THP work. Verify which candidate wrapper contract is retained before retiring this source. |
| `oxc/src/oxc/runtime/flash_cache_pref.py` — legacy Flash cache tier owner control | MISSING | `ctl_cache.py`, `model_catalog.py` and `oxe_runtime.py` read and present its owner preference. Preserve `test_flash_cache_pref.py` and `test_flash_cache_setting.py`. [#222](https://github.com/oxstack/ox-llm/issues/222) covers moving cache management behind the engine API. The chosen tier and owner setting still need an explicit disposition. |
| `oxc/src/oxc/runtime/oxc` — default legacy control launcher | RETIREMENT INDEX PENDING | [#681](https://github.com/oxstack/ox-llm/issues/681) retires the Python launcher from the default. Keep its controller callers until their feature gates have cleared. Native sign-in is mapped above; share is tracked in #688, cache controls in #222 and image generation in #1196. Stage rollback and uninstall remain separate command contracts. |

### Controller proxy, cache and image boundaries at `3b6719cd`

The legacy controller is still exercised by the lab and comparison runners.
The typed local and cloud paths cover the behaviors named below. Legacy
server routes, telemetry, cache operations and fixtures retain their own
caller gates.

| Python module / feature | Status | Evidence and remaining caller gate |
|---|---|---|
| `oxc/src/oxc/runtime/controller.py` — default packaged Python daemon | RETIREMENT INDEX PENDING | [#681](https://github.com/oxstack/ox-llm/issues/681) retires the default daemon. The #946 entry above covers the HTTP front specifically. The controller still supplies the model manager and routes to the lab and both comparison runners. Record the daemon entry with its callers before removal. |
| `oxc/src/oxc/runtime/ctl_proxy.py` — typed native request and tool-result forwarding | PORTED | `oxa/oxc-local/tests/transport.rs::typed_tool_result_continues_the_same_owned_engine_without_a_controller` covers native local continuation. `controller.py` still uses the Python proxy for request defaults and all legacy routes; keep `test_v013_proxy_contract.py`. |
| `oxc/src/oxc/runtime/ctl_proxy_prep.py` — legacy request normalization and backend hints | MISSING | [#222](https://github.com/oxstack/ox-llm/issues/222) scopes request normalization. This preparation stage still selects the Responses or Chat shim and attaches the legacy cache hints. Preserve `test_openai_conformance.py` and `test_cache_perfect_p1.py`. |
| `oxc/src/oxc/runtime/ctl_proxy_stream.py` — typed local reply streaming | PORTED | `oxa/oxc-local/tests/transport.rs::typed_request_streams_tools_usage_and_text_without_controller_routes` exercises the native stream. The Python proxy still handles HTTP response classification and forwarding with the legacy disconnect watcher. Keep the controller comparison and `test_controller_disconnect_http.py`. |
| `oxc/src/oxc/runtime/ctl_proxy_ledger.py` — native admission and paid settlement | PORTED | `oxa/oxc-local/tests/lifecycle.rs::admitted_ticket_covers_cold_load_and_generation_without_a_fifo_gap` and `oxa/oxc-gateway/tests/ledger.rs::daily_handles_share_admission_and_record_one_row_before_releasing_each_hold` cover native ownership. The Python controller still consumes its request ledger and post-response diagnostics through this stage. |
| `oxc/src/oxc/runtime/ctl_cloud.py` — typed native cloud route and cancellation | PORTED | `oxa/oxa-engine/tests/embedded_runtime.rs::cancellation_wakes_a_live_header_wait_and_settles_before_returning` covers cancellation and settlement through the typed bridge. The Python proxy still invokes its provider-specific stage and uses `test_cloud_conformance.py` and its accounting tests. |
| `oxc/src/oxc/runtime/cloud.py` — configured native catalog and remote budget admission | PORTED | `oxa/oxc-gateway/tests/catalog.rs` and `tests/runtime_openai.rs::subscription_owns_an_account_bound_handle_and_zero_usd_settlement_enforces_request_caps` check the native component boundary. `ctl_cloud.py` still uses Python listing, policy and routing. Live refresh is tracked under [#628](https://github.com/oxstack/ox-llm/issues/628). |
| `oxc/src/oxc/runtime/ctl_routes.py` — default legacy HTTP control routes | RETIREMENT INDEX PENDING | [#681](https://github.com/oxstack/ox-llm/issues/681) removes the Python daemon from the default. Its control route module still services the legacy launcher, UI and `test_controller_contract.py`. Scope retained commands and record the index entry. |
| `oxc/src/oxc/runtime/ctl_seams.py` — legacy controller namespace adapter | RETIREMENT INDEX PENDING | `controller.py` binds the namespace used by the split proxy modules and patch-based lab tests. Track it with the default daemon retirement and retire or replace those consumers together. |
| `oxc/src/oxc/runtime/ctl_replay.py` — duplicate request replay after a cut HTTP stream | UNQUALIFIED | `test_v014_core_robust.py` preserves the legacy contract. `oxa/oxc-local/tests/transport.rs::refusal_and_cut_stream_are_not_replayed_and_keep_cleanup_owned` deliberately refuses native replay. Record which duplicate-request behavior is retired before source removal. |
| `oxc/src/oxc/runtime/ctl_diag.py` — legacy turn diagnostics | UNQUALIFIED | `ctl_proxy_ledger.py` and `ctl_proxy_stream.py` use the diagnostic module, exercised by `test_v014_core_robust.py`. Native settlement evidence covers a different contract. Record the diagnostic disposition with the controller callers. |
| `oxc/src/oxc/runtime/ctl_cache.py` — cache administration and learned primer controls | MISSING | [#222](https://github.com/oxstack/ox-llm/issues/222) scopes cache and primer migration. The manager and HTTP control routes call the mixin. Keep `test_state_cache.py` and `test_prefix_cache.py` until budget, clears and primer callers have explicit dispositions. |
| `oxc/src/oxc/runtime/prefix_plan.py` — legacy learned-primer construction | MISSING | [#222](https://github.com/oxstack/ox-llm/issues/222). `ctl_cache.py` builds primer requests and `test_prefix_cache.py` checks their prefix-only behavior. Native token reuse is separate from scheduling a learned primer. |
| `oxc/src/oxc/runtime/prefix_store.py` — legacy controller prefix-state store | MISSING | [#222](https://github.com/oxstack/ox-llm/issues/222). The controller launches backends with store paths from `ctl_engines.py` and `ctl_cache.py`. Preserve `test_prefix_cache.py` and `test_flash_prefix_store.py` while store ownership and compatibility are unresolved. |
| `oxc/src/oxc/runtime/harness_registry.py` — learned harness metadata for prefix preparation | MISSING | [#222](https://github.com/oxstack/ox-llm/issues/222). `ctl_models.py`, `ctl_proxy_prep.py` and `ctl_routes.py` retain observation and preparation; `test_cache_perfect_p1.py` exercises the legacy integration. |
| `oxc/src/oxc/runtime/harness_compat.py` — legacy HTTP harness request normalization | MISSING | [#222](https://github.com/oxstack/ox-llm/issues/222). `ctl_proxy_prep.py` and `openai_conformance.py` use the adapter. Preserve `test_harness_compat.py` until retained normalization is qualified. |
| `oxc/src/oxc/runtime/cache_turn.py` — legacy turn labels and backend cache hints | MISSING | [#222](https://github.com/oxstack/ox-llm/issues/222). `ctl_proxy_prep.py` and `request_ledger.py` use turn labels and snapshot hints. Keep the related `test_cache_m2.py` and `test_cache_perfect_p1.py` coverage. |
| `oxc/src/oxc/runtime/cache_miss.py` — legacy numeric cache-miss telemetry | UNQUALIFIED | `cache_turn.py` and `request_ledger.py` classify cache misses. `test_cache_miss.py` and `test_cache_m2.py` cover classification. Record the telemetry disposition separately from native engine cache reuse. |
| `oxc/src/oxc/runtime/openai_api.py` — server-side key, CORS and error contracts | RETIREMENT INDEX PENDING | The default daemon retirement in [#681](https://github.com/oxstack/ox-llm/issues/681) applies to the installed server path. `ctl_models.py`, the controller and `test_openai_api.py` still use this module. The native guest listener has its own #688 boundary. |
| `oxc/src/oxc/runtime/openai_conformance.py` — legacy chat server normalization and event adapter | MISSING | [#222](https://github.com/oxstack/ox-llm/issues/222) covers request shaping. The Python proxy instantiates `ChatShim`. Preserve `test_openai_conformance.py` and `test_thinking_budget.py`; native client tests exercise an upstream boundary. |
| `oxc/src/oxc/runtime/openai_responses.py` — legacy public Responses server shim | RETIREMENT INDEX PENDING | `ctl_proxy_prep.py` instantiates `ResponsesShim`, tested by `test_openai_conformance.py`. The native client consumes upstream Responses. Record the legacy server fixture disposition with the default daemon retirement before removing it. |
| `oxc/src/oxc/runtime/image_npu.py` — legacy NPU image admission policy | MISSING | [#1196](https://github.com/oxstack/ox-llm/issues/1196) requires native image memory admission. `ctl_lease.py` and `image_npu_worker.py` consume Python policy; `test_controller_image_slot.py` covers its legacy integration. |
| `oxc/src/oxc/runtime/image_npu_worker.py` — legacy image admission and worker cleanup | MISSING | [#1196](https://github.com/oxstack/ox-llm/issues/1196) scopes the retained native image lifecycle. `studio_worker.py` invokes the Python worker, and `test_image_npu_worker.py` and `test_v013_image_watchdog.py` cover worker admission and cleanup. |
| `oxc/src/oxc/runtime/image_slot.py` — legacy controller image reservation | MISSING | [#1196](https://github.com/oxstack/ox-llm/issues/1196) requires native image resource admission. `ctl_manager.py` and Studio use inherited image leases. Preserve `test_image_slot.py` and `test_controller_image_slot.py` until treatment of this legacy lease is recorded. |
| `oxc/src/oxc/runtime/image_size_gate.py` — Studio per-size qualification receipts | UNQUALIFIED | The Studio router consumes the legacy per-size gate tested by `test_v013_image_size_gate.py`. [#1196](https://github.com/oxstack/ox-llm/issues/1196) specifies native size validation and deterministic fixture coverage. Record the old routing and qualification fixture disposition separately. |

## Complete module inventory awaiting feature-level qualification

Each path below needs a ledger disposition with its own evidence; presence here is neither UNUSED nor a deletion approval. Move a module into the table only after checking every retained feature and its callers. Keep existing #681 retirement decisions distinct from actual Rust replacement coverage. The provider, MEMLAT, retired installer/bootstrap and alert-file rows already mapped above are omitted here. Partial-feature rows do not qualify a whole module. The extensionless control and Studio launchers have bounded rows above.

- `oxc/benchmarks/classic/loopback_bridge.py`
- `oxc/benchmarks/classic/screen.py`
- `oxc/benchmarks/classic/screen/checks.py`
- `oxc/benchmarks/classic/screen/functional.py`
- `oxc/benchmarks/classic/test_screen.py`
- `oxc/benchmarks/fixtures/reservations/inventory.py`
- `oxc/benchmarks/fixtures/reservations/reservations.py`
- `oxc/benchmarks/fixtures/reservations/test_reservations.py`
- `oxc/benchmarks/fixtures/reservations/validation.py`
- `oxc/benchmarks/req_path/stub_bench.py`
- `oxc/benchmarks/roofline/flash_nvme.py`
- `oxc/benchmarks/roofline/icc_sample.py`
- `oxc/benchmarks/roofline/nvme_read.py`
- `oxc/benchmarks/roofline/run.py`
- `oxc/benchmarks/roofline/summarize_attention.py`
- `oxc/benchmarks/roofline/summarize_integrated_server.py`
- `oxc/benchmarks/roofline/summarize_integrated_upgrade.py`
- `oxc/benchmarks/roofline/summarize_prefix_fairness.py`
- `oxc/benchmarks/tokens/token_harness.py`
- `oxc/src/oxc/__init__.py`
- `oxc/src/oxc/__main__.py`
- `oxc/src/oxc/batch_runner.py`
- `oxc/src/oxc/devfreq_probe.py`
- `oxc/src/oxc/doctor.py`
- `oxc/src/oxc/engine_report.py`
- `oxc/src/oxc/flash_profile.py`
- `oxc/src/oxc/legacy_config.py`
- `oxc/src/oxc/platform_support.py`
- `oxc/src/oxc/plugin_install.py`
- `oxc/src/oxc/power.py`
- `oxc/src/oxc/results.py`
- `oxc/src/oxc/runtime/__init__.py`
- `oxc/src/oxc/runtime/bigmoe_adapter.py`
- `oxc/src/oxc/runtime/bigmoe_runtime.py`
- `oxc/src/oxc/runtime/classic_beta.py`
- `oxc/src/oxc/runtime/ctl_env.py`
- `oxc/src/oxc/runtime/ctl_host.py`
- `oxc/src/oxc/runtime/ctl_repack.py`
- `oxc/src/oxc/runtime/engine_diagnostics.py`
- `oxc/src/oxc/runtime/engine_ledger.py`
- `oxc/src/oxc/runtime/health_digest.py`
- `oxc/src/oxc/runtime/model_catalog.py`
- `oxc/src/oxc/runtime/release_update.py`
- `oxc/src/oxc/runtime/slim_request.py`
- `oxc/src/oxc/runtime/storage_view.py`
- `oxc/src/oxc/runtime/uninstall.py`
- `oxc/src/oxc/source_archive.py`
- `oxc/src/oxc/system_counters.py`
- `oxc/tests/_isolation.py`
- `oxc/tests/_stub_manager.py`
- `oxc/tests/conftest.py`
- `oxc/tests/fixtures/stub_backend/llama_stub.py`
- `oxc/tests/flash_profile_fixture.py`
- `oxc/tests/mock_cloud.py`
- `oxc/tests/openai_api_stack.py`
- `oxc/tests/oxe_profile_fixture.py`
- `oxc/tests/session_replay_mock.py`
- `oxc/tests/stage_layout_fixture.py`
- `oxc/tests/test_000_isolation.py`
- `oxc/tests/test_backend_identity.py`
- `oxc/tests/test_backend_port_preflight.py`
- `oxc/tests/test_backend_token.py`
- `oxc/tests/test_beta_selector.py`
- `oxc/tests/test_bootstrap.py`
- `oxc/tests/test_bootstrap_build.py`
- `oxc/tests/test_bootstrap_checks.py`
- `oxc/tests/test_bootstrap_download.py`
- `oxc/tests/test_bootstrap_install.py`
- `oxc/tests/test_bootstrap_manifest_pins.py`
- `oxc/tests/test_bootstrap_sources.py`
- `oxc/tests/test_bootstrap_transfer.py`
- `oxc/tests/test_cache_breaker_a.py`
- `oxc/tests/test_cache_contract.py`
- `oxc/tests/test_cache_m2.py`
- `oxc/tests/test_cache_miss.py`
- `oxc/tests/test_cache_perfect_p1.py`
- `oxc/tests/test_client_disconnect.py`
- `oxc/tests/test_cloud_accounting_review.py`
- `oxc/tests/test_cloud_anthropic.py`
- `oxc/tests/test_cloud_anthropic_identity.py`
- `oxc/tests/test_cloud_cancel.py`
- `oxc/tests/test_cloud_cancel_cost.py`
- `oxc/tests/test_cloud_chatgpt.py`
- `oxc/tests/test_cloud_conformance.py`
- `oxc/tests/test_cloud_only_install.py`
- `oxc/tests/test_cloud_only_no_engine.py`
- `oxc/tests/test_cloud_openai.py`
- `oxc/tests/test_cloud_prices.py`
- `oxc/tests/test_cloud_pricing.py`
- `oxc/tests/test_cloud_remote.py`
- `oxc/tests/test_cloud_residuals.py`
- `oxc/tests/test_cloud_review_limits.py`
- `oxc/tests/test_cloud_review_security.py`
- `oxc/tests/test_cloud_secrets.py`
- `oxc/tests/test_cloud_stream_order.py`
- `oxc/tests/test_controller_behaviour.py`
- `oxc/tests/test_controller_contract.py`
- `oxc/tests/test_controller_disconnect_http.py`
- `oxc/tests/test_controller_export.py`
- `oxc/tests/test_controller_image_slot.py`
- `oxc/tests/test_controller_last_error.py`
- `oxc/tests/test_controller_n6.py`
- `oxc/tests/test_controller_path_canonical.py`
- `oxc/tests/test_controller_queue.py`
- `oxc/tests/test_controller_reaper_cleanup.py`
- `oxc/tests/test_controller_recovery.py`
- `oxc/tests/test_controller_security.py`
- `oxc/tests/test_controller_stop_errors.py`
- `oxc/tests/test_core_client_neutral.py`
- `oxc/tests/test_devfreq_probe.py`
- `oxc/tests/test_doc_links.py`
- `oxc/tests/test_doctor.py`
- `oxc/tests/test_engine_diagnostics.py`
- `oxc/tests/test_engine_fit.py`
- `oxc/tests/test_engine_ledger.py`
- `oxc/tests/test_engine_oracle.py`
- `oxc/tests/test_env_defaults.py`
- `oxc/tests/test_exec_no_thp.py`
- `oxc/tests/test_explore1_observer.py`
- `oxc/tests/test_export_receipt.py`
- `oxc/tests/test_flash_admission_rule.py`
- `oxc/tests/test_flash_cache_pref.py`
- `oxc/tests/test_flash_cache_setting.py`
- `oxc/tests/test_flash_idle_qos.py`
- `oxc/tests/test_flash_memory.py`
- `oxc/tests/test_flash_oxe_production.py`
- `oxc/tests/test_flash_power_degraded.py`
- `oxc/tests/test_flash_pref_controller.py`
- `oxc/tests/test_flash_pref_swap.py`
- `oxc/tests/test_flash_prefix_store.py`
- `oxc/tests/test_flash_profile.py`
- `oxc/tests/test_flash_qos_helper.py`
- `oxc/tests/test_flash_qos_install_user.py`
- `oxc/tests/test_flash_qualification.py`
- `oxc/tests/test_flash_transport.py`
- `oxc/tests/test_guard2.py`
- `oxc/tests/test_guard_reason_text.py`
- `oxc/tests/test_harness_compat.py`
- `oxc/tests/test_harness_probe.py`
- `oxc/tests/test_health_digest.py`
- `oxc/tests/test_image_npu.py`
- `oxc/tests/test_image_npu_worker.py`
- `oxc/tests/test_image_slot.py`
- `oxc/tests/test_install_staged_release.py`
- `oxc/tests/test_interim_adapters.py`
- `oxc/tests/test_interim_codex_controls.py`
- `oxc/tests/test_interim_sessions.py`
- `oxc/tests/test_keep_warm.py`
- `oxc/tests/test_lan_share.py`
- `oxc/tests/test_ledger_history.py`
- `oxc/tests/test_ledger_salt_stable.py`
- `oxc/tests/test_legacy_config_migration.py`
- `oxc/tests/test_long_session.py`
- `oxc/tests/test_loopback_bridge.py`
- `oxc/tests/test_mac_reproduce.py`
- `oxc/tests/test_memlat.py`
- `oxc/tests/test_memlat_engine_override.py`
- `oxc/tests/test_memlat_shutdown.py`
- `oxc/tests/test_memory_policy.py`
- `oxc/tests/test_metadata_catalog.py`
- `oxc/tests/test_nvme_read.py`
- `oxc/tests/test_oem_drills.py`
- `oxc/tests/test_openai_api.py`
- `oxc/tests/test_openai_conformance.py`
- `oxc/tests/test_oxc_status_cli.py`
- `oxc/tests/test_oxc_ui_data.py`
- `oxc/tests/test_oxe_production.py`
- `oxc/tests/test_oxe_runtime.py`
- `oxc/tests/test_panel_model.py`
- `oxc/tests/test_panel_preview.py`
- `oxc/tests/test_panel_share_poll.py`
- `oxc/tests/test_panel_v013_model.py`
- `oxc/tests/test_panel_v013_states.py`
- `oxc/tests/test_plugin_install.py`
- `oxc/tests/test_plugin_install_v013.py`
- `oxc/tests/test_post_install_regression.py`
- `oxc/tests/test_power.py`
- `oxc/tests/test_prefix_cache.py`
- `oxc/tests/test_prereview_fixes.py`
- `oxc/tests/test_profile_policy.py`
- `oxc/tests/test_public_results.py`
- `oxc/tests/test_reasoning_copy_metrics.py`
- `oxc/tests/test_release_scrub.py`
- `oxc/tests/test_release_update.py`
- `oxc/tests/test_remote_messages.py`
- `oxc/tests/test_render_units.py`
- `oxc/tests/test_repack_cache_notify.py`
- `oxc/tests/test_request_ledger.py`
- `oxc/tests/test_request_ledger_origin.py`
- `oxc/tests/test_run_lab_tests_argv.py`
- `oxc/tests/test_run_synthetic_screen.py`
- `oxc/tests/test_runtime_config_write.py`
- `oxc/tests/test_runtime_examples.py`
- `oxc/tests/test_runtime_sources.py`
- `oxc/tests/test_runtime_version.py`
- `oxc/tests/test_rust_engine_default.py`
- `oxc/tests/test_selection_status.py`
- `oxc/tests/test_session_history_stability.py`
- `oxc/tests/test_session_replay.py`
- `oxc/tests/test_session_replay_output_proof.py`
- `oxc/tests/test_source_archive.py`
- `oxc/tests/test_stage_rollback_guard.py`
- `oxc/tests/test_stage_v020_final.py`
- `oxc/tests/test_state_cache.py`
- `oxc/tests/test_storage_cleanup.py`
- `oxc/tests/test_studio_crop_edit.py`
- `oxc/tests/test_studio_edit.py`
- `oxc/tests/test_studio_handoff.py`
- `oxc/tests/test_studio_klein.py`
- `oxc/tests/test_studio_oxi.py`
- `oxc/tests/test_studio_security.py`
- `oxc/tests/test_studio_upscale.py`
- `oxc/tests/test_swap_tolerant_counter_stop.py`
- `oxc/tests/test_swap_tolerant_guard.py`
- `oxc/tests/test_system_counters.py`
- `oxc/tests/test_thinking_budget.py`
- `oxc/tests/test_tiny_real_compare.py`
- `oxc/tests/test_tiny_real_convert.py`
- `oxc/tests/test_tiny_real_oxa_loop.py`
- `oxc/tests/test_tools_long_session_shapes.py`
- `oxc/tests/test_tools_model_inspect.py`
- `oxc/tests/test_tools_offline_analyses.py`
- `oxc/tests/test_tools_organic_report.py`
- `oxc/tests/test_tools_tune_from_ledger.py`
- `oxc/tests/test_trial_status.py`
- `oxc/tests/test_ui_data.py`
- `oxc/tests/test_uninstall.py`
- `oxc/tests/test_unload_after_reply.py`
- `oxc/tests/test_v013_begin_golden.py`
- `oxc/tests/test_v013_image_size_gate.py`
- `oxc/tests/test_v013_image_slot.py`
- `oxc/tests/test_v013_image_watchdog.py`
- `oxc/tests/test_v013_integration.py`
- `oxc/tests/test_v013_modes.py`
- `oxc/tests/test_v013_proxy_contract.py`
- `oxc/tests/test_v013_studio_contract.py`
- `oxc/tests/test_v014_core_robust.py`
- `oxc/tests/test_v014_split2_oddities.py`
- `oxc/tests/test_v020_cancel_settle.py`
- `oxc/tests/test_v020_capabilities.py`
- `oxc/tests/test_v020_preempt.py`
- `oxc/tests/test_v020_rig.py`
- `oxc/tests/test_v020_rig_prereview.py`
- `oxc/tests/test_verify_flash_source.py`
- `oxc/tests/test_w4d_endpoints.py`
- `oxc/tests/test_zswap_setup.py`
- `oxc/tests/test_zzz_production_sentinel.py`
- `oxc/tools/cache_breaker_a.py`
- `oxc/tools/cache_contract.py`
- `oxc/tools/cache_miss_eval.py`
- `oxc/tools/ci_test_shard.py`
- `oxc/tools/desktop_memory_sampler.py`
- `oxc/tools/devfreq_trace.py`
- `oxc/tools/engine_oracle.py`
- `oxc/tools/exec_no_thp.py`
- `oxc/tools/export_receipt.py`
- `oxc/tools/harness_probe.py`
- `oxc/tools/install_staged_release.py`
- `oxc/tools/interim/flash_smoke.py`
- `oxc/tools/interim/keepwarm_fixture.py`
- `oxc/tools/interim/real_use_session.py`
- `oxc/tools/interim/rehearse_interim.py`
- `oxc/tools/interim/soak_slope.py`
- `oxc/tools/interim/thermal_aa.py`
- `oxc/tools/interim/tool_counters.py`
- `oxc/tools/long_session.py`
- `oxc/tools/mac_reproduce.py`
- `oxc/tools/memlat.py`
- `oxc/tools/memlat_shutdown.py`
- `oxc/tools/model_inspect.py`
- `oxc/tools/oem_drills.py`
- `oxc/tools/offline_analyses.py`
- `oxc/tools/openai_api_check.py`
- `oxc/tools/organic_report.py`
- `oxc/tools/post_install_regression.py`
- `oxc/tools/reasoning_copy_metrics.py`
- `oxc/tools/render_runtime_examples.py`
- `oxc/tools/render_units.py`
- `oxc/tools/repack_cache.py`
- `oxc/tools/run_synthetic_screen.py`
- `oxc/tools/scrub_receipts.py`
- `oxc/tools/session_history_stability.py`
- `oxc/tools/stage_rollback_guard.py`
- `oxc/tools/stage_shared.py`
- `oxc/tools/stage_sources.py`
- `oxc/tools/stage_unit.py`
- `oxc/tools/stage_v020_final.py`
- `oxc/tools/storage_cleanup.py`
- `oxc/tools/studio_mask_check.py`
- `oxc/tools/system_counters.py`
- `oxc/tools/tiny_real/compare_logits.py`
- `oxc/tools/tiny_real/convert_dense_to_qwen35moe.py`
- `oxc/tools/tiny_real/echo_restart_loop.py`
- `oxc/tools/tiny_real/oxa_tiny_loop.py`
- `oxc/tools/tune_from_ledger.py`
- `oxc/tools/verify-flash-source.py`
- `oxc/tools/verify_image_source.py`
