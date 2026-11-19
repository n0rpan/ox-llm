//! Setting descriptors: what the TUI `/settings` menu and the lead web settings page show for a key
//! (issue #1489, design section 4.1).
//!
//! This module holds the types, the `const fn` builder and the lookups. The entries themselves go
//! in `descriptor/*.rs` (one `ENTRIES` slice per file); `table()` joins them. `KeyDef` and `SCHEMA`
//! are untouched: validation and merge do not read descriptors.

mod advanced;
mod budgets;
mod compaction;
mod display;
mod lead;
mod models;
mod safety;
mod tools;

use std::sync::OnceLock;

/// A settings menu group, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    Display,
    Models,
    Budgets,
    Tools,
    Workers,
    Memory,
    Compaction,
    Codemode,
    Lead,
    Safety,
    Advanced,
}

impl Group {
    /// Every group, in display order.
    pub const ALL: [Group; 11] = [
        Group::Display,
        Group::Models,
        Group::Budgets,
        Group::Tools,
        Group::Workers,
        Group::Memory,
        Group::Compaction,
        Group::Codemode,
        Group::Lead,
        Group::Safety,
        Group::Advanced,
    ];

    /// Stable id, used in URLs (`/settings/<id>`) and tests.
    pub const fn id(self) -> &'static str {
        match self {
            Group::Display => "display",
            Group::Models => "models",
            Group::Budgets => "budgets",
            Group::Tools => "tools",
            Group::Workers => "workers",
            Group::Memory => "memory",
            Group::Compaction => "compaction",
            Group::Codemode => "codemode",
            Group::Lead => "lead",
            Group::Safety => "safety",
            Group::Advanced => "advanced",
        }
    }

    /// Heading shown on the group page.
    pub const fn title(self) -> &'static str {
        match self {
            Group::Display => "Display",
            Group::Models => "Models and thinking",
            Group::Budgets => "Budgets",
            Group::Tools => "Tools and bash",
            Group::Workers => "Workers",
            Group::Memory => "Memory",
            Group::Compaction => "Compaction",
            Group::Codemode => "Codemode",
            Group::Lead => "Lead and web",
            Group::Safety => "Safety",
            Group::Advanced => "Advanced",
        }
    }

    /// Tag of at most 8 characters, for narrow rows.
    pub const fn short(self) -> &'static str {
        match self {
            Group::Display => "Display",
            Group::Models => "Models",
            Group::Budgets => "Budgets",
            Group::Tools => "Tools",
            Group::Workers => "Workers",
            Group::Memory => "Memory",
            Group::Compaction => "Compact",
            Group::Codemode => "Codemode",
            Group::Lead => "Lead",
            Group::Safety => "Safety",
            Group::Advanced => "Advanced",
        }
    }

    /// One line shown under the group heading.
    pub const fn summary(self) -> &'static str {
        match self {
            Group::Display => "How oxa looks: theme, editor, footer and notices.",
            Group::Models => "Which models oxa uses, and how hard they think.",
            Group::Budgets => "Daily spend and token limits for metered models.",
            Group::Tools => "The tools the agent may use, and bash limits.",
            Group::Workers => "Background workers and how many run at once.",
            Group::Memory => "What oxa remembers between sessions.",
            Group::Compaction => "When and how long conversations are summarised.",
            Group::Codemode => "The code-mode sandbox and its limits.",
            Group::Lead => "The lead agent and the web page that serves it.",
            Group::Safety => "Guards on file writes, commands and network access.",
            Group::Advanced => "Rarely needed. Changing these can break a session.",
        }
    }
}

/// The editor a row opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Toggle,
    TriState,
    Choice,
    Model,
    Theme,
    Number,
    /// A number, or a word that stands for "no limit" (the word is the payload).
    NumberOr(&'static str),
    Decimal,
    Text,
    Path,
    MultiSelect(Options),
    List(Item),
    Map(MapKind),
    /// Opens a sub-page with one row per field of an object key.
    Fields,
    /// Raw JSON, read-only in the TUI, edited with Ctrl+G.
    Raw,
    /// Accepted and ignored by oxa (a verdict-O key).
    Ignored,
}

