//! C8: prefix epochs (W3). Owner: task C8. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/agent-session.ts (reload, system prompt rebuild)
use crate::agent_loop::persist;
use crate::prefix::{build_prefix_with, PrefixOpts};
use crate::toolset::{resolve_toolset, ToolSelection, Toolset};
use crate::SessionState;
use oxa_config::Settings;
use oxa_perm::{gated_resources, resolve_trust, DefaultTrust, TrustInputs, TrustOutcome, TrustStore};
use oxa_proto::{
    codes, AgentEvent, Entry, EntryBody, EventSink, FrozenPrefix, HookPoint, Level, OxaError, Source, Tool,
    ToolSpec, Ui, Value,
};
use oxa_resources::{discover, DiscoverOpts};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Everything one epoch is built from, gathered without touching the state.
struct Inputs {
    /// Built-ins plus the hook tools as read now.
    all: Vec<Arc<dyn Tool>>,
    active: Toolset,
    /// The `Direct` specs (the tool list of the prefix). Callable-only tools never reach the prefix.
    specs: Vec<ToolSpec>,
    extra: Vec<(String, String)>,
    warnings: Vec<String>,
    /// `oxa.profile`, read once for this epoch (stored with it).
    profile: oxa_proto::Profile,
    /// The tool tuning of this epoch (`tune::tune_for`).
    tune: oxa_proto::ToolTune,
}

fn gather(st: &SessionState) -> Inputs {
    // Built-ins come from the last epoch; hook tools are read again (`Hooks::tools`, W3).
    let mut all: Vec<Arc<dyn Tool>> = st
        .all_tools
        .iter()
        .filter(|t| t.spec().source == Source::Builtin)
        .cloned()
        .collect();
    let mut extra: Vec<(String, String)> = Vec::new();
    for h in &st.hooks {
        all.extend(h.tools());
        extra.extend(h.prompt_sections());
    }
    // The CLI choices survive an epoch; `defaultTools` is read from the settings as they are now.
    let sel = ToolSelection {
        default_tools: st.settings.default_tools(),
        default_edits: st.settings.default_tools_edits(),
        ..st.selection.clone()
    };
    let profile = st.settings.profile();
    let (mut active, warnings) = resolve_toolset(&all, &sel);
    crate::toolset::apply_codemode_mode(&mut active, &st.settings);
    let specs = crate::profile::declared_specs(active.specs(), profile);
    let tune = crate::tune::tune_for(profile, &st.settings, st.info.n_ctx, crate::tune::budget_soft(st));
    Inputs {
        all,
        active,
        specs,
        extra,
        warnings,
        profile,
        tune,
    }
}

/// The prefix options of an epoch of `profile`, from the settings as they are now.
pub(crate) fn prefix_opts(st: &SessionState, profile: oxa_proto::Profile) -> PrefixOpts {
    PrefixOpts {
        profile,
        diet_output: st.settings.diet_output(profile),
        child: crate::subagent::is_child_process(),
    }
}

/// Gather the inputs of `build_prefix` from the state as it is now: re-read hook tools
/// (`Hooks::tools`), resolve the toolset, collect `Hooks::prompt_sections`. Returns the
/// `Direct` specs and the extra sections. Does not modify `st`.
pub fn epoch_inputs(st: &SessionState) -> (Vec<ToolSpec>, Vec<(String, String)>) {
    let g = gather(st);
    (g.specs, g.extra)
}

/// Refuse anything that would change the prefix while a run is in progress.
fn ensure_idle(st: &SessionState) -> Result<(), OxaError> {
    if st.shared.busy.load(Ordering::SeqCst) {
        return Err(OxaError::new(
            codes::BUSY,
            "a run is in progress; the prefix changes only between runs",
        )
        .with_hint("wait for the run to settle, then /reload"));
    }
    Ok(())
}

