# Python port ledger

Release qualification for #951, inspected at main fd337a5f and packaging candidate #824 at f38febd3. This initial ledger records three provider replacements, the MEMLAT port, two retained feature gaps and the explicit installer retirement; it inventories the original 366 Python modules. It authorizes no deletion.

PORTED means the named Rust implementation and its contract tests replace the feature. DROPPED requires an explicit retirement decision and an entry in #946. UNUSED requires caller/shipping evidence. MISSING requires a feature-specific issue before that module can be deleted. RETIREMENT INDEX PENDING records a bounded owner decision awaiting its #946 entry. DEV TOOL marks a developer workflow that can remain Python. UNQUALIFIED records concrete callers or a missing decision without authorizing deletion.

| Python module / feature | Status | Rust replacement (crate/module) | Evidence |
|---|---|---|---|
| oxc/src/oxc/runtime/cloud_anthropic.py — Anthropic transport, translation and native tool loop | PORTED | oxa/oxc-gateway/src/anthropic_client.rs; anthropic_request.rs; anthropic_stream.rs | oxa/oxc-gateway/tests/anthropic_client.rs; anthropic_request.rs; anthropic_stream.rs; production consumer: oxa/oxa-cli/src/in_process.rs. Source/test mapping, not live-provider acceptance. |
| oxc/src/oxc/runtime/cloud_openai.py — Responses API-key transport, translation and native tool loop | PORTED | oxa/oxc-gateway/src/openai/client.rs; request.rs; stream.rs | oxa/oxc-gateway/tests/openai_client.rs; oxa/oxa-cli/tests/in_process_cloud.rs; production consumer: oxa/oxa-cli/src/in_process.rs. [#1305](https://github.com/oxstack/ox-llm/pull/1305) adds in_process_openai.rs::catalog_default_output_limit_is_visible_and_reaches_a_native_response. The [x86_64 and ARM64 lane](https://github.com/oxstack/ox-llm/actions/runs/44241717571) verifies the catalog limit, one upstream request, and the resulting length stop and exact usage. The legacy SDK wire contract and live-provider acceptance remain separate. |
| oxc/src/oxc/runtime/cloud_chatgpt.py — subscription transport, account-pinned continuation, sign-in and refresh | PORTED | oxa/oxc-gateway/src/openai/client.rs; chatgpt_tokens.rs; login.rs; oxa/oxa-cli/src/login_cmd.rs | oxa/oxc-gateway/tests/chatgpt_tokens.rs; openai_client.rs; oxa/oxa-cli/tests/native_chatgpt_login.rs; production consumer: oxa/oxa-cli/src/in_process.rs. Source/test mapping, not live-provider acceptance. |
| oxc/tools/memlat_boot.py — boot-menu/rollback command | PORTED | oxa/oxc-memlat/src/{main,menu,policy,image}.rs | oxa/oxc-memlat/tests/boot.rs ports all 19 Python cases. The actual Rust command replays 28 scenarios and 52 steps against tests/boot-oracle.json, captured with the same synthetic filesystem on both hosted architectures in [run 44196495976](https://github.com/oxstack/ox-llm/actions/runs/44196495976). Replay checks exit, stderr, selected stdout and pre/post ESP file names and bytes by digest. [#1322](https://github.com/oxstack/ox-llm/pull/1322) removed the old command after the exact archive and caller gates cleared. |
| oxc/tests/test_memlat_boot.py — all19 synthetic boot cases | PORTED | oxa/oxc-memlat/tests/boot.rs | [#1322](https://github.com/oxstack/ox-llm/pull/1322) retired the paired Python test after all 19 cases were carried to Rust. Keep the actual-command differential. |
| oxc/src/oxc/runtime/cloud_prices.py — native live OpenRouter pricing and catalog refresh | PORTED | oxa/oxc-gateway/src/{catalog_cache,catalog_discovery,openrouter,pricing,runtime}.rs; native picker and list in oxa/oxa-cli/src/in_process.rs | [#1236](https://github.com/oxstack/ox-llm/pull/1236) landed account-scoped discovery, a bounded disk cache and stale OpenRouter price refresh before admission. Discovery is opt-in and runs off the inference path. The native OpenRouter CLI discovery and gateway price-floor and token-bound tests passed in the [x86_64 and ARM64 lane](https://github.com/oxstack/ox-llm/actions/runs/44242621656). [#1337](https://github.com/oxstack/ox-llm/pull/1337) moves the historical retry test into the dedicated pricing suite and removes the accounting suite's direct import. The moved test passed in [protected lab CI](https://github.com/oxstack/ox-llm/actions/runs/44257606090/job/134643029390). Preserve the Python daemon's public refresh and the overlay in `cloud.py` while those callers survive. [#628](https://github.com/oxstack/ox-llm/issues/628) retains the owner HTTP catalog and status integration. |
| oxc/src/oxc/runtime/lan_share.py — production owner share controls | MISSING | Native owner CLI and local serving in `oxa/oxa-cli/src/share_cmd.rs` and `oxa/oxc-gateway/src/share/native.rs` | [#1216](https://github.com/oxstack/ox-llm/pull/1216) and [#1235](https://github.com/oxstack/ox-llm/pull/1235) cover foreground serving of selected local aliases and bind restrictions in `oxa/oxa-cli/tests/share_keys.rs`. `oxa/oxc-gateway/tests/share_listener.rs` covers guest streaming, usage settlement and revocation. [#1237](https://github.com/oxstack/ox-llm/pull/1237) adds sharing the interactive owner’s native engine. `share_cmd.rs::owner_router_and_listener_use_the_same_local_engine` verifies shared engine ownership, and `share_keys.rs::owner_session_sharing_flags_refuse_noninteractive_and_agent_invocations` verifies owner entry gating. [#1255](https://github.com/oxstack/ox-llm/pull/1255) adds local guest accounting in the shared native adapter. `oxa/oxc-gateway/tests/ledger.rs::local_guest_records_usage_or_reservation_without_cloud_spend` covers measured and reserved outcomes, zero cloud spend and salted attribution. `tests/share_listener.rs::authenticated_native_bridge_streams_only_guest_text_and_settles_usage` checks the actual listener-to-ledger path. [#1262](https://github.com/oxstack/ox-llm/pull/1262) adds native owner status, exercised by `share_keys.rs::owner_terminal_reports_share_status_only_when_enabled`. [#1263](https://github.com/oxstack/ox-llm/pull/1263) adds explicitly selected metered guest cloud models with a default-empty owner allowlist. `share_keys.rs::foreground_metered_cloud_requires_owner_allowlist_and_settles_a_guest_request` checks default refusal, streaming and ledger settlement against a loopback provider. [#1278](https://github.com/oxstack/ox-llm/pull/1278) exercises the real foreground CLI against a loopback Anthropic stream with backpressure, checking complete guest text, terminal completion and one private settled ledger row. `share_keys.rs::metered_openrouter_forwards_only_supplied_guest_sampling` checks explicit zero and omitted fields against a loopback upstream. The [exact-head lane](https://github.com/oxstack/ox-llm/actions/runs/44227984285) passed all 29 share cases on x86_64 and ARM64. [#688](https://github.com/oxstack/ox-llm/issues/688) remains open for end-to-end tailnet and remaining owner-control acceptance. |
| oxc/src/oxc/cloud_install.py — Python-only installer/venv/controller daemon | DROPPED | Direct native oxa package/default path. | Owner retirement [#681](https://github.com/oxstack/ox-llm/issues/681) and [#946](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947705088). [#1344](https://github.com/oxstack/ox-llm/pull/1344) removed the installer and its dedicated tests after both main blobs were verified in the [indexed archive](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6970464355). The legacy quickstart runs from published v0.2.0 source, and [#1339](https://github.com/oxstack/ox-llm/pull/1339) directs current installs to the native guide. [Hosted #821 tests](https://github.com/oxstack/ox-llm/actions/runs/44035530124/job/133879204920) verify native cloud-only model listing with local disabled and zero controller HTTP requests. |
| oxc/src/oxc/bootstrap.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_build.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_checks.py — legacy bootstrap software-check runner | DROPPED | The native product has no Python bootstrap runner. | [#681](https://github.com/oxstack/ox-llm/issues/681) and the [bootstrap retirement index](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766) cover this runner. [#1358](https://github.com/oxstack/ox-llm/pull/1358) removed it, its dedicated tests and its manifest after matching all three to the indexed archive. The build guide retains the historical procedure and finish gate. The native test binaries, recipes and independent acceptance gates remain. |
| oxc/src/oxc/bootstrap_download.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_guard.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_install.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_jobs.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_sources.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |
| oxc/src/oxc/bootstrap_transfer.py — legacy default bootstrap | DROPPED | No native-default Python planning/fetch/build/install pipeline. | Explicit owner [#681](https://github.com/oxstack/ox-llm/issues/681) default-installer retirement; [reusable archive plan](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6947917766). Not UNUSED: keep until all callers/fixtures are retired or ported. |

Qualified source retirement is Feature F, ahead of release publication (Feature H) in [#1214](https://github.com/oxstack/ox-llm/issues/1214). [#1198](https://github.com/oxstack/ox-llm/pull/1198) landed native packaging. Qualify remaining callers, preserve the exact pre-removal commit as `archive/python-oxc` and record it in #946 before deleting qualified source slices in ledger order. Keep each controller, CLI, Studio, metadata-refresh and test contract tied to its own disposition.

Known callers on the inspected base included packaging/release/{stage,smoke}.py, the Python lab job in .github/workflows/checks.yml, tools/{oxa-contract,openai-contract}/run.sh, tools/gates/tiny_real.sh and related documentation. The MEMLAT differential now uses the captured reference described above. Synthetic path strings in tools/ci/tests must be distinguished from executable imports before changing them.

Native lab transition: existing Go profiles, ledgers, sessions and caps must remain intact. ChatGPT requires its normal protected token reader; no operator copying/reading tokens, implicit re-login or fresh accounting store. Ordinary native use retains the canonical protected token reader. Custom lab HTTP routes remain a separate migration boundary; no credential import or accounting reset is inferred.

## Qualified component receipts and release blockers

[CI run 44093927834](https://github.com/oxstack/ox-llm/actions/runs/44093927834), source 9a1dbaff4e8dd653357bd19e44ab67bb0d3c7167, records passing native Anthropic/OpenRouter tool-loop and settlement controls; both Responses modes with done-only tools; new-process account-pinned ChatGPT continuation; proactive expiry rechecks and refresh rotation; synthetic native PKCE/login/logout; all 19 MEMLAT boot cases and the Python/Rust actual-command differential.

The inspected provider, token, login and MEMLAT implementation blobs in candidate f38febd3 match that tested source. Main fd337a5f additionally changes login_cmd.rs through #889; the older receipt does not qualify that changed input handling. MEMLAT intentionally selects verified STOCK on pending-marker failure, so full failure-path parity with Python is not claimed. These are component receipts, not exact release-artifact or real-provider acceptance. A later documentation-guard queue failure does not waive artifact qualification.

| Remaining release gate | Concrete evidence still required |
|---|---|
| Native packaging, #1198 | The four package boundaries passed on the corrected candidate below, and the packaging controls landed in #1198. #763 still requires exact-release artifact and clean-home receipts. Python remains in build and test tooling. |
| Native memory scope, #1010 | Qualify inherited no-swap or attached-scope behavior in the packaged artifact. No obsolete memory-size requirement is inferred. |
| Provider acceptance, #763 | Complete the named clean-home local, API-key and subscription tasks on the accepted artifact. Synthetic component tests do not substitute for those receipts. |
| Repository qualification, #1012 | Clear main documentation/CI coverage defects through the existing writer; no competing CI edit or test relaxation. |

The bounded native price refresh landed in [#1236](https://github.com/oxstack/ox-llm/pull/1236). The owner HTTP catalog and status integration under #628 and remaining owner LAN controls under #688 still need their applicable acceptance evidence. Preserve legacy callers and qualify each source slice before removal.

Remaining deletion users differ from release blockers: controller CI and the two contract runners retain Python mocks, and tiny_real retains benchmark/conversion tooling. Preserve these until their actual consumers are replaced. The native packaging candidate still consumes four inert MEMLAT templates and a license under oxc/. No module is classified UNUSED from a caller search alone.

## Python alert-file retirement audit

At source `7e6e4b04`, the Python alert rows 5–13 in [the tracked triage](security/code-scanning-triage.md) identify **four unique files**, including the extensionless Studio entry point. The supplied operator open-alert export confirms all nine IDs/locations below: 19 total rows, nine Python rows, file SHA-256 `97f9209b44aae1c1f16bf6bd88a818ba6c28706b536c93b415607e94120daeca`. The worker App still lacks alert-read permission; this is snapshot evidence, not a live API query or a new security verdict. These rows authorize no deletion or dismissal.

| File / bounded feature | Recorded alerts | Status | Replacement or retirement evidence; remaining deletion gate |
|---|---|---|---|
| `oxc/src/oxc/runtime/ctl_http.py` — legacy default controller HTTP front | 5 | DROPPED | [#681](https://github.com/oxstack/ox-llm/issues/681) / [#763](https://github.com/oxstack/ox-llm/issues/763) retire the Python controller daemon from the native default; [retirement index](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6955874053). This does not claim that in-process providers implement HTTP Host/Origin/CORS parity. `controller.py` and Python cloud/mock tests still import the front; qualify retained contracts before deleting it. The existing `oxc/tests/test_openai_api.py::KeyAndCors::test_cors_never_echoes_a_line_break_into_a_header` covers the earlier #479 fix, not Rust replacement or a fresh alert verdict. |
| `oxc/src/oxc/runtime/studio` — legacy Studio web UI, HTTP uploads and job queue | 11, 12, 13 | DROPPED | The [#951 owner decision](https://github.com/oxstack/ox-llm/issues/951#issuecomment-6959811415) and [#946 retirement entry](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6968082873) retire these product surfaces for v0.2.x. [#1196](https://github.com/oxstack/ox-llm/issues/1196) retains a separate native image acceptance gate. [#1300](https://github.com/oxstack/ox-llm/pull/1300) removed the Studio package-data declaration while preserving the controller launcher. The legacy Studio unit, `oxc/tools/stage_v020_final.py` and Python Studio tests still consume the script. [#1274](https://github.com/oxstack/ox-llm/pull/1274) moved the real listener assertion into the Studio security suite, which still starts the legacy process. Preserve the contracts and exact pre-removal archive before source deletion. |
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
| 11 | py/path-injection | high | `oxc/src/oxc/runtime/studio:309` | DROPPED Studio web surface; alert open in supplied snapshot |
| 12 | py/path-injection | high | `oxc/src/oxc/runtime/studio:309` | DROPPED Studio web surface; alert open in supplied snapshot |
| 13 | py/path-injection | high | `oxc/src/oxc/runtime/studio:311` | DROPPED Studio web surface; alert open in supplied snapshot |

MISSING blocks only the named module's deletion. None of these four files is marked PORTED or UNUSED, and none is deleted. The export does not establish any alert fixed or dismissed; all nine are open in that supplied snapshot.

### Installed payload versus build-source archive

Current inspected #824 source is `84f1928bf627ec555e22a59e6f0f9c25fff4f17e`. Its `packaging/release/stage.py` stages three native commands, four inert MEMLAT templates, licenses and the native guide; it does not stage any of the four alert files. That staging blob matches executed source `2f84770eddd55e1443b412b70fa67636c804f7e7` (SHA-256 `8ce2f62c2631d1b089882d194dbb80ec811b63e5096ecccda4b1674d48f66901`).

The [four-boundary artifact receipt](https://github.com/oxstack/ox-llm/pull/824#issuecomment-6954487105) binds runtime tarballs and both Arch variants to **2f84770e**: each inherited proof passed 15/15, native payload/manifests were checked, and Arch runtime dependencies exclude Python. [Run 44121115282](https://github.com/oxstack/ox-llm/actions/runs/44121115282) nevertheless ended FAILURE on separate real-scope setup. This is historical installed-payload evidence, not qualification of the newer 84f1928b artifact or the whole release.

At that #824 revision the workflow used a full `git archive HEAD`. Its
source archive inventory at `84f1928b` included all four tracked alert files.

The later [#1198 artifact member receipt](https://github.com/oxstack/ox-llm/issues/951#issuecomment-6970132625)
qualifies the exact tested build, `174b10136c08186fff165951029431338231ea8f`.
The two CPU tarballs, both Arch install variants and the two identical
filtered source-archive copies omit these paths:

- `oxc/src/oxc/runtime/studio`
- `oxc/src/oxc/runtime/ctl_http.py`
- `oxc/tools/session_replay.py`
- `oxc/tools/interim/recording_proxy.py`

The filtered source-archive copies retain the inert historical example
`oxc/examples/runtime/units/prompt-studio-app.service`. It is absent from the
four install payloads. Those payloads each contain the four inert MEMLAT
templates. Release artifact acceptance remains in #763. Repository-source
remediation, retained callers and the alert disposition remain in #951 and
#477.

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

#### MEMLAT source retirement and #1198 packaging evidence

[#1234](https://github.com/oxstack/ox-llm/pull/1234) merged at
`be0e5b82a47bf313704fbcb079ea68d26ddbb333` after
[integration CI](https://github.com/oxstack/ox-llm/actions/runs/44199296898).
Before removal, the paired Python test was the only tracked executable
importer of `oxc/tools/memlat_boot.py`. The Rust suite covers its 19 cases,
and the four tracked MEMLAT templates invoke
`/usr/bin/local-ai-memlat-boot`.

- The [stage rule](https://github.com/oxstack/ox-llm/blob/6ecc77a3ee5b2ff1beed7b11780b5d3f22d4dc74/packaging/release/stage.py#L19-L42)
  installs the Rust boot command and excludes the legacy boot script and test
  via the `tools` and `tests` ignore patterns. That baseline rule also copied the
  Python controller and created Python wrappers.
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

The [#1288 staging control](https://github.com/oxstack/ox-llm/pull/1288)
exercises release staging with a synthetic `oxc-memlat-shutdown.service` under
the excluded basename. It retains the four real shipping templates and checks
their inventory, bytes and modes, plus the service exclusion. The
[exact-head lane](https://github.com/oxstack/ox-llm/actions/runs/44230833991)
passed all 52 captured boot-command steps and the real staging assertion on
both x86_64 and ARM64. Keep the four templates and the captured oracle.

[archive/python-oxc](https://github.com/oxstack/ox-llm/tree/archive/python-oxc)
preserves the full pre-removal `oxc/` tree at
`e64b985a2ff6abf51d406471382ed95a18fd161c`, recorded in
[#946](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6970464355).
[#1322](https://github.com/oxstack/ox-llm/pull/1322) removed both Python files
and updated the manual link in `ab193a52be0ef1d15e675e69287eaf1d9e41d268`.
The [protected MEMLAT differential](https://github.com/oxstack/ox-llm/actions/runs/44249788127/job/134619511480)
passed all 52 steps and the staging exclusion assertion after the removal.
The captured oracle, developer builder and four shipping templates are
unchanged.

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
| `oxc/src/oxc/runtime/studio_page.py` — embedded Studio web page | DROPPED | The [#946 retirement entry](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6968082873) covers the v0.2.x Studio web UI. The `studio` script imports `PAGE`. Preserve the legacy caller contracts and archive before deleting source. |
| `oxc/src/oxc/runtime/studio_context.py` — Python Studio namespace adapter | DROPPED | The [#946 retirement entry](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6968082873) covers the legacy Studio product surface. `studio`, the planners, the worker and the oxi adapter still use the shared namespace. Keep it with those callers until their dispositions and the source archive are recorded. |
| `oxc/src/oxc/runtime/studio_pixels.py` — legacy Studio upload, mask and crop transforms | DROPPED | The [#946 retirement entry](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6968082873) covers the legacy Studio editing surface. The Studio caller still uses the PNG, mask and crop helpers. Retain its callers and `test_studio_crop_edit.py` coverage until qualified for removal. Native image behavior remains tracked by [#1196](https://github.com/oxstack/ox-llm/issues/1196). |
| `oxc/src/oxc/runtime/studio_plan_edit.py` — Studio FLUX.2 edit and generation planning | MISSING | `studio` imports the planner; `test_studio_edit.py` and `test_studio_klein.py` cover legacy modes. Carry the generation behavior named in [#1196](https://github.com/oxstack/ox-llm/issues/1196). The [#946 retirement entry](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6968082873) covers editing. |
| `oxc/src/oxc/runtime/studio_plan_generate.py` — Studio generation, upscale and refine planning | MISSING | Existing product replacement [#1196](https://github.com/oxstack/ox-llm/issues/1196). `studio` imports the planner; `test_studio_upscale.py` exercises the legacy upscale path. Scope the native image behavior and index any retired planning modes. |
| `oxc/src/oxc/runtime/studio_oxi.py` — Studio native image adapter and legacy fallback | MISSING | Existing product replacement [#1196](https://github.com/oxstack/ox-llm/issues/1196). `studio` imports the adapter, and `test_studio_oxi.py` covers route and fallback behavior. Native acceptance explicitly excludes silent fallback. Preserve the legacy test while its caller remains. |
| `oxc/src/oxc/runtime/studio_worker.py` — legacy Studio job and image-worker lifecycle | MISSING | Existing product replacement [#1196](https://github.com/oxstack/ox-llm/issues/1196). `studio` invokes the worker, and `test_studio_handoff.py` exercises the worker boundary. The [#946 retirement entry](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6968082873) covers web-job orchestration. Qualify native image-worker admission and cleanup separately. |
| `oxc/packaging/flash-qos/oxc-flash-qos` — Flash idle-QoS helper | UNQUALIFIED | [#1198](https://github.com/oxstack/ox-llm/pull/1198) excludes this extensionless script from the installed package and release source archive. The legacy unit and Flash stage use it, and `test_flash_qos_helper.py` and `test_flash_qos_install_user.py` retain its contract. Source retirement still needs an explicit feature decision and index entry. |

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
existing success probes for `oxe` in the Arch smoke. The corrected run and
its Arch package receipts are recorded below.

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
| `oxc/src/oxc/runtime/ctl_proxy.py` — typed native request and tool-result forwarding | PORTED | `oxa/oxc-local/tests/transport.rs::typed_tool_result_continues_the_same_owned_engine_without_a_controller` covers native local continuation. In [protected CI](https://github.com/oxstack/ox-llm/actions/runs/44240079306/job/134587945254), `local_overflow_uses_the_shared_error_classification` and `progress_before_overflow_preserves_typed_recovery_without_replaying` passed on `6fde2c71`. They cover native consumer classification for direct and queued in-stream overflow. The queued case checks a single POST. [#1314](https://github.com/oxstack/ox-llm/pull/1314) also exercises the owned LocalEngine child. `owned_child_in_stream_overflow_reaches_caller_without_output` checks typed context overflow and no text, reasoning or tool-argument output for the plain in-stream SSE error. It passed in [hosted CI on both architectures](https://github.com/oxstack/ox-llm/actions/runs/44244269082). The legacy gateway's production of the F3 wire response remains a separate contract in `tools/oxa-contract/stub_contract.rs`. `controller.py` still uses the Python proxy for request defaults and all legacy routes; keep `test_v013_proxy_contract.py`. [#1320](https://github.com/oxstack/ox-llm/pull/1320) covers F1: two unchanged-history follow-ups keep the same owned child and report the expected cached-token counts. The case passed on [x86_64 and ARM64](https://github.com/oxstack/ox-llm/actions/runs/44247971224). [#1321](https://github.com/oxstack/ox-llm/pull/1321) covers F9: the native transport forwards trailing assistant history intact and gets a new reply. The legacy contract runner still exercises the old serving path. |
| `oxc/src/oxc/runtime/ctl_proxy_prep.py` — legacy request normalization and backend hints | MISSING | [#222](https://github.com/oxstack/ox-llm/issues/222) scopes request normalization. This preparation stage still selects the Responses or Chat shim and attaches the legacy cache hints. Preserve `test_openai_conformance.py` and `test_cache_perfect_p1.py`. |
| `oxc/src/oxc/runtime/ctl_proxy_stream.py` — typed local reply streaming | PORTED | `oxa/oxc-local/tests/transport.rs::typed_request_streams_tools_usage_and_text_without_controller_routes` exercises the native stream. The Python proxy still handles HTTP response classification and forwarding with the legacy disconnect watcher. Keep the controller comparison and `test_controller_disconnect_http.py`. [#1321](https://github.com/oxstack/ox-llm/pull/1321) covers F2 and F6: the native transport delivers typed prefill progress and preserves an explicit output budget, length finish and usage when generation fills the context. Both cases and F9 passed on [x86_64 and ARM64](https://github.com/oxstack/ox-llm/actions/runs/44248341368). [#1335](https://github.com/oxstack/ox-llm/pull/1335) covers F10: the native child transport delivers two positionless queue comments with their wait durations before reply text. The case passed on [x86_64 and ARM64](https://github.com/oxstack/ox-llm/actions/runs/44254870615). The F7/Info prefill-only contract remains open: native LocalEngine advertises `prefill_only: false`. |
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

### Corrected packaging receipts at `b8111ecd`

[Release run 44211078636](https://github.com/oxstack/ox-llm/actions/runs/44211078636)
produced both CPU artifacts from `174b10136c08186fff165951029431338231ea8f`, the
test merge of [#1198](https://github.com/oxstack/ox-llm/pull/1198) head
`b8111ecd` into `be52cf0e`. Their checksum manifests and 19/19 no-Python
receipts passed. The filtered source archives were byte-identical at SHA-256
`232c919b05ac2dba6981654f3f596a7d68d0706bc16a5a5a6cd76926f9ce40a3`.
Each native payload contains an executable ELF MEMLAT command and four regular
0644 hook templates whose bytes match the archived sources. Both payloads
exclude active hook paths, MEMLAT policy, Python files and legacy wrappers.
[Arch job 134497318931](https://github.com/oxstack/ox-llm/actions/runs/44211078636/job/134497318931)
passed source and binary builds, namcap, installation, smoke, integrity and
removal. The uploaded package checksum manifest passed. Both archives list
`glibc`, `gcc-libs` and `bubblewrap` as runtime dependencies. Each ships the
same four native commands and four inert templates as the CPU payloads,
with no Python files, controller wrappers, active hooks or MEMLAT policy.

| Arch package | SHA-256 |
|---|---|
| Source-built | `53a7299b500de91d989f4b7651f04159176989bb1b097f078004c6c2179360a7` |
| Prebuilt binary | `68aec81c865d088236b5b166d53dcef1580c3c97d84443678147d1ee8b3e4096` |

#1198 merged at `0ddf790ada7708966957da50e916d505ed0b7f22`. The tracked
workflow, release/Arch scripts and MEMLAT templates in that commit match
the tested packaging head. The cited artifacts come from the candidate test
merge. This clears the packaging SHIP condition for #951 rows 021
(`oxc/src/oxc/__main__.py`), 095 (`oxc/src/oxc/runtime/oxc`) and 105
(`oxc/src/oxc/runtime/studio`). Retain the remaining caller gates and all four inert MEMLAT templates.

- The Python reference pass in `.github/workflows/checks.yml` still invokes
  the legacy launcher. [#1275](https://github.com/oxstack/ox-llm/pull/1275)
  removed the native parser pass's launcher link and Python import path.
  Both passes retain the same 143 golden cases.
- [#1271](https://github.com/oxstack/ox-llm/pull/1271) moved the controller
  startup-error regression into `test_controller_recovery.py`. The five
  status CLI tests still exercise the legacy launcher.
- [#1274](https://github.com/oxstack/ox-llm/pull/1274) moved the real Studio
  listener test from `test_controller_behaviour.py` to
  `test_studio_security.py`. The destination still invokes Studio.

The [#763](https://github.com/oxstack/ox-llm/issues/763) packaging requirement
remains open. The tested packages contain standalone `oxe`, which still needs
an explicit release allowlist decision. Each source slice still requires
its caller and evidence gates and the indexed pre-removal archive.

### Remaining controller-path evidence at `7f7bfb6a`

These entries account for the remaining paths in the packaged legacy subtree.
Development tools can remain Python. Native packaging excludes the old
controller subtree. Source removal still needs the applicable index, caller
and native acceptance evidence.

| Python module / feature | Status | Evidence and remaining caller gate |
|---|---|---|
| `oxc/src/oxc/__init__.py` — legacy package version and import namespace | UNQUALIFIED | Exports the legacy package version and supports the package imports below. Retain with surviving lab imports until the package boundary is settled. |
| `oxc/src/oxc/__main__.py` — default packaged diagnostic launcher | DROPPED | [#681](https://github.com/oxstack/ox-llm/issues/681) and the [#946 entry](https://github.com/oxstack/ox-llm/issues/946#issuecomment-6973156119) retire the default packaged launcher. [#1198](https://github.com/oxstack/ox-llm/pull/1198) cleared the shipping gate, and [#1355](https://github.com/oxstack/ox-llm/pull/1355) pinned the two documented hardware-check commands to the archive. Source retirement still requires handling the `oxc-diagnostics` script entry in `oxc/pyproject.toml`, the legacy CI `python3 -m oxc --help` check, and `oxc/tests/test_v013_integration.py`'s panel-dispatch fixture. Preserve the archived hardware checks and the independent developer diagnostics, panel and public-metrics contracts. |
| `oxc/src/oxc/batch_runner.py` — guarded benchmark-arm orchestration | DEV TOOL | Runs ordered benchmark arms with frozen evidence and delegates counters to `system_counters.py`. Developer workload. Confirm any runnable harness caller before moving after the product slices. |
| `oxc/src/oxc/devfreq_probe.py` — developer devfreq instrumentation | DEV TOOL | `tools/devfreq_trace.py` and `test_devfreq_probe.py` use its devfreq instrumentation. Retain the developer contract. It currently travels in the broad Python stage. |
| `oxc/src/oxc/doctor.py` — legacy hardware diagnostics | RETIREMENT INDEX PENDING | The diagnostics entry, legacy bootstrap and engine report import it. `test_doctor.py` covers its report. The #681 default diagnostics retirement covers the installed launcher. Qualify the report and its other callers separately. |
| `oxc/src/oxc/engine_report.py` — legacy configured-profile diagnostics | RETIREMENT INDEX PENDING | Imported by the diagnostics `doctor` command; exercised by `test_engine_fit.py`. The owned native engine has typed status. The legacy report additionally inspects controller configuration and needs a retirement disposition. |
| `oxc/src/oxc/flash_profile.py` — developer Flash profiling and attribution | DEV TOOL | `test_flash_profile.py` exercises numeric profiler attribution and process identity. Namesake `flash_profile` arguments are distinct. Retain as developer support until the actual tracked caller boundary is verified. |
| `oxc/src/oxc/legacy_config.py` — legacy configuration migration | UNQUALIFIED | [#1344](https://github.com/oxstack/ox-llm/pull/1344) removed the installer consumer and its installer-specific migration cases. The four standalone cases in `test_legacy_config_migration.py` still exercise configuration continuity and atomicity, and passed in the [#1344 hosted lab shard](https://github.com/oxstack/ox-llm/actions/runs/44261493968/job/134655738352). The cloud-installer disposition does not cover this standalone contract. |
| `oxc/src/oxc/platform_support.py` — legacy diagnostics and panel platform preflight | RETIREMENT INDEX PENDING | `doctor.py` and `plugin_install.py` use SoC detection; `test_doctor.py` and `test_plugin_install.py` exercise it. Group with the diagnostics and panel outcomes. |
| `oxc/src/oxc/plugin_install.py` — legacy Omarchy panel installer | RETIREMENT INDEX PENDING | `__main__.py install-plugin` and both plugin-install test files exercise the explicit legacy path. #681 drops default panel installation. Keep fixture coverage for the surviving contributor path until indexed. |
| `oxc/src/oxc/power.py` — legacy runtime power and benchmark readings | UNQUALIFIED | Bootstrap, doctor, result export, instrumentation and `bigmoe_runtime.py` consume power readings. `test_power.py` and `test_flash_power_degraded.py` cover separate consumers. Qualify candidate admission and retain developer reading semantics. |
| `oxc/src/oxc/results.py` — public developer export and legacy command | DEV TOOL | `__main__.py export-screen` and `test_controller_export.py` cover allowlisted export functions. Preserve the allowlisted developer export in `test_public_results.py` and `test_controller_export.py` when retiring the package launcher. |
| `oxc/src/oxc/runtime/__init__.py` — legacy runtime import namespace | UNQUALIFIED | Package marker for the staged runtime imports. Retain with legacy runtime importers and fixtures. |
| `oxc/src/oxc/runtime/bigmoe_adapter.py` — legacy Flash candidate fallback adapter | RETIREMENT INDEX PENDING | `bigmoe_runtime.py` injects candidate transport into this adapter. #681 excludes the legacy fallback and adapter from the new default. Record the index entry and retire or replace the remaining candidate callers. |
| `oxc/src/oxc/runtime/bigmoe_runtime.py` — legacy Flash candidate qualification and launch | RETIREMENT INDEX PENDING | `ctl_engines.py`, `ctl_lifecycle.py` and `ctl_models.py` use its launch and qualification policy. It also checks the no-THP and slim-request source identities. #681 excludes the legacy fallback and adapter from the native default. Preserve the candidate evidence and fixtures until their retirement is indexed. |
| `oxc/src/oxc/runtime/classic_beta.py` — legacy Ornith qualification and fallback policies | RETIREMENT INDEX PENDING | Live controller, model, launch and proxy imports. `test_beta_selector.py` checks legacy qualification. #681 excludes the old fallback from the native default. Record its remaining acceptance and evidence requirements before deleting the source. |
| `oxc/src/oxc/runtime/ctl_env.py` — legacy controller environment and request labels | RETIREMENT INDEX PENDING | Controller and request mixins share environment, error and origin handling. Keep the environment and fixture contract with the default daemon retirement, including origin and limits. |
| `oxc/src/oxc/runtime/ctl_host.py` — native engine child-memory sampling | PORTED | Controller reads host memory, swap, temperatures, frequencies and port state. `oxa/oxc-local/src/memory.rs::kernel_samples_are_complete_and_fail_closed_on_malformed_or_missing_data` covers the child-memory subset. Qualify the other legacy host report consumers separately. |
| `oxc/src/oxc/runtime/ctl_repack.py` — legacy llama.cpp repack-cache diagnostics | UNQUALIFIED | `ctl_engines.py` reads llama.cpp cache identity and hooks. `test_repack_cache_notify.py` exercises the legacy error path. Separate llama.cpp cache policy from native launch qualification. |
| `oxc/src/oxc/runtime/engine_diagnostics.py` — legacy controller stderr capture | UNQUALIFIED | Controller, lifecycle and ledger read bounded stderr; `test_engine_diagnostics.py` exercises capture. Keep diagnostics and error-retention cases until their policy is indexed. |
| `oxc/src/oxc/runtime/engine_ledger.py` — legacy inherited-FD engine diagnostics | UNQUALIFIED | `ctl_lifecycle.py` passes a descriptor and starts capture. `ctl_status.py` exposes metadata. `test_engine_ledger.py` checks failure cleanup. Preserve the diagnostic FD and lifecycle tests until their caller disposition. The native controller-status fixture carries a compatibility field. |
| `oxc/src/oxc/runtime/health_digest.py` — legacy request and engine health digest | UNQUALIFIED | The control launcher reads ledger rows and death events to build a digest. `test_health_digest.py` exercises output filtering. Keep the read-side reporting and privacy behavior until a retained-feature decision is recorded. |
| `oxc/src/oxc/runtime/model_catalog.py` — native local listing without launch | PORTED | `ctl_models.py`, `ctl_cache.py`, `storage_view.py` and the legacy chooser use the metadata. `oxa/oxa-cli/tests/in_process_local.rs::native_local_discovery_is_user_only_non_launching_and_allowed_offline` covers native enumeration. Keep `test_flash_qualification.py` and `test_ui_data.py` for the legacy chooser facts. |
| `oxc/src/oxc/runtime/release_update.py` — legacy two-service staged update and rollback | RETIREMENT INDEX PENDING | The #951 owner decision at 04:32 UTC retires controller and Studio staging and updates from the v0.2.x product path. Preserve the old implementation for developer and historical use with the rollback tests, exact pre-removal archive and #946 entry. v0.2.x upgrades use native packages. The legacy CLI, stage generator and `oxc/tools/oem_drills.py` still consume this code. |
| `oxc/src/oxc/runtime/slim_request.py` — opt-in Flash candidate request shaping | UNQUALIFIED | Optional candidate-specific shaping. `bigmoe_runtime.py` binds its source digest and enables it explicitly. Preserve with the candidate evidence and tests until disposition. |
| `oxc/src/oxc/runtime/storage_view.py` — legacy owner storage inventory and deletion | RETIREMENT INDEX PENDING | `ctl_routes.py` serves inventory; `ctl_cache.py` calls deletion. `test_controller_n6.py` and `test_state_cache.py` cover consumers. #681 excludes the default legacy storage view. Record the index entry and carry any retained native storage-control contract separately from #222. |
| `oxc/src/oxc/runtime/uninstall.py` — legacy staged-install ownership and model retention | UNQUALIFIED | The legacy CLI loads this module and `test_uninstall.py` exercises guarded ownership and model retention. Compare #1198 Arch package removal with the old staged uninstall contract once the release is accepted. |
| `oxc/src/oxc/source_archive.py` — legacy developer source exporter | DEV TOOL | Retained developer exporter and `test_source_archive.py`. Its required paths name the standalone controller, Studio and bootstrap. [#1343](https://github.com/oxstack/ox-llm/pull/1343) pins the selected-setup guide to the archived instructions and the 271-file receipt for standalone commit `8c1cc846e606763fadd75429fde4f58b246291c1`. The monorepo preservation tag cannot be the exporter input. Native release archives use the packaging workflow. |
| `oxc/src/oxc/system_counters.py` — benchmark runner counters | DEV TOOL | `tools/system_counters.py`, `batch_runner.py` and `test_system_counters.py` use prompt-free counters. Retain the developer workflow while packaging excludes it. |

### Legacy two-service staging and updates

The #951 owner decision at 04:32 UTC retires the Python controller and Studio
staging and updater from the v0.2.x product path. Follow the [native install and upgrade
guide](NATIVE-INSTALL.md) for that release line. The legacy generator, shared
helpers, updater, rollback guard and tests remain developer and historical
material. The [preservation inventory](https://github.com/oxstack/ox-llm/issues/951#issuecomment-6969167764)
lists the exact source and retained callers. Preserve the full pre-removal
source in the archive and record it in #946 before actual source removal.

The [compatibility trace](https://github.com/oxstack/ox-llm/issues/947#issuecomment-6969013425)
shows why the historical contracts stay together. The installed updater
records both services, and rollback requires that recorded set to match the
sealed binding. A controller-only stage would make rollback refuse. Preserve
the seal, preimage, service identity and rollback checks, plus all four real
MEMLAT templates. [#1323](https://github.com/oxstack/ox-llm/pull/1323) routes the current upgrade guide to native packages and pins both legacy procedure links to the archive. The Studio caller, native image and archive gates remain
separate.

## Tests and developer module inventory

The evidence tables account for all 96 Python paths under `oxc/src/oxc` at the inspected main commit, including both extensionless launchers. Individual rows record any outstanding feature disposition or caller proof. The paths below cover tests, benchmark files and tools. Retained fixtures and developer workflows have their own caller gates before removal.

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
- `oxc/tests/test_bootstrap_checks.py` (retired with the runner in [#1358](https://github.com/oxstack/ox-llm/pull/1358), preserved in `archive/python-oxc`)
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
- `oxc/tests/test_cloud_only_install.py` (retired with the installer in [#1344](https://github.com/oxstack/ox-llm/pull/1344), preserved in `archive/python-oxc`)
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
