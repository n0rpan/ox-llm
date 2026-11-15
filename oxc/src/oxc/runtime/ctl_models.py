"""runtime.json as the controller sees it: the resolved profile of a model, the model list
and chooser rows, and the settings a user changes (selected model, context mode, harness
learning, harness prefix preparation).
"""
import json
import time
if __package__:
    from .ctl_env import Unavailable
    from .ctl_seams import ctl
    from .harness_registry import learning_settings
    from . import bigmoe_runtime, classic_beta, cloud, harness_registry, lan_share, model_catalog, openai_api, oxe_runtime, profile_policy
else:
    from ctl_env import Unavailable
    from ctl_seams import ctl
    from harness_registry import learning_settings
    import bigmoe_runtime
    import classic_beta
    import cloud
    import harness_registry
    import lan_share
    import model_catalog
    import openai_api
    import oxe_runtime
    import profile_policy


def output_limit(config):
    """The output tokens the controller lets one request of this resolved profile produce when
    the client sends no length field: Flash's qualified cap (openai_api.FLASH_MAX_TOKENS, also
    the most it accepts), else the default the proxy adds (oxe `default_max_tokens` bounded by
    the context, the classic beta's 2048). None when the controller adds no default."""
    if not isinstance(config, dict):
        return None
    if config.get('backend_kind') == bigmoe_runtime.KIND:
        return openai_api.FLASH_MAX_TOKENS
    try:
        value = ctl.request_defaults(config, {}, '/v1/chat/completions')
    except (ValueError, TypeError, KeyError):
        return None
    limit = value.get('max_tokens') if isinstance(value, dict) else None
    return limit if type(limit) is int and limit > 0 else None


SNAPSHOT_KEEP = 2  # the Rust engine's H4 limit per session (engine crates/server cache::MAX_SNAP, /props oxe_hints)


def _args(config, key):
    args = config.get(key)
    return [a for a in args if isinstance(a, str)] if isinstance(args, list) else []


def _flag(args, name):
    """The value after the last `name` in an argv list, else None."""
    value = None
    for index, arg in enumerate(args[:-1]):
        if arg == name:
            value = args[index + 1]
    return value


def engine_family(config):
    """'rust-flash' (backend_kind oxe with an `oxe_flash` block), 'rust' (oxe), 'classic-flash' (the
    C++ BigMoe adapter, bigmoe-cpu-stream), 'classic' (llama.cpp) or 'unknown'."""
    kind = (config.get('backend_kind') or 'llama') if isinstance(config, dict) else None
    if kind == oxe_runtime.KIND:
        return 'rust-flash' if oxe_runtime.is_flash(config) else 'rust'
    if kind == bigmoe_runtime.KIND:
        return 'classic-flash'
    return 'classic' if kind == 'llama' else 'unknown'


def _mtp(config, family):
    """True / False from the profile's engine flags; None = the engine's own per-model default
    (decided at load, e.g. by an MTP head next to the model file)."""
    if family in ('rust-flash', 'classic-flash', 'unknown'):
        return False  # no Flash engine in this tree has MTP (F-MTP6/F-REL7); an unknown engine: none claimed
    args = _args(config, 'oxe_args' if family == 'rust' else 'extra_args')
    spec = _flag(args, '--spec-type')
    if spec is not None:
        return spec == 'draft-mtp'
    if family == 'rust':
        return True if '--mtp' in args else None
    return False


