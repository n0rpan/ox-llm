//! Data: the Compaction group (sub-groups Automatic, When, Summaries) and the Codemode group
//! (issue #1489, design section 1.2, slice A2b). Codemode has no sub-group.

use super::{d, Apply, Control, Descriptor, Group, Unit, WebEdit};
use crate::schema::Kind;

/// The Compaction entries, then the Codemode entries, in display order.
pub const ENTRIES: &[Descriptor] = &[
    d(
        "compaction.profile",
        Group::Compaction,
        "Automatic",
        "Context profile",
        "Auto picks cloud for a cloud model. Cloud keeps a larger recent context and compacts later.",
    )
    .key(Kind::Enum(&["auto", "local", "cloud"]), "\"auto\"")
    .choice(&[("auto", ""), ("local", ""), ("cloud", "")])
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "compaction.enabled",
        Group::Compaction,
        "Automatic",
        "Auto-compaction",
        "Compact automatically; off keeps only the emergency compaction.",
    )
    .key(Kind::Bool, "true")
    .toggle()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.compaction.softTokens",
        Group::Compaction,
        "Automatic",
        "Compact at",
        "Context size that starts a compaction. Unset = the engine's point.",
    )
    .key(Kind::Number, "null")
    .number(Unit::Tokens)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "compaction.reserveTokens",
        Group::Compaction,
        "Automatic",
        "Reserved room",
        "Room kept free for the summary and the next reply.",
    )
    .key(Kind::Number, "16384")
    .number(Unit::Tokens)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "compaction.keepRecentTokens",
        Group::Compaction,
        "Automatic",
        "Recent context kept",
        "Recent context kept word for word after a compaction.",
    )
    .key(Kind::Number, "4000")
    .number(Unit::Tokens)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "compaction.modelOverrides",
        Group::Compaction,
        "Automatic",
        "Per-model values",
        "Reserved room and recent context for one model.",
    )
    .key(Kind::Object, "null")
    .control(Control::Raw)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    // L5 mid-run: unset = on.
    d(
        "oxa.compaction.midRun",
        Group::Compaction,
        "When",
        "Compact mid-run",
        "Compact between tool steps when the context passes the point.",
    )
    .key(Kind::Bool, "null")
    .toggle()
    .eff("true")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    // L5: unset = on.
    d(
        "oxa.compaction.turnStart",
        Group::Compaction,
        "When",
        "Compact before a prompt",
        "Compact first when the context is already above the point.",
    )
    .key(Kind::Bool, "null")
    .toggle()
    .eff("true")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    // V020-OPTIMIZE #7: unset = on.
    d(
        "oxa.compaction.idleSummary",
        Group::Compaction,
        "When",
        "Summarise while idle",
        "Write the next summary in idle time so compaction is instant.",
    )
    .key(Kind::Bool, "null")
    .toggle()
    .eff("true")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    // V020-OPTIMIZE #7: soft length target of a summary in tokens; unset = 700; 0 = none.
    d(
        "oxa.compaction.summaryTarget",
        Group::Compaction,
        "Summaries",
        "Summary length",
        "Soft length target of a summary. 0 = no target.",
    )
    .key(Kind::Number, "null")
    .number(Unit::Tokens)
    .eff("700")
    .zero("no target")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "branchSummary.reserveTokens",
        Group::Compaction,
        "Summaries",
        "Branch summary budget",
        "Output budget of a branch summary.",
    )
    .key(Kind::Number, "16384")
    .number(Unit::Tokens)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "branchSummary.skipPrompt",
        Group::Compaction,
        "Summaries",
        "Skip the branch question",
        "Do not ask \"summarise the branch?\".",
    )
    .key(Kind::Bool, "false")
    .toggle()
    .apply(Apply::Now)
    .web(WebEdit::Edit),
    // oxa: `codemode.enabled`, `codemode.toolSearch` and `codemode.maxWorkers` turn on the
    // built-in codemode / tool-search hook sets and size the script pool (oxa-cli hooks.rs);
    // pi activates them through `defaultTools`, which oxa honours as well.
    d(
        "codemode.enabled",
        Group::Codemode,
        "",
        "Codemode",
        "The codemode tool: the model writes a script that calls tools.",
    )
    .key(Kind::Bool, "false")
    .toggle()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "codemode.mode",
        Group::Codemode,
        "",
        "Codemode only",
        "`only` hides every other tool while codemode is on.",
    )
    .key(Kind::Enum(&["on", "only"]), "\"on\"")
    .choice(&[("on", ""), ("only", "")])
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "codemode.toolSearch",
        Group::Codemode,
        "",
        "Tool search",
        "The tool_search and call_tool tools.",
    )
    .key(Kind::Bool, "false")
    .toggle()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "codemode.maxWorkers",
        Group::Codemode,
        "",
        "Script workers",
        "Script workers alive at once. 0 = 2.",
    )
    .key(Kind::Number, "0")
    .control(Control::Number)
    .zero("two workers")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::control_fits;
    use crate::schema::find_key;

    #[test]
    fn compaction_keys_exist_in_schema() {
        let compaction = ENTRIES.iter().filter(|e| e.group == Group::Compaction).count();
        let codemode = ENTRIES.iter().filter(|e| e.group == Group::Codemode).count();
        assert_eq!((compaction, codemode), (12, 4));
        let mut seen = std::collections::HashSet::new();
        for e in ENTRIES {
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
    fn compaction_controls_match_kinds() {
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

    #[test]
    fn model_overrides_use_a_control_that_preserves_nested_objects() {
        let descriptor = crate::descriptor::descriptor("compaction.modelOverrides").unwrap();
        assert_eq!(descriptor.control, Control::Raw);
    }
}