/// Build a prefix from the current inputs and, if its sha differs from `st.prefix` (or
/// `force`, for the first epoch of a session), open a new epoch: append one `Prefix` entry,
/// `Transcript::rebase(same messages, epoch)`, update `st.prefix` / `all_tools` / `active`,
/// emit `PrefixEpoch { reprefill_tokens }` (= current context size, the cost) and a `Notice`.
/// Returns whether an epoch was opened. Only called between runs (`busy` otherwise): never
/// from a timer, a late MCP server or an extension during a run.
pub fn open_epoch(
    st: &mut SessionState,
    reason: &str,
    force: bool,
    sink: &mut dyn EventSink,
) -> Result<bool, OxaError> {
    open_epoch_at(st, reason, force, sink, None)
}

/// The printed cost of an epoch whose caller knows it better than the current context size:
/// a model switch, where the TARGET prefills the whole conversation cold (S3 / ENG-004).
pub(crate) struct EpochCost {
    pub tokens: u32,
    /// The model that pays it (named in the notice).
    pub who: Option<String>,
}

/// [`open_epoch`] with `force` and the caller's cost: `PrefixEpoch.reprefill_tokens` and the
/// one notice carry the same number, with its estimated seconds.
pub(crate) fn open_epoch_costed(
    st: &mut SessionState,
    reason: &str,
    sink: &mut dyn EventSink,
    cost: EpochCost,
) -> Result<bool, OxaError> {
    open_epoch_at(st, reason, true, sink, Some(cost))
}

fn open_epoch_at(
    st: &mut SessionState,
    reason: &str,
    force: bool,
    sink: &mut dyn EventSink,
    known: Option<EpochCost>,
) -> Result<bool, OxaError> {
    ensure_idle(st)?;
    let Inputs {
        all,
        active,
        specs,
        extra,
        warnings,
        profile,
        tune,
    } = gather(st);
    for w in warnings {
        sink.emit(&AgentEvent::Notice {
            level: Level::Warning,
            text: w,
        });
    }
    let prefix = build_prefix_with(&st.cwd, &st.resources, &specs, &extra, &prefix_opts(st, profile));
    if !force && prefix.sha256 == st.prefix.sha256 {
        // Same bytes: no epoch. Callable-only tools may still have changed, which does not
        // touch the prefix (W3).
        st.all_tools = all;
        st.active = active;
        // The page size, diet and timeout follow the window and the settings without an epoch.
        crate::tune::refresh(st);
        return Ok(false);
    }

    // The cost is the context that must be prefilled again; the rebase drops the usage.
    let cost = known
        .as_ref()
        .map_or_else(|| st.transcript.context(), |c| c.tokens);
    let sha = prefix.sha256;
    // `append_entry` folds the entry: `Transcript::apply` rebases the kept messages with cause
    // `epoch` when a prefix was already in force, so no second rebase is logged here.
    persist(st, EntryBody::Prefix(prefix.clone()), sink)?;
    if let Some(marker) = crate::profile::profile_entry(profile) {
        persist(st, marker, sink)?;
    }
    st.stats.profile = profile;
    st.tune = tune;
    st.prefix = prefix;
    st.all_tools = all;
    st.active = active;
    st.stats.epochs += 1;
    sink.emit(&AgentEvent::PrefixEpoch {
        sha256: sha,
        reason: reason.to_string(),
        reprefill_tokens: cost,
    });
    let secs = crate::prefill_estimate_secs(cost);
    let text = match known.and_then(|c| c.who) {
        Some(who) if ["anthropic/", "openai/", "openrouter/"].iter().any(|p| who.starts_with(p)) => {
            format!("new prefix epoch ({reason}): {who} gets the context of about {cost} tokens re-sent (the provider cache may miss)")
        }
        Some(who) => {
            format!("new prefix epoch ({reason}): {who} prefills about {cost} tokens cold (~{secs} s)")
        }
        None => format!("new prefix epoch ({reason}): about {cost} tokens to prefill again (~{secs} s)"),
    };
    sink.emit(&AgentEvent::Notice {
        level: Level::Info,
        text,
    });
    Ok(true)
}