/// Where the options of a [`Control::MultiSelect`] come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Options {
    Tools,
    Models,
    Fixed(&'static [&'static str]),
}

/// The element type of a [`Control::List`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Text,
    Path,
}

/// The value type of a [`Control::Map`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapKind {
    Text,
    Number,
}

/// The unit a number is shown in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Tokens,
    Bytes,
    Ms,
    Seconds,
    Rows,
    Cells,
    Count,
    Usd,
}

/// A range for a number. `hard` ranges refuse a value outside them; advised ranges save with a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub min: u64,
    pub max: Option<u64>,
    pub hard: bool,
    pub step: u64,
    pub note: &'static str,
}

/// When a changed value takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    Now,
    Turn,
    Reload,
    Restart,
}

/// What the web page may do with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebEdit {
    /// Saved at once.
    Edit,
    /// Saved only after the review page's Confirm (class C).
    Confirm,
    /// Not editable on the web.
    Terminal,
    /// Shown, never editable.
    ReadOnly,
}

/// One row of the settings menus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Descriptor {
    /// Schema key, or schema key + "." + field path.
    pub key: &'static str,
    pub group: Group,
    /// Sub-group heading, "" for none.
    pub sub: &'static str,
    /// At most 32 characters.
    pub label: &'static str,
    /// One line, at most 100 characters.
    pub help: &'static str,
    pub control: Control,
    pub unit: Option<Unit>,
    pub range: Option<Range>,
    /// Option -> short description (the description may be "").
    pub choices: &'static [(&'static str, &'static str)],
    /// JSON text, when the schema default is null but a value is in force.
    pub effective_default: Option<&'static str>,
    /// What zero means, e.g. "no timeout". "" for none.
    pub zero_means: &'static str,
    pub advanced: bool,
    pub apply: Apply,
    /// `false` until the audit (A6) has confirmed `apply` against the code.
    pub apply_checked: bool,
    /// The project file cannot set it.
    pub user_only: bool,
    pub web: WebEdit,
    /// Consequence sentence for the review page and the TUI notice.
    pub widens: &'static str,
    /// Masked on display.
    pub secret: bool,
}

/// Starts a descriptor. Every other field has a safe default: a raw editor, `Apply::Restart`
/// unchecked, read-only on the web, not secret, not advanced. The builder methods change them.
// Unused outside tests until the per-group data tables (A2a-d, #1489) fill the table.
#[allow(dead_code)]
pub const fn d(
    key: &'static str,
    group: Group,
    sub: &'static str,
    label: &'static str,
    help: &'static str,
) -> Descriptor {
    Descriptor {
        key,
        group,
        sub,
        label,
        help,
        control: Control::Raw,
        unit: None,
        range: None,
        choices: &[],
        effective_default: None,
        zero_means: "",
        advanced: false,
        apply: Apply::Restart,
        apply_checked: false,
        user_only: false,
        web: WebEdit::ReadOnly,
        widens: "",
        secret: false,
    }
}

impl Descriptor {
    pub const fn control(mut self, control: Control) -> Self {
        self.control = control;
        self
    }
    pub const fn toggle(self) -> Self {
        self.control(Control::Toggle)
    }
    pub const fn choice(mut self, choices: &'static [(&'static str, &'static str)]) -> Self {
        self.control = Control::Choice;
        self.choices = choices;
        self
    }
    pub const fn number(mut self, unit: Unit) -> Self {
        self.control = Control::Number;
        self.unit = Some(unit);
        self
    }
    pub const fn decimal(self) -> Self {
        self.control(Control::Decimal)
    }
    pub const fn text(self) -> Self {
        self.control(Control::Text)
    }
    /// The value shown when the schema default is null but a value is in force (JSON text).
    pub const fn eff(mut self, json: &'static str) -> Self {
        self.effective_default = Some(json);
        self
    }
    pub const fn zero(mut self, means: &'static str) -> Self {
        self.zero_means = means;
        self
    }
    /// An advised range: values outside it save with a note.
    pub const fn advise(mut self, min: u64, max: u64, step: u64, note: &'static str) -> Self {
        self.range = Some(Range {
            min,
            max: Some(max),
            hard: false,
            step,
            note,
        });
        self
    }
    /// A hard range: values outside it are refused.
    pub const fn hard(mut self, min: u64, max: u64, step: u64, note: &'static str) -> Self {
        self.range = Some(Range {
            min,
            max: Some(max),
            hard: true,
            step,
            note,
        });
        self
    }
    pub const fn unit(mut self, unit: Unit) -> Self {
        self.unit = Some(unit);
        self
    }

