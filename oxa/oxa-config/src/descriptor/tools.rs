//! Data for the Tools and bash group (22) and the Workers group (5, the subagent keys), issue
//! #1489, design section 1.2. The tests here check both groups and the Budgets data.

use super::*;

/// The Tools and bash entries, then the Workers entries, in display order.
pub(super) const ENTRIES: &[Descriptor] = &[
    d(
        "defaultTools",
        Group::Tools,
        "Tools",
        "Tools on by default",
        "Tools the model gets in a new session.",
    )
    .control(Control::MultiSelect(Options::Tools))
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.toolOutputCap",
        Group::Tools,
        "Tools",
        "Tool output cap",
        "Most bytes of one tool result sent to the model.",
    )
    .number(Unit::Bytes)
    .advise_from(2048, 1024, "under 2048 bytes a tool result is cut short")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.toolRepair",
        Group::Tools,
        "Tools",
        "Repair bad tool calls",
        "One extra turn to repair a malformed tool call.",
    )
    .toggle()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.toolRepeatGuard",
        Group::Tools,
        "Tools",
        "Repeat guard",
        "Note, then stop, when a tool call repeats with the same result.",
    )
    .toggle()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.diet.output",
        Group::Tools,
        "Tools",
        "Concise answers",
        "Prompt rules for short final answers.",
    )
    .toggle()
    .eff("true")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.diet.tools",
        Group::Tools,
        "Tools",
        "Shorter tool output",
        "Compact grep and bash output.",
    )
    .toggle()
    .eff("true")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.envNote",
        Group::Tools,
        "Tools",
        "Environment note",
        "Add date, time and runtime facts to prompts.",
    )
    .toggle()
    .eff("true")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.bash.defaultTimeout",
        Group::Tools,
        "Bash",
        "Bash timeout",
        "Timeout of a bash call that names none.",
    )
    .number(Unit::Seconds)
    .eff("120")
    .zero("no timeout")
    .advise(0, 3600, 30, "a hung command holds the turn that long")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "shellPath",
        Group::Tools,
        "Bash",
        "Shell",
        "Shell for bash and `!cmd`.",
    )
    .control(Control::Path)
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Bash and `!cmd` will run in this shell."),
    d(
        "shellCommandPrefix",
        Group::Tools,
        "Bash",
        "Command prefix",
        "Text put before each bash command.",
    )
    .text()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Text is put in front of every bash command."),
    d(
        "oxa.webSearch.enabled",
        Group::Tools,
        "Web search",
        "Web search",
        "The web_search tool, when a key exists.",
    )
    .toggle()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.webSearch.provider",
        Group::Tools,
        "Web search",
        "Search provider",
        "Search provider; none turns the tool off.",
    )
    .choice(&[
        ("brave", "Brave Search, needs a key"),
        ("none", "turns the tool off"),
    ])
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.webSearch.maxTokens",
        Group::Tools,
        "Web search",
        "Search result size",
        "Size of the result the provider returns.",
    )
    .number(Unit::Tokens)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "done",
        Group::Tools,
        "Checks and hooks",
        "Done checks",
        "Commands oxa runs when a run that edited files is about to finish.",
    )
    .control(Control::List(Item::Text))
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("These commands run after each run that edits files."),
    d(
        "hooks",
        Group::Tools,
        "Checks and hooks",
        "Exec hooks",
        "A program to run on an event.",
    )
    .control(Control::Raw)
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("This program runs on the chosen event."),
    d(
        "oxa.builtinSkills",
        Group::Tools,
        "Skills and extensions",
        "Built-in skills",
        "Include the skills built into oxa.",
    )
    .toggle()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.skills.includeSynced",
        Group::Tools,
        "Skills and extensions",
        "Synced skills",
        "Also list skills another product synced into ~/.agents/skills.",
    )
    .toggle()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "skills",
        Group::Tools,
        "Skills and extensions",
        "Extra skill folders",
        "Skill files and folders to load.",
    )
    .control(Control::List(Item::Path))
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Skills from these folders load into every new session."),
    d(
        "prompts",
        Group::Tools,
        "Skills and extensions",
        "Prompt template folders",
        "Prompt template files and folders to load.",
    )
    .control(Control::List(Item::Path))
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Prompt templates from these folders load into every new session."),
    d(
        "extensions",
        Group::Tools,
        "Skills and extensions",
        "Extensions",
        "Extension files and folders to load.",
    )
    .control(Control::List(Item::Path))
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Extension code from these folders runs in oxa."),
    d(
        "oxa.reportIssue.repo",
        Group::Tools,
        "Issue reports",
        "Issue report repository",
        "Repository for `oxa report-issue submit`, as OWNER/NAME.",
    )
    .text()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Issue reports will be posted to this repository."),
    d(
        "oxa.reportIssue.semanticDedupe",
        Group::Tools,
        "Issue reports",
        "Dedupe issue reports",
        "Search open and recent issues before posting a report.",
    )
    .toggle()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.subagent.enabled",
        Group::Workers,
        "Subagents",
        "Subagents",
        "The subagent tool. Auto: on, except on small models.",
    )
    .control(Control::TriState)
    .eff("\"auto\"")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.subagent.smallContext",
        Group::Workers,
        "Subagents",
        "Subagents on small models",
        "Offer subagents on a model with a window below 32K.",
    )
    .toggle()
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.subagent.remoteConcurrency",
        Group::Workers,
        "Subagents",
        "Cloud subagents at once",
        "Children of one call that run at once on cloud models.",
    )
    .number(Unit::Count)
    .advise(1, 16, 1, "more than 16 children at once is unusual")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.subagent.softTokens",
        Group::Workers,
        "Subagents",
        "Subagent compaction point",
        "Context size at which a child compacts.",
    )
    .number(Unit::Tokens)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "oxa.subagent.resultCap",
        Group::Workers,
        "Subagents",
        "Subagent result cap",
        "Most bytes a child returns.",
    )
    .number(Unit::Bytes)
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Kind;

    /// Every key of the three groups is a schema key, or a field of an object key.
    #[test]
    fn slice_keys_exist_in_schema() {
        let schema = crate::schema();
        for group in [Group::Budgets, Group::Tools, Group::Workers] {
            for e in in_group(group) {
                let owner = schema
                    .iter()
                    .filter(|k| e.key == k.key || e.key.starts_with(&format!("{}.", k.key)))
                    .max_by_key(|k| k.key.len());
                let owner = owner.unwrap_or_else(|| panic!("no schema key for {}", e.key));
                if e.key != owner.key {
                    assert_eq!(
                        owner.kind,
                        Kind::Object,
                        "field {} must sit under an object",
                        e.key
                    );
                }
            }
        }
        assert_eq!(fields_of("oxa.cloud").count(), 8);
    }

    /// The editor of each schema key matches its kind, and choices name only schema options.
    #[test]
    fn controls_match_schema_kinds() {
        let schema = crate::schema();
        for group in [Group::Budgets, Group::Tools, Group::Workers] {
            for e in in_group(group) {
                let Some(kind) = schema.iter().find(|k| k.key == e.key).map(|k| k.kind) else {
                    continue; // a field key
                };
                let fits = matches!(
                    (e.control, kind),
                    (Control::Toggle | Control::TriState, Kind::Bool)
                        | (Control::Number | Control::Decimal, Kind::Number)
                        | (Control::Choice, Kind::Enum(_))
                        | (Control::Text | Control::Path, Kind::String)
                        | (Control::MultiSelect(_) | Control::List(_), Kind::StringList)
                        | (
                            Control::Map(_) | Control::Fields | Control::Raw,
                            Kind::Object | Kind::Any
                        )
                );
                assert!(fits, "{} control {:?} does not fit {:?}", e.key, e.control, kind);
                if let Kind::Enum(options) = kind {
                    for (choice, _) in e.choices {
                        assert!(
                            options.contains(choice),
                            "{} names unknown option {}",
                            e.key,
                            choice
                        );
                    }
                }
            }
        }
    }

    /// Dynamic objects keep their nested values in the raw JSON editor, while sensitive settings
    /// remain web-editable only through review and confirmation.
    #[test]
    fn structured_and_sensitive_controls_are_preserved() {
        for key in [
            "oxa.cloud.models",
            "oxa.cloud.unpriced_discovered_pricing",
            "oxa.cloud.discovery",
            "hooks",
        ] {
            let descriptor = descriptors()
                .iter()
                .find(|descriptor| descriptor.key == key)
                .unwrap_or_else(|| panic!("missing descriptor for {key}"));
            assert_eq!(descriptor.control, Control::Raw, "{key}");
        }

        for key in ["shellPath", "shellCommandPrefix", "done", "hooks", "extensions"] {
            let descriptor = descriptors()
                .iter()
                .find(|descriptor| descriptor.key == key)
                .unwrap_or_else(|| panic!("missing descriptor for {key}"));
            assert_eq!(descriptor.web, WebEdit::Confirm, "{key}");
            assert!(!descriptor.widens.is_empty(), "{key}");
        }
    }
}