/// Whether the project at `st.cwd` is trusted, from the saved decision alone. A session that was
/// started with an approve flag is not known here (the state keeps no trust input).
fn project_trusted(st: &SessionState) -> bool {
    let store = TrustStore::load(&st.dirs.trust_file());
    let inputs = TrustInputs {
        flag: None,
        extension: None,
        saved: store.lookup(&st.cwd),
        default: DefaultTrust::Ask,
        interactive: false,
        has_gated: !gated_resources(&st.cwd).is_empty(),
    };
    // B4: files changed since the user approved them (by the model, in this session or an
    // earlier one) do not load on a reload either.
    resolve_trust(&inputs) == TrustOutcome::Trusted
        && !matches!(store.approval(&st.cwd), oxa_perm::Approval::Changed(_))
}

/// `/reload`: re-read settings and resources, fire `ResourcesDiscover`, shut down and reload
/// hook sets, then [`open_epoch`]. No change in the prefix bytes = no new epoch, returns false.
pub fn reload(st: &mut SessionState, sink: &mut dyn EventSink, ui: &mut dyn Ui) -> Result<bool, OxaError> {
    ensure_idle(st)?;
    let trusted = project_trusted(st);
    let settings = Settings::load(&st.dirs, &st.cwd, trusted);
    for d in settings.diagnostics() {
        sink.emit(&AgentEvent::Notice {
            level: d.level,
            text: format!("{}: {}", d.source, d.message),
        });
    }
    // Only the trust-independent discovery inputs are known here (see `SessionState`).
    let opts = DiscoverOpts {
        trusted,
        ..DiscoverOpts::default()
    };
    // Packages, extension and theme paths included (E10).
    let resources = discover(&st.dirs, &st.cwd, &settings, &opts);
    for d in &resources.diagnostics {
        sink.emit(&AgentEvent::Notice {
            level: d.level,
            text: d.message.clone(),
        });
    }
    let prev_enabled = st.settings.compaction().enabled;
    // EXT-007 (pi /reload): the code-loaded hook sets are re-created. The paths are the ones
    // discovered now plus the earlier ones that still exist (`-e` paths are not discovered).
    let mut paths = resources.extension_paths.clone();
    for p in &st.resources.extension_paths {
        if !paths.contains(p) && p.exists() {
            paths.push(p.clone());
        }
    }
    st.settings = settings;
    st.resources = resources;
    crate::host::observe_hooks(st, HookPoint::SessionShutdown, &Value::Null, sink, ui);
    for h in st.hooks.clone() {
        h.reload(&paths);
        crate::session::report_hook_errors(h.as_ref(), sink);
    }
    crate::host::attach_hosts(st, sink, ui);
    let start = serde_json::json!({ "reason": "reload" });
    crate::host::observe_hooks(st, HookPoint::SessionStart, &start, sink, ui);
    // X3: the reloaded `compaction.*` settings (and the engine's current limits) take effect now.
    crate::session::refresh_budget(st, Some(prev_enabled), sink);
    // Hook sets are re-read by `open_epoch` (`tools`, `prompt_sections`). The reply is ignored:
    // an observer point cannot change the prefix.
    let payload = serde_json::json!({ "cwd": st.cwd.display().to_string(), "reason": "reload" });
    crate::host::observe_hooks(st, HookPoint::ResourcesDiscover, &payload, sink, ui);
    open_epoch(st, "reload", false, sink)
}

/// The prefix in force at the end of `branch`: its last `Prefix` entry, byte for byte. Resume
/// uses this and never rebuilds (so a changed AGENTS.md shows up only after `/reload`).
pub fn replay_prefix(branch: &[&Entry]) -> Option<FrozenPrefix> {
    branch.iter().rev().find_map(|e| match &e.body {
        EntryBody::Prefix(p) => Some(p.clone()),
        _ => None,
    })
}