    /// An advised minimum with no maximum: values below it save with a note.
    pub const fn advise_from(mut self, min: u64, step: u64, note: &'static str) -> Self {
        self.range = Some(Range {
            min,
            max: None,
            hard: false,
            step,
            note,
        });
        self
    }

    pub const fn advanced(mut self) -> Self {
        self.advanced = true;
        self
    }
    pub const fn apply(mut self, apply: Apply) -> Self {
        self.apply = apply;
        self
    }
    pub const fn web(mut self, web: WebEdit) -> Self {
        self.web = web;
        self
    }
    pub const fn user_only(mut self) -> Self {
        self.user_only = true;
        self
    }
    pub const fn secret(mut self) -> Self {
        self.secret = true;
        self
    }
    pub const fn widens(mut self, sentence: &'static str) -> Self {
        self.widens = sentence;
        self
    }
}

/// The data table, in display order: the Display rows, then the Lead and web, the Safety and the
/// Advanced entries.
fn table() -> &'static [Descriptor] {
    static TABLE: OnceLock<Vec<Descriptor>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            [
                display::ENTRIES,
                budgets::ENTRIES,
                tools::ENTRIES,
                models::ENTRIES,
                compaction::ENTRIES,
                lead::ENTRIES,
                safety::ENTRIES,
                advanced::ENTRIES,
            ]
            .concat()
        })
        .as_slice()
}

/// Every descriptor, in display order.
pub fn descriptors() -> &'static [Descriptor] {
    table()
}

/// The descriptor for one key (a schema key or a field key).
pub fn descriptor(key: &str) -> Option<&'static Descriptor> {
    table().iter().find(|d| d.key == key)
}

/// The descriptors of one group, in display order.
pub fn in_group(group: Group) -> impl Iterator<Item = &'static Descriptor> {
    table().iter().filter(move |d| d.group == group)
}

