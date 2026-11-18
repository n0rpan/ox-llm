# Python port ledger

Release qualification for #951, inspected at main fd337a5f and packaging candidate #824 at f38febd3. This initial ledger records three provider replacements, the MEMLAT port, two retained feature gaps and the explicit installer retirement; it inventories all366 Python modules. It authorizes no deletion.

PORTED means the named Rust implementation and its contract tests replace the feature. DROPPED requires an explicit retirement decision and an entry in #946. UNUSED requires caller/shipping evidence. MISSING requires a feature-specific issue before that module can be deleted. Unqualified modules below are deliberately not assigned a guessed status.

| Python module / feature | Status | Rust replacement (crate/module) | Evidence |
|---|---|---|---|
| oxc/src/oxc/runtime/cloud_anthropic.py — Anthropic transport, translation and native tool loop | PORTED | oxa/oxc-gateway/src/anthropic_client.rs; anthropic_request.rs; anthropic_stream.rs | oxa/oxc-gateway/tests/anthropic_client.rs; anthropic_request.rs; anthropic_stream.rs; production consumer: oxa/oxa-cli/src/in_process.rs. Source/test mapping, not live-provider acceptance. |
| oxc/src/oxc/runtime/cloud_openai.py — Responses API-key transport, translation and native tool loop | PORTED | oxa/oxc-gateway/src/openai/client.rs; request.rs; stream.rs | oxa/oxc-gateway/tests/openai_client.rs; oxa/oxa-cli/tests/in_process_cloud.rs; production consumer: oxa/oxa-cli/src/in_process.rs. Source/test mapping, not live-provider acceptance. |
| oxc/src/oxc/runtime/cloud_chatgpt.py — subscription transport, account-pinned continuation, sign-in and refresh | PORTED | oxa/oxc-gateway/src/openai/client.rs; chatgpt_tokens.rs; login.rs; oxa/oxa-cli/src/login_cmd.rs | oxa/oxc-gateway/tests/chatgpt_tokens.rs; openai_client.rs; oxa/oxa-cli/tests/native_chatgpt_login.rs; production consumer: oxa/oxa-cli/src/in_process.rs. Source/test mapping, not live-provider acceptance. |
| oxc/tools/memlat_boot.py — boot-menu/rollback command | PORTED | oxa/oxc-memlat/src/{main,menu,policy,image}.rs | oxa/oxc-memlat/tests/boot.rs ports all 19 Python cases. The actual Rust command replays 28 scenarios and 52 steps against tests/boot-oracle.json, captured with the same synthetic filesystem on both hosted architectures in [run 44196495976](https://github.com/oxstack/ox-llm/actions/runs/44196495976). Replay checks exit, stderr, selected stdout and pre/post ESP file names and bytes by digest. The old command remains subject to packaging, archive and caller gates. |
| oxc/tests/test_memlat_boot.py — all19 synthetic boot cases | PORTED | oxa/oxc-memlat/tests/boot.rs | Explicit port declaration and matching19-case source inventory. No host boot actions. Keep the stronger actual-command differential coverage. |
| oxc/src/oxc/runtime/cloud_prices.py — live public OpenRouter pricing metadata | MISSING | Existing native pricing.rs parses configured prices; that is not a refresh consumer. | Retained feature issue [#628](https://github.com/oxstack/ox-llm/issues/628). Preserve bounded replies, durable last-known data and no request-path fetch before deleting this module. |
| oxc/src/oxc/runtime/lan_share.py — production owner share controls | MISSING | Existing oxc-gateway/src/share components have loopback tests; component existence does not provide the owner CLI/control path. | Retained feature issue [#688](https://github.com/oxstack/ox-llm/issues/688); qualify its actual consumer before deletion. |
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
| `oxc/src/oxc/runtime/studio` — HTTP/UI/job/image pipeline | 11, 12, 13 | MISSING | [Module blocker #1118](https://github.com/oxstack/ox-llm/issues/1118). #681 says rebuilt later; release exclusion is not permanent feature retirement. Native image executables alone do not replace the application. The legacy Studio unit, package-data declaration, `test_controller_behaviour.py` and `test_studio_*` still consume it. No native application parity receipt is established here. |
| `oxc/tools/session_replay.py` — HTTP recording, equal-work replay, comparison and summaries | 8, 9, 10 | MISSING | [Module blocker #1119](https://github.com/oxstack/ox-llm/issues/1119). `test_session_replay.py`, `test_session_replay_output_proof.py` and `oxc/docs/HARNESSES.md` retain this workflow. Native conversation continuation is not an equal-work recording/replay replacement. Preserve the retained contracts or record explicit retirement before deletion. |
| `oxc/tools/interim/recording_proxy.py` — metadata recorder and listener-ownership helpers | 6, 7 | MISSING | [Module blocker #1120](https://github.com/oxstack/ox-llm/issues/1120). `oxc/tools/interim/real_use_session.py`, `test_interim_adapters.py` and `test_explore1_observer.py` import it. These executable callers prevent UNUSED; no equivalent Rust consumer/test receipt was identified. |

The operator export's per-alert mapping is:

| Alert | Rule | Severity | Exported location | File disposition |
|---|---|---|---|---|
| 5 | py/http-response-splitting | medium | `oxc/src/oxc/runtime/ctl_http.py:173` | DROPPED default controller front; callers still gate deletion |
| 6 | py/http-response-splitting | medium | `oxc/tools/interim/recording_proxy.py:154` | MISSING #1120 |
| 7 | py/http-response-splitting | medium | `oxc/tools/interim/recording_proxy.py:154` | MISSING #1120 |
| 8 | py/http-response-splitting | medium | `oxc/tools/session_replay.py:370` | MISSING #1119 |
| 9 | py/http-response-splitting | medium | `oxc/tools/session_replay.py:370` | MISSING #1119 |
| 10 | py/http-response-splitting | medium | `oxc/tools/session_replay.py:374` | MISSING #1119 |
| 11 | py/path-injection | high | `oxc/src/oxc/runtime/studio:309` | MISSING #1118 |
| 12 | py/path-injection | high | `oxc/src/oxc/runtime/studio:309` | MISSING #1118 |
| 13 | py/path-injection | high | `oxc/src/oxc/runtime/studio:311` | MISSING #1118 |

MISSING blocks only the named module's deletion. None of these four files is marked PORTED or UNUSED, and none is deleted. The separate module blockers do not add a v0.2.1 feature promise. The export does not establish any alert fixed or dismissed; all nine are open in that supplied snapshot.

### Installed payload versus build-source archive

Current inspected #824 source is `84f1928bf627ec555e22a59e6f0f9c25fff4f17e`. Its `packaging/release/stage.py` stages three native commands, four inert MEMLAT templates, licenses and the native guide; it does not stage any of the four alert files. That staging blob matches executed source `2f84770eddd55e1443b412b70fa67636c804f7e7` (SHA-256 `8ce2f62c2631d1b089882d194dbb80ec811b63e5096ecccda4b1674d48f66901`).

The [four-boundary artifact receipt](https://github.com/oxstack/ox-llm/pull/824#issuecomment-6954487105) binds runtime tarballs and both Arch variants to **2f84770e**: each inherited proof passed 15/15, native payload/manifests were checked, and Arch runtime dependencies exclude Python. [Run 44121115282](https://github.com/oxstack/ox-llm/actions/runs/44121115282) nevertheless ended FAILURE on separate real-scope setup. This is historical installed-payload evidence, not qualification of the newer 84f1928b artifact or the whole release.

The same release workflow also creates a full `git archive HEAD` build-source tarball. A source-only archive inventory at 84f1928b includes **all four alert files**. Therefore neither “absent from every release artifact” nor “alerts fixed by removal” is established. Fresh exact-head artifact qualification remains with #824; repository-source remediation/removal remains with #951 / #477.

## Complete module inventory awaiting feature-level qualification

Each path below needs a ledger disposition with its own evidence; presence here is neither UNUSED nor a deletion approval. Move a module into the table only after checking every retained feature and its callers. Keep existing #681 retirement decisions distinct from actual Rust replacement coverage. The provider, MEMLAT, retired installer/bootstrap and alert-file rows already mapped above are omitted here. Partial-feature rows do not qualify a whole module. The extensionless entry point oxc/src/oxc/runtime/oxc still needs an explicit launcher/feature row; Studio is tracked by #1118 above.

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
- `oxc/src/oxc/runtime/cache_miss.py`
- `oxc/src/oxc/runtime/cache_turn.py`
- `oxc/src/oxc/runtime/classic_beta.py`
- `oxc/src/oxc/runtime/client_disconnect.py`
- `oxc/src/oxc/runtime/cloud.py`
- `oxc/src/oxc/runtime/cloud_prices.py`
- `oxc/src/oxc/runtime/controller.py`
- `oxc/src/oxc/runtime/ctl_cache.py`
- `oxc/src/oxc/runtime/ctl_cloud.py`
- `oxc/src/oxc/runtime/ctl_diag.py`
- `oxc/src/oxc/runtime/ctl_engines.py`
- `oxc/src/oxc/runtime/ctl_env.py`
- `oxc/src/oxc/runtime/ctl_host.py`
- `oxc/src/oxc/runtime/ctl_lease.py`
- `oxc/src/oxc/runtime/ctl_lifecycle.py`
- `oxc/src/oxc/runtime/ctl_manager.py`
- `oxc/src/oxc/runtime/ctl_models.py`
- `oxc/src/oxc/runtime/ctl_proxy.py`
- `oxc/src/oxc/runtime/ctl_proxy_ledger.py`
- `oxc/src/oxc/runtime/ctl_proxy_prep.py`
- `oxc/src/oxc/runtime/ctl_proxy_stream.py`
- `oxc/src/oxc/runtime/ctl_queue.py`
- `oxc/src/oxc/runtime/ctl_repack.py`
- `oxc/src/oxc/runtime/ctl_replay.py`
- `oxc/src/oxc/runtime/ctl_routes.py`
- `oxc/src/oxc/runtime/ctl_seams.py`
- `oxc/src/oxc/runtime/ctl_status.py`
- `oxc/src/oxc/runtime/engine_diagnostics.py`
- `oxc/src/oxc/runtime/engine_ledger.py`
- `oxc/src/oxc/runtime/exec_in_scope.py`
- `oxc/src/oxc/runtime/exec_no_thp.py`
- `oxc/src/oxc/runtime/flash_cache_pref.py`
- `oxc/src/oxc/runtime/harness_compat.py`
- `oxc/src/oxc/runtime/harness_registry.py`
- `oxc/src/oxc/runtime/health_digest.py`
- `oxc/src/oxc/runtime/image_npu.py`
- `oxc/src/oxc/runtime/image_npu_worker.py`
- `oxc/src/oxc/runtime/image_size_gate.py`
- `oxc/src/oxc/runtime/image_slot.py`
- `oxc/src/oxc/runtime/keep_warm.py`
- `oxc/src/oxc/runtime/lan_share.py`
- `oxc/src/oxc/runtime/memory_policy.py`
- `oxc/src/oxc/runtime/model_catalog.py`
- `oxc/src/oxc/runtime/openai_api.py`
- `oxc/src/oxc/runtime/openai_conformance.py`
- `oxc/src/oxc/runtime/openai_responses.py`
- `oxc/src/oxc/runtime/oxe_runtime.py`
- `oxc/src/oxc/runtime/prefix_plan.py`
- `oxc/src/oxc/runtime/prefix_store.py`
- `oxc/src/oxc/runtime/profile_policy.py`
- `oxc/src/oxc/runtime/release_update.py`
- `oxc/src/oxc/runtime/request_ledger.py`
- `oxc/src/oxc/runtime/slim_request.py`
- `oxc/src/oxc/runtime/state_cache.py`
- `oxc/src/oxc/runtime/storage_view.py`
- `oxc/src/oxc/runtime/studio_context.py`
- `oxc/src/oxc/runtime/studio_oxi.py`
- `oxc/src/oxc/runtime/studio_page.py`
- `oxc/src/oxc/runtime/studio_pixels.py`
- `oxc/src/oxc/runtime/studio_plan_edit.py`
- `oxc/src/oxc/runtime/studio_plan_generate.py`
- `oxc/src/oxc/runtime/studio_worker.py`
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
