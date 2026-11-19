//! Advanced: paths, launch settings and keys that oxa accepts but ignores (design section 5, row
//! A2d). Every entry here is `advanced`, so the group and the flag agree.

use super::{d, Apply, Control, Descriptor, Group, WebEdit};

/// The Advanced entries, in display order.
pub(super) const ENTRIES: &[Descriptor] = &[
    d(
        "sessionDir",
        Group::Advanced,
        "Paths and launch",
        "Session folder",
        "Where sessions are stored; --session-dir wins.",
    )
    .control(Control::Path)
    .eff("\"~/.oxa/agent/sessions\"")
    .advanced()
    .apply(Apply::Restart)
    .web(WebEdit::ReadOnly),
    d(
        "oxa.local",
        Group::Advanced,
        "Paths and launch",
        "Local engine launch",
        "Binary, model, arguments and limits of the native local engine.",
    )
    .control(Control::Raw)
    .user_only()
    .advanced()
    .apply(Apply::Restart)
    .web(WebEdit::ReadOnly),
    d(
        "oxa.lead.sessionId",
        Group::Advanced,
        "Paths and launch",
        "Lead session id",
        "Set by the owner operation that creates the Lead session.",
    )
    .text()
    .user_only()
    .advanced()
    .apply(Apply::Restart)
    .web(WebEdit::ReadOnly),
    d(
        "packages",
        Group::Advanced,
        "Paths and launch",
        "Installed packages",
        "Package sources; `oxa install` edits this.",
    )
    .control(Control::Raw)
    .advanced()
    .apply(Apply::Reload)
    .web(WebEdit::ReadOnly),
    d(
        "$schema",
        Group::Advanced,
        "Paths and launch",
        "Schema hint",
        "Editor schema URL; ignored by oxa.",
    )
    .text()
    .advanced()
    .apply(Apply::Now)
    .web(WebEdit::ReadOnly),
    d(
        "markdown.mermaid",
        Group::Advanced,
        "Replies",
        "Mermaid diagrams",
        "Draw mermaid diagrams: off, once a reply ends, or as it streams.",
    )
    .choice(&[("off", ""), ("final", ""), ("streaming", "")])
    .advanced()
    .web(WebEdit::ReadOnly),
    d(
        "defaultProvider",
        Group::Advanced,
        "No effect in oxa",
        "Default provider",
        "Accepted for compatibility; ignored.",
    )
    .control(Control::Ignored)
    .advanced()
    .web(WebEdit::ReadOnly),
    d(
        "enableAnalytics",
        Group::Advanced,
        "No effect in oxa",
        "Analytics",
        "Accepted for compatibility; ignored. oxa sends none.",
    )
    .control(Control::Ignored)
    .advanced()
    .web(WebEdit::ReadOnly),
    d(
        "enableInstallTelemetry",
        Group::Advanced,
        "No effect in oxa",
        "Install telemetry",
        "Accepted for compatibility; ignored. oxa sends none.",
    )
    .control(Control::Ignored)
    .advanced()
    .web(WebEdit::ReadOnly),
    d(
        "images.autoResize",
        Group::Advanced,
        "No effect in oxa",
        "Image auto-resize",
        "Accepted for compatibility; ignored.",
    )
    .control(Control::Ignored)
    .advanced()
    .web(WebEdit::ReadOnly),
    d(
        "selfUpdateCommand",
        Group::Advanced,
        "No effect in oxa",
        "Self-update command",
        "Accepted for compatibility; ignored.",
    )
    .control(Control::Ignored)
    .advanced()
    .web(WebEdit::ReadOnly),
    d(
        "sessionShareDestination",
        Group::Advanced,
        "No effect in oxa",
        "Session share destination",
        "Accepted for compatibility; ignored.",
    )
    .control(Control::Ignored)
    .advanced()
    .web(WebEdit::ReadOnly),
    d(
        "transport",
        Group::Advanced,
        "No effect in oxa",
        "Transport",
        "Accepted for compatibility; ignored.",
    )
    .control(Control::Ignored)
    .advanced()
    .web(WebEdit::ReadOnly),
    d(
        "warnings.anthropicExtraUsage",
        Group::Advanced,
        "No effect in oxa",
        "Extra-usage warning",
        "Accepted for compatibility; ignored.",
    )
    .control(Control::Ignored)
    .advanced()
    .web(WebEdit::ReadOnly),
    d(
        "websocketConnectTimeoutMs",
        Group::Advanced,
        "No effect in oxa",
        "WebSocket connect timeout",
        "Accepted for compatibility; ignored.",
    )
    .control(Control::Ignored)
    .advanced()
    .web(WebEdit::ReadOnly),
];