/// The field descriptors below one object key, in display order.
pub fn fields_of(schema_key: &'static str) -> impl Iterator<Item = &'static Descriptor> {
    table().iter().filter(move |d| is_field_of(d.key, schema_key))
}

/// Test helper: true when `control` edits a value of schema kind `kind`.
#[cfg(test)]
pub(crate) fn control_fits(control: Control, kind: crate::schema::Kind) -> bool {
    use crate::schema::Kind;
    match control {
        Control::Toggle => kind == Kind::Bool,
        Control::Choice => matches!(kind, Kind::Enum(_)),
        Control::Number | Control::Decimal => kind == Kind::Number,
        Control::NumberOr(_) => matches!(kind, Kind::Number | Kind::Any),
        Control::Model | Control::Text => kind == Kind::String,
        Control::MultiSelect(_) => kind == Kind::StringList,
        Control::Map(_) | Control::Raw => kind == Kind::Object,
        _ => false,
    }
}

/// True when `key` is `schema_key` followed by "." and a field path.
fn is_field_of(key: &str, schema_key: &str) -> bool {
    key.strip_prefix(schema_key)
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|f| !f.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_sets_fields() {
        let e = d(
            "oxa.bash.defaultTimeout",
            Group::Tools,
            "Bash",
            "Bash timeout",
            "Timeout of a bash call that names none.",
        )
        .number(Unit::Seconds)
        .eff("120")
        .zero("no timeout")
        .advise(0, 3600, 30, "a hung command holds the turn that long");
        assert_eq!(e.key, "oxa.bash.defaultTimeout");
        assert_eq!(e.group, Group::Tools);
        assert_eq!(e.sub, "Bash");
        assert_eq!(e.label, "Bash timeout");
        assert_eq!(e.control, Control::Number);
        assert_eq!(e.unit, Some(Unit::Seconds));
        assert_eq!(e.effective_default, Some("120"));
        assert_eq!(e.zero_means, "no timeout");
        assert_eq!(
            e.range,
            Some(Range {
                min: 0,
                max: Some(3600),
                hard: false,
                step: 30,
                note: "a hung command holds the turn that long"
            })
        );
        // Defaults the builder did not touch.
        assert!(!e.advanced && !e.user_only && !e.secret && !e.apply_checked);
        assert_eq!(e.apply, Apply::Restart);
        assert_eq!(e.web, WebEdit::ReadOnly);
        assert_eq!(e.choices, &[] as &[(&str, &str)]);

        let h = d(
            "oxa.safety.mode",
            Group::Safety,
            "",
            "Mode",
            "How strict the guard is.",
        )
        .choice(&[("strict", "ask first"), ("off", "")])
        .hard(1, 9, 1, "rows")
        .advanced()
        .user_only()
        .secret()
        .apply(Apply::Now)
        .web(WebEdit::Confirm)
        .widens("Loosens the guard.");
        assert_eq!(h.control, Control::Choice);
        assert_eq!(h.choices, &[("strict", "ask first"), ("off", "")]);
        assert_eq!(h.range.map(|r| r.hard), Some(true));
        assert!(h.advanced && h.user_only && h.secret);
        assert_eq!(h.apply, Apply::Now);
        assert_eq!(h.web, WebEdit::Confirm);
        assert_eq!(h.widens, "Loosens the guard.");
    }

    #[test]
    fn group_order_and_ids_are_stable() {
        let ids: Vec<&str> = Group::ALL.iter().map(|g| g.id()).collect();
        assert_eq!(
            ids,
            [
                "display",
                "models",
                "budgets",
                "tools",
                "workers",
                "memory",
                "compaction",
                "codemode",
                "lead",
                "safety",
                "advanced"
            ]
        );
        assert_eq!(Group::ALL.len(), 11);
        for g in Group::ALL {
            assert!(
                g.short().chars().count() <= 8,
                "short tag too long: {}",
                g.short()
            );
            assert!(!g.title().is_empty() && !g.summary().is_empty());
            assert!(!g.summary().contains('\n'));
        }
    }

    #[test]
    fn unwired_groups_stay_empty() {
        // Display (A2a), Models and compaction (A2b), Budgets and Tools (A2c), and Lead,
        // Safety and Advanced (A2d) have data.
        assert!(descriptor("oxa.bash.defaultTimeout").is_some());
        assert!(descriptor("oxa.maxTurns").is_some());
        assert!(in_group(Group::Budgets).next().is_some());
        assert!(in_group(Group::Tools).next().is_some());
        assert!(in_group(Group::Models).next().is_some());
        assert!(descriptor("compaction.keepRecentTokens").is_some());
        assert_eq!(fields_of("oxa.lead.serve").count(), 9);
        assert_eq!(in_group(Group::Display).count(), 30);
        // main's 147 descriptors plus this branch's two token keys.
        assert_eq!(descriptors().len(), 149);
    }

    #[test]
    fn field_keys_sit_below_their_object_key() {
        assert!(is_field_of("oxa.lead.serve.port", "oxa.lead.serve"));
        assert!(!is_field_of("oxa.lead.serve", "oxa.lead.serve"));
        assert!(!is_field_of("oxa.lead.serve.", "oxa.lead.serve"));
        assert!(!is_field_of("oxa.lead.servex.port", "oxa.lead.serve"));
    }
}