def capabilities(config, context):
    """`model_limits[id].capabilities` (v0.2.0 EG2): what a client may use with this model, known
    BEFORE it loads (metadata from the resolved profile; never loads a model). Generic, one rule
    per engine family (`engine_family`):
    - `prefill_only` (H6 `max_tokens: 0`), `snapshot_hint` (H4 `X-Oxe-Snapshot: keep`: checkpoints
      kept per session, 0 = no hint), `turn_kind` (H5 label accepted), `hint_clear`
      (`/_local/cache/clear {"scope": "hint"}`, EG5): the Rust engine's (MoE and Flash), as its
      `/props` `oxe_hints` lists them; the classic llama.cpp engine, the C++ Flash adapter and an
      unknown engine claim none.
    - `thinking`: "toggle" (on/off; no levels or budgets). `preserve_thinking`: the controller
      keeps reasoning in the replayed history (Rust MoE engine only).
    - `mtp`: speculative MTP decoding on (True) / off (False) / the engine's per-model default (None).
      For the loaded model, the engine's own report wins: a requested draft-mtp the model cannot run
      (no MTP block) is `mtp: False` plus `mtp_fallback` (the engine's reason, from /props.speculative).
    - `max_context`: the largest context this model can run in (the largest available mode's cap,
      else its cap); `cap` above stays the context of its current mode.
    - `vision` False, `slots` 1, `park`: where a parked session's state lives ("ram+disk" Rust MoE,
      "disk" Rust Flash, "none" otherwise)."""
    family = engine_family(config)
    rust = family in ('rust', 'rust-flash')
    caps = [row.get('cap') for row in context.get('modes') or () if row.get('available') and type(row.get('cap')) is int]
    max_context = max(caps) if caps else context.get('cap')
    return {'prefill_only': rust, 'snapshot_hint': SNAPSHOT_KEEP if rust else 0, 'turn_kind': rust, 'hint_clear': rust,
            'thinking': 'toggle', 'preserve_thinking': family == 'rust', 'mtp': _mtp(config, family),
            'max_context': max_context, 'vision': False, 'slots': 1,
            'park': {'rust': 'ram+disk', 'rust-flash': 'disk'}.get(family, 'none'),
            # Wave D D4: the client-neutral, per-model capability names (also on each /v1/models row).
            'remote': False, 'remote_provider': None, 'echo': rust,
            'cache_hints': rust, 'cancel': True, 'prefill': rust, 'context_window': max_context,
            'sampling': sampling_controls(family)}


def sampling_controls(family, engine=None):
    """`capabilities.sampling` (v0.2.0): the OpenAI sampling controls (presence_penalty,
    frequency_penalty, logit_bias) the model's engine applies, as a sorted list. The Rust engine
    reports them itself once it runs (`engine`: oxe_runtime.sampling_support of its /props); before
    that it is None (unknown: the controller forwards them and lets the engine decide). The classic
    llama.cpp engine has all three; the C++ Flash adapter none."""
    if engine is not None:
        return sorted(engine)
    if family == 'classic':
        return sorted(oxe_runtime.SAMPLING_CONTROLS)
    if family == 'classic-flash':
        return []
    return None


