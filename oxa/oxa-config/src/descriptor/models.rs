//! Data: the Models and thinking group (issue #1489, design section 1.2, slice A2b). Sub-groups:
//! Model, Thinking, Sampling, Speed, Requests and retries.

use super::{d, Apply, Control, Descriptor, Group, MapKind, Options, Unit, WebEdit};

/// The Models and thinking entries, in display order.
pub const ENTRIES: &[Descriptor] = &[
    d(
        "defaultModel",
        Group::Models,
        "Model",
        "Default model",
        "Model a new session starts with.",
    )
    .control(Control::Model)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "enabledModels",
        Group::Models,
        "Model",
        "Models in the cycle",
        "Models for Ctrl+P and the start model; patterns allowed.",
    )
    .control(Control::MultiSelect(Options::Models))
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.engine.inProcessCloud",
        Group::Models,
        "Model",
        "Built-in cloud gateway",
        "Use the Rust cloud gateway inside oxa.",
    )
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Cloud model requests will run inside this oxa process."),
    d(
        "oxa.engine.inProcessLocal",
        Group::Models,
        "Model",
        "Built-in local engine",
        "Run the local engine inside oxa.",
    )
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Local model runs will start inside this oxa process."),
    d(
        "defaultThinkingLevel",
        Group::Models,
        "Thinking",
        "Thinking level",
        "Thinking level of a new session.",
    )
    .choice(&[
        ("off", "no thinking"),
        ("minimal", "shortest reasoning"),
        ("low", ""),
        ("medium", ""),
        ("high", ""),
        ("xhigh", ""),
        ("max", "longest reasoning, slowest replies"),
    ])
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "modelThinkingLevels",
        Group::Models,
        "Thinking",
        "Thinking per model",
        "A thinking level for one model id.",
    )
    .control(Control::Map(MapKind::Text))
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.thinking.policy",
        Group::Models,
        "Thinking",
        "Thinking policy",
        "Fixed level, or let oxa turn thinking on when a run needs it.",
    )
    .choice(&[("fixed", ""), ("auto", "")])
    .eff("\"auto\"")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.thinking.toolFollowups",
        Group::Models,
        "Thinking",
        "Think after tool results",
        "Thinking on replies that follow tool results, under policy auto.",
    )
    .choice(&[("auto", ""), ("on", ""), ("off", "")])
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.thinking.escalation.fruitlessCalls",
        Group::Models,
        "Thinking",
        "Think after stuck calls",
        "Calls without progress before thinking turns on. 0 = never.",
    )
    .number(Unit::Count)
    .zero("never")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.thinking.budget",
        Group::Models,
        "Thinking",
        "Thinking token cap",
        "Cap on reasoning tokens for local engines that support it. Unset = no cap.",
    )
    .number(Unit::Tokens)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.sampling.mode",
        Group::Models,
        "Sampling",
        "Sampling",
        "Greedy (deterministic) or the model card's sampling.",
    )
    .choice(&[
        ("greedy", "deterministic"),
        ("model", "the model card's sampling"),
    ])
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.sampling.seed",
        Group::Models,
        "Sampling",
        "Seed",
        "Fixed seed, sent with model sampling. Unset = none.",
    )
    .control(Control::Number)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "cacheWarming",
        Group::Models,
        "Speed",
        "Cache warming",
        "Keep the prompt cache warm while idle.",
    )
    .choice(&[("off", ""), ("streaming", ""), ("idle", "")])
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.bootPreload",
        Group::Models,
        "Speed",
        "Preload at start",
        "Prefill the fixed prompt prefix when oxa starts.",
    )
    .toggle()
    .apply(Apply::Restart)
    .web(WebEdit::Edit),
    d(
        "oxa.prefill.paste",
        Group::Models,
        "Speed",
        "Prefill while typing",
        "Prefill a long message while you type, on a local engine.",
    )
    .toggle()
    .eff("true")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.prefill.tools",
        Group::Models,
        "Speed",
        "Prefill during tools",
        "Prefill earlier tool results while a slow tool runs.",
    )
    .toggle()
    .eff("true")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "retry.enabled",
        Group::Models,
        "Requests and retries",
        "Retry failed requests",
        "Retry a failed model request.",
    )
    .toggle()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "retry.maxRetries",
        Group::Models,
        "Requests and retries",
        "Retries",
        "Retries of one agent request.",
    )
    .control(Control::Number)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "retry.baseDelayMs",
        Group::Models,
        "Requests and retries",
        "First retry delay",
        "First wait, doubled each retry.",
    )
    .number(Unit::Ms)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "retry.maxAgentDelayMs",
        Group::Models,
        "Requests and retries",
        "Longest retry wait",
        "Longest wait between retries.",
    )
    .number(Unit::Ms)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.emptyReplyRetries",
        Group::Models,
        "Requests and retries",
        "Empty reply retries",
        "Ask again when a cloud model returns no text and no tool call. 0 = off.",
    )
    .number(Unit::Count)
    .zero("off")
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "retry.provider.maxRetries",
        Group::Models,
        "Requests and retries",
        "Silent HTTP retries",
        "Retries of one HTTP request before the first byte.",
    )
    .control(Control::Number)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "retry.provider.maxRetryDelayMs",
        Group::Models,
        "Requests and retries",
        "Longest HTTP retry wait",
        "Longest wait of one HTTP retry.",
    )
    .number(Unit::Ms)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "httpIdleTimeoutMs",
        Group::Models,
        "Requests and retries",
        "Stream silence limit",
        "How long the model stream may be silent. 0 = no limit.",
    )
    .number(Unit::Ms)
    .control(Control::NumberOr("disabled"))
    .zero("no limit")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "retry.provider.timeoutMs",
        Group::Models,
        "Requests and retries",
        "Stream silence limit (provider)",
        "Overrides the stream silence limit for provider requests. Unset = use the limit above.",
    )
    .number(Unit::Ms)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::control_fits;
    use crate::schema::find_key;

    #[test]
    fn models_keys_exist_in_schema() {
        let mut seen = std::collections::HashSet::new();
        for e in ENTRIES {
            assert_eq!(e.group, Group::Models, "{}", e.key);
            assert!(find_key(e.key).is_some(), "not in schema: {}", e.key);
            assert!(seen.insert(e.key), "duplicate key: {}", e.key);
            assert!(e.label.chars().count() <= 32, "label too long: {}", e.label);
            assert!(
                e.help.chars().count() <= 100 && !e.help.contains('\n'),
                "help: {}",
                e.key
            );
        }
    }

    #[test]
    fn models_controls_match_kinds() {
        for e in ENTRIES {
            let kind = find_key(e.key).unwrap().kind;
            assert!(
                control_fits(e.control, kind),
                "{}: {:?} vs {:?}",
                e.key,
                e.control,
                kind
            );
        }
    }
}
