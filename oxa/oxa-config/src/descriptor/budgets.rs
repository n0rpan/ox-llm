//! Data for the Budgets group: `oxa.cloud` with its fields (section 1.3) and `oxa.maxTurns`
//! (issue #1489, design section 1.2). Checked by the tests in `tools.rs`.

use super::*;

/// The Budgets entries, in display order.
pub(super) const ENTRIES: &[Descriptor] = &[
    d(
        "oxa.cloud",
        Group::Budgets,
        "Cloud budget",
        "Cloud budget and models",
        "Daily USD budget, subscription caps, cloud models and prices.",
    )
    .key(Kind::Object, "{}")
    .control(Control::Fields)
    .apply(Apply::Reload)
    .web(WebEdit::ReadOnly)
    .user_only(),
    d(
        "oxa.cloud.daily_budget_usd",
        Group::Budgets,
        "",
        "Daily cloud budget",
        "USD a day for metered cloud models; unset means no cap.",
    )
    .decimal()
    .unit(Unit::Usd)
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .user_only()
    .widens("Changes how much metered cloud spending oxa allows each day."),
    d(
        "oxa.cloud.subscription_limits.daily_requests",
        Group::Budgets,
        "",
        "Subscription requests a day",
        "Daily request cap for subscription routes.",
    )
    .number(Unit::Count)
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .user_only()
    .widens("Changes how many subscription requests oxa allows each day."),
    d(
        "oxa.cloud.subscription_limits.daily_tokens",
        Group::Budgets,
        "",
        "Subscription tokens a day",
        "Daily token cap for subscription routes.",
    )
    .number(Unit::Tokens)
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .user_only()
    .widens("Changes how many subscription tokens oxa allows each day."),
    d(
        "oxa.cloud.discover_models",
        Group::Budgets,
        "",
        "Discover models at start",
        "Ask configured providers for their model list.",
    )
    .toggle()
    .apply(Apply::Restart)
    .web(WebEdit::Edit)
    .user_only(),
    d(
        "oxa.cloud.models",
        Group::Budgets,
        "",
        "Cloud models",
        "Model routes: provider, upstream model, limits, prices.",
    )
    .control(Control::Raw)
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .user_only()
    .widens("Changes which provider and model oxa sends cloud requests to, and their prices."),
    d(
        "oxa.cloud.unpriced_discovered_pricing",
        Group::Budgets,
        "",
        "Prices for unpriced models",
        "Your own USD rates for discovered models with no known price.",
    )
    .control(Control::Raw)
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .user_only()
    .widens("Changes the prices used to count spending on discovered models."),
    d(
        "oxa.cloud.discovery",
        Group::Budgets,
        "",
        "Discovery endpoints",
        "Base URL per provider when several are configured.",
    )
    .control(Control::Raw)
    .apply(Apply::Restart)
    .web(WebEdit::ReadOnly)
    .advanced()
    .user_only(),
    d(
        "oxa.cloud.subscription",
        Group::Budgets,
        "",
        "Subscription refresh",
        "Public client id and refresh timeout.",
    )
    .control(Control::Raw)
    .apply(Apply::Restart)
    .web(WebEdit::ReadOnly)
    .advanced()
    .user_only(),
    d(
        "oxa.maxTurns",
        Group::Budgets,
        "Run limits",
        "Turn limit per run",
        "End a run after this many model replies. Unset or 0 = no limit.",
    )
    .key(Kind::Number, "null")
    .number(Unit::Count)
    .zero("no limit")
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
];