class ModelsMixin:
    """Manager: profiles, catalog and persisted settings."""

    def config(self, alias=None):
        try:
            value = json.loads(ctl.CONFIG.read_text())
            if not isinstance(value, dict): raise ValueError()
            base_alias = value.get('model_alias', 'qwen-local')
            profiles = value.get('profiles', {})
            selected = value.get('selected_model', base_alias)
            # local-default / local-heavy / openai/<name>: static names only (openai_api.py)
            alias = openai_api.resolve_model(alias, value)
            requested = selected if alias in (None, '', 'qwen-local') else alias
            if requested != base_alias and requested not in profiles:
                if requested in classic_beta.RETIRED_ALIASES:
                    raise Unavailable('This local model has been retired.')
                if profiles: raise Unavailable('Unknown local model. Choose one listed in /v1/models.')
                requested = base_alias
            selected_profile = profiles.get(requested, {})
            merged = {**value, **selected_profile, 'model_alias': requested}
            # Unlike legacy shared defaults, execution overrides are per model.
            if requested != base_alias and 'memlat_overrides' not in selected_profile:
                merged.pop('memlat_overrides', None)
            # V013: the model's stored context mode (fast/deep/max; max raises the engine cap
            # to 262144 only with an RSS proof, else deep).
            return profile_policy.apply_mode(profile_policy.resolve(merged),
                                             profile_policy.stored_mode(value, requested))
        except (OSError, ValueError):
            raise Unavailable('Local model setup is still being prepared.')

    def cloud_only(self):
        """#70: the install has no local engine (`"cloud_only": true` in the runtime config)."""
        try: return json.loads(ctl.CONFIG.read_text()).get('cloud_only') is True
        except (OSError, ValueError, AttributeError): return False

    def eligible(self, config):
        if classic_beta.is_retired(config): return False
        if config.get('backend_kind') == oxe_runtime.KIND:
            return oxe_runtime.eligible(config)
        if classic_beta.is_beta(config):
            return classic_beta.eligible(config)
        if config.get('backend_kind') not in (None, 'llama', bigmoe_runtime.KIND): return False
        if config.get('backend_kind') == bigmoe_runtime.KIND:
            return bigmoe_runtime.eligible(config)
        return bool(config.get('ready'))

    def models(self, remote=False):
        value = json.loads(ctl.CONFIG.read_text())
        if value.get('cloud_only') is True:  # no local rows; only the remote catalog
            return [cloud.row(item) for item in cloud.models(value)] if cloud.enabled(value) and not cloud.offline() and not remote else []
        aliases = [value.get('model_alias', 'qwen-local'), *value.get('profiles', {})]
        if value.get('profiles'): aliases.insert(0, 'qwen-local')
        rows = [{'id': alias, 'object': 'model', 'created': 0, 'owned_by': 'local',
                 'name': model_catalog.SELECTED_ALIAS_NAME if alias == 'qwen-local' else model_catalog.display_name(alias, self.config(alias))} for alias in dict.fromkeys(aliases)
                if self.eligible(self.config(alias))]
        models = openai_api.model_rows(value, rows, value.get('selected_model', value.get('model_alias', 'qwen-local')))
        self._publish_limits(models, value)
        if cloud.enabled(value) and not cloud.offline() and not remote:  # never to a LAN-share guest
            models.extend(cloud.row(item) for item in cloud.models(value))
        return models

    def _publish_limits(self, rows, value):
        """v0.2.0 M1: every `/v1/models` row (and `/v1/models/{id}`) carries the limits a generic
        client reads there: `context_length` (the context of the model's current mode) and
        `max_output_tokens` (the output default the controller applies when a request has no
        length field; null = none). Extra fields are legal in the OpenAI model object. An alias
        row gets its target's numbers. Metadata only; never loads a model."""
        limits = {}
        for row in rows:
            target = row.get('alias_of') or row.get('id')
            if not isinstance(target, str):
                continue
            if target not in limits:
                try:
                    config = self.config(target)
                    cap = self.context_block(config, value).get('cap')
                    if type(cap) is not int:
                        cap = config.get('context_size') if type(config.get('context_size')) is int else None
                    caps = capabilities(config, self.context_block(config, value))
                    limits[target] = {'context_length': cap, 'max_output_tokens': output_limit(config),
                                      'capabilities': caps}
                except (Unavailable, OSError, ValueError, KeyError, TypeError, AttributeError):
                    limits[target] = None
            if limits[target] is not None:
                row.update(limits[target])
                if isinstance(limits[target].get('capabilities'), dict):
                    row.setdefault('context_window', limits[target]['capabilities'].get('context_window'))

    def catalog(self, remote=False):
        """Chooser rows for eligible profiles; metadata only, never loads a model."""
        value = json.loads(ctl.CONFIG.read_text())
        if not isinstance(value, dict): raise ValueError('runtime_object_required')
        base = value.get('model_alias', 'qwen-local')
        profiles = value.get('profiles') if isinstance(value.get('profiles'), dict) else {}
        selected = self.config().get('model_alias')
        # Unlocked snapshot: status polling must not wait for a model load.
        loaded, state = self.loaded_config, self.state
        rows = []
        for alias in dict.fromkeys([base, *profiles]):
            try: config = self.config(alias)
            except Unavailable: continue
            if not self.eligible(config): continue
            raw = profiles.get(alias, value if alias == base else {})
            rows.append(model_catalog.row(alias, raw, config, selected=selected,
                                          loaded_alias=loaded.get('model_alias') if loaded else None, state=state,
                                          flash_cache=(getattr(self, 'flash_cache_status', None) if loaded and
                                              loaded.get('model_alias') == alias else None)))
        rows.sort(key=model_catalog.sort_key)
        if cloud.enabled(value) and not cloud.offline() and not remote:
            rows.extend(cloud.catalog_row(item) for item in cloud.models(value))
        return {'object': 'list', 'default_model': model_catalog.DEFAULT_ALIAS,
                'selected_model': selected, 'data': rows}

    def cloud_status(self):
        """The privacy switch, budget and secret readiness.  Never the key or its path."""
        try:
            value = json.loads(ctl.CONFIG.read_text())
            if not isinstance(value, dict):
                value = {}
        except (OSError, ValueError):
            value = {}
        try:
            cap, spend = cloud.budget(value)
            ledger_error = None
        except cloud.CloudError as error:
            # Status must stay alive: report the accounting fault instead of a 500, and cloud
            # admission will refuse anyway until the ledger is repaired.
            cap, spend, ledger_error = None, None, error.code
        latched = cloud.ledger_latched()
        if latched and ledger_error is None:
            ledger_error = 'cloud_ledger_unavailable'
        return {'enabled': cloud.enabled(value), 'offline': cloud.offline(),
                'daily_budget_usd': cap, 'spend_today_usd': spend, 'ledger_ok': ledger_error is None,
                'ledger_latched': latched,
                'ledger_error': ledger_error,
                'providers': cloud.configured(value), 'models': len(cloud.models(value)),
                'rejected': cloud.rejected(value),
                'subscription': cloud.subscription_status(value)}

    def flash_qualification(self):
        """M3: Flash's derived status (model_catalog.qualification of the resolved installed
        profile), or None when there is no Flash profile or it is not on the Rust engine."""
        try:
            config = self.config(model_catalog.FLASH_ALIAS)
        except Unavailable:
            return None
        if config.get('model_alias') != model_catalog.FLASH_ALIAS or not oxe_runtime.is_flash(config):
            return None
        return model_catalog.qualification(config)

    def select(self, alias):
        if not isinstance(alias, str): raise Unavailable('Choose a local model.')
        if not self.mu.acquire(blocking=False): raise Unavailable('A model is loading or a request is running. Try again when it finishes.')
        try:
            if self.active: raise Unavailable('A request is running, so the model was not changed. Try again when it finishes.')
            chosen_config = self.config(alias)
            if classic_beta.is_retired(chosen_config):
                raise Unavailable('This local model has been retired.')
            if classic_beta.is_beta(chosen_config) and not self.eligible(chosen_config):
                raise Unavailable('This model is not verified on this machine yet. Pick another model.')
            if chosen_config.get('backend_kind') == oxe_runtime.KIND and not self.eligible(chosen_config):
                raise Unavailable(oxe_runtime.ineligible_reason(chosen_config) or 'The Rust engine is not enabled and ready.')
            if chosen_config.get('backend_kind') == bigmoe_runtime.KIND and not self.eligible(chosen_config):
                raise Unavailable('Flash is not ready on this machine or does not fit in memory. '
                                  'Run `oxc status` to see why.')
            chosen = chosen_config['model_alias']
            value = json.loads(ctl.CONFIG.read_text())
            if chosen != value.get('model_alias') and chosen not in value.get('profiles', {}):
                raise Unavailable('That model is not installed. Choose one listed in Local AI.')
            ctl.guard_production_path(ctl.CONFIG)
            if self.loaded_config and self.loaded_config['model_alias'] != chosen: self.stop_for_control()
            value['selected_model'] = chosen
            ctl.guard_production_path(ctl.CONFIG)
            lan_share.write_config(ctl.CONFIG, value)
            self.cache.pop('/props', None)
            return chosen
        finally: self.mu.release()

    def context_block(self, config, runtime=None):
        """`status.context` for one resolved profile (V013-PLAN sec. 3)."""
        runtime = self.runtime_value() if runtime is None else runtime
        alias = config.get('model_alias') if isinstance(config, dict) else None
        return profile_policy.context_status(config or {}, profile_policy.stored_mode(runtime, alias))

    def model_limits(self, runtime=None):
        """`status.model_limits`: the limits of every model a client can name, keyed by each
        `/v1/models` id (an alias row such as `local-default` gets its target's limits), so a
        client knows them BEFORE its first request to a model that is not loaded. Metadata only;
        never loads a model. Per id: `{cap, mode, modes, max_output, capabilities}` where `cap`, `mode` and
        `modes` (each with its `compact_at`) are exactly `status.context` for that model, and
        `max_output` is `output_limit` (None = the controller sets no output default; a client
        then derives its own, e.g. min(16384, cap // 4)) and `capabilities` is `capabilities()`."""
        runtime = self.runtime_value() if runtime is None else runtime
        if runtime is None:
            return {}
        try:
            rows = self.models()
        except (OSError, ValueError, Unavailable):
            return {}
        try:
            value = json.loads(ctl.CONFIG.read_text())
        except (OSError, ValueError):
            value = {}
        limits, out = {}, {}
        for row in rows:
            target = row.get('alias_of') or row.get('id')
            if not isinstance(target, str):
                continue
            if target not in limits:
                remote = cloud.model(value, target)
                if remote is not None:
                    caps = cloud.capabilities(remote)
                    limits[target] = {'cap': remote['context_window'], 'mode': None, 'modes': [],
                                      'max_output': remote['max_output'], 'capabilities': caps,
                                      'context_window': remote['context_window']}
                    out[row['id']] = limits[target]
                    continue
                try:
                    config = self.config(target)
                    context = self.context_block(config, runtime)
                    caps = capabilities(config, context)
                    loaded = self.loaded_config if getattr(self, 'process', None) is not None else None
                    spec = getattr(self, 'engine_speculative', None)
                    if (isinstance(loaded, dict) and loaded.get('model_alias') == target and isinstance(spec, dict)
                            and spec.get('fallback')):
                        # the engine runs without the requested MTP drafts (no MTP block in the model)
                        caps = {**caps, 'mtp': False, 'mtp_fallback': str(spec['fallback'])[:300]}
                    engine_sampling = self.engine_sampling_for(config)
                    if engine_sampling is not None:
                        caps = {**caps, 'sampling': sorted(engine_sampling)}
                    limits[target] = {**context, 'max_output': output_limit(config), 'capabilities': caps}
                except Unavailable:
                    limits[target] = None
            if limits[target] is not None:
                out[row['id']] = limits[target]
        return out

    def engine_sampling_for(self, config):
        """The sampling controls the RUNNING engine of this resolved profile reports (a frozenset),
        or None when that model is not the loaded one or its engine did not say."""
        loaded = self.loaded_config if getattr(self, 'process', None) is not None else None
        engine = getattr(self, 'engine_sampling', None)
        if (isinstance(loaded, dict) and isinstance(config, dict) and loaded.get('model_alias') == config.get('model_alias')
                and config.get('backend_kind') == oxe_runtime.KIND and isinstance(engine, frozenset)):
            return engine
        return None

    def set_mode(self, alias, mode):
        """POST /_local/mode: persist `mode` for `alias` in runtime.json `context_modes.models`.
        Returns the model's new `context`. Unavailable (409) for a mode this model cannot run.
        fast/deep change only the published compact_at (a client applies it to its next session); a cap
        change (to or from max) unloads that model when it is loaded and idle, so its next load
        runs the new cap."""
        if not isinstance(alias, str) or not alias:
            raise Unavailable('Name the model.')
        if mode not in harness_registry.MODES:
            raise Unavailable('Mode must be fast, deep or max.')
        if not self.mu.acquire(timeout=2): raise Unavailable('A model is loading; change the mode when it is ready.')
        try:
            config = self.config(alias)
            rows = {row['id']: row for row in profile_policy.mode_rows(config)}
            if not rows:
                raise Unavailable('This model has no context modes.')
            if not rows[mode]['available']:
                raise Unavailable(rows[mode]['reason'] or 'This mode is unavailable.')
            loaded = self.loaded_config if self.process is not None else None
            cap_change = (loaded is not None and loaded.get('model_alias') == config['model_alias']
                          and loaded.get('context_size') != profile_policy.apply_mode(config, mode).get('context_size'))
            if cap_change and self.active:
                raise Unavailable('A request is running, so the mode was not changed. Try again when it finishes.')
            # v0.1.3 integration: the same writer as `share` (LAN M5): the file keeps its permission
            # bits and a symlinked runtime.json stays a symlink; P56 guard on both paths first
            saved = {}
            def change(value):
                saved['value'] = profile_policy.with_stored_mode(value, config['model_alias'], mode)
                value.clear(); value.update(saved['value'])
            try:
                lan_share.update_config(ctl.CONFIG, change, ctl.guard_production_path)
                value = saved['value']
            except (OSError, ValueError):
                raise Unavailable('The mode could not be saved.')
            if cap_change:
                self.stop_for_control()  # the next request loads the model at the new cap
            self.cache.pop('/props', None)
            return self.context_block(self.config(config['model_alias']), value)
        finally: self.mu.release()

    def harness_learning(self):
        """The opt-in as it is in runtime.json now (a loaded model keeps an older snapshot)."""
        try: return learning_settings(json.loads(ctl.CONFIG.read_text()))
        except (OSError, ValueError): return learning_settings(None)

    def harness_status(self, config):
        learning = self.harness_learning()
        with self.harness_mu:
            return {'harnesses': self.harness_registry.status(config), 'request_compat': dict(self.compat_stats),
                    'harness_learning': self.harness_registry.learning_status(learning)}

    def set_harness_learning(self, enabled, min_requests=None):
        """Persist the opt-in (runtime.json `harness_learning`); nothing else is changed."""
        if type(enabled) is not bool or (min_requests is not None and (type(min_requests) is not int or not 1 <= min_requests <= 100)):
            raise Unavailable('Send {"enabled": true or false} and optionally "min_requests" (1-100).')
        # runtime.json is rewritten under mu (like select); mu is held for a whole model load.
        if not self.mu.acquire(timeout=2): raise Unavailable('A model is loading; change the setting when it is ready.')
        try:
            try:
                value = json.loads(ctl.CONFIG.read_text())
                if not isinstance(value, dict): raise ValueError()
                current = value.get('harness_learning') if isinstance(value.get('harness_learning'), dict) else {}
                value['harness_learning'] = {**current, 'enabled': enabled,
                                             **({'min_requests': min_requests} if min_requests is not None else {})}
                ctl.guard_production_path(ctl.CONFIG)
                lan_share.write_config(ctl.CONFIG, value)
            except (OSError, ValueError):
                raise Unavailable('The harness learning setting could not be saved.')
            return learning_settings(value)
        finally: self.mu.release()

    def prepare_harness(self, harness, model_alias=None):
        """Build or load the stored prefixes of one harness (explicit user action: this
        loads the model). One 1-token primer per known prefix, through the normal queue."""
        if not isinstance(harness, str) or not harness:
            raise Unavailable('Name the harness to prepare.')
        target = self.config(model_alias)
        if target.get('backend_kind') != oxe_runtime.KIND:
            raise Unavailable('Harness prefixes are prepared on the Rust engine models only.')
        if harness == '*':
            # "Prepare all" (after an engine update): every harness whose newest prefix is
            # not expected warm for this model, one after the other through the queue.
            with self.harness_mu:
                names = self.harness_registry.harness_names(target, cold_only=True)
            done = []
            for name in names:
                try: done.append(self.prepare_harness(name, target['model_alias']))
                except Unavailable: pass  # no usable primer for this one
            return {'harness': '*', 'model': target['model_alias'], 'harnesses': done,
                    'prepared': [row for item in done for row in item['prepared']]}
        with self.harness_mu:
            primers = self.harness_registry.primers(harness, target['model_alias'])
        if not primers:
            raise Unavailable('No recorded prefix for this harness yet; run it once against the local model.')
        prepared = []
        for key, body in primers:
            self.begin_queued(inference=True, allow_start=True, model_alias=target['model_alias'])
            try:
                started = time.monotonic()
                reply = self.backend_json('/v1/chat/completions', oxe_runtime.request_defaults(body, '/v1/chat/completions', target))
                timings = reply.get('timings') if isinstance(reply.get('timings'), dict) else {}
                with self.harness_mu:
                    self.harness_registry.complete(key, self.loaded_config or target, timings.get('cache_n'), timings.get('prompt_n'))
                prepared.append({'key': key[:12], 'cache_n': timings.get('cache_n'), 'prompt_n': timings.get('prompt_n'),
                                 'seconds': round(time.monotonic() - started, 2)})
            finally:
                self.end(inference=True)
        return {'harness': harness, 'model': target['model_alias'], 'prepared': prepared}
