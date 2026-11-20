//! Advanced: paths, launch settings and keys that oxa accepts but ignores (design section 5, row
//! A2d). Every entry here is `advanced`, so the group and the flag agree.

use super::{d, Apply, Control, Descriptor, Group, WebEdit};
use crate::schema::Kind;

/// The Advanced entries, in display order.
pub(super) const ENTRIES: &[Descriptor] = &[
    d(
        "sessionDir",
        Group::Advanced,
        "Paths and launch",
        "Session folder",
        "Where sessions are stored; --session-dir wins.",
    )
    .key(Kind::String, "null")
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
    .key(Kind::Object, "{}")
    .control(Control::Raw)
    .user_only()
    .advanced()
    .apply(Apply::Restart)
    .web(WebEdit::ReadOnly),
    // Exact protected owner Session ID for free-text Lead chat; user settings only.
    d(
        "oxa.lead.sessionId",
        Group::Advanced,
        "Paths and launch",
        "Lead session id",
        "Set by the owner operation that creates the Lead session.",
    )
    .key(Kind::String, "null")
    .text()
    .user_only()
    .advanced()
    .apply(Apply::Restart)
    .web(WebEdit::ReadOnly),
    // Exact owner Session ID the lead web page attaches to; user settings only.
    d(
        "oxa.lead.attachSessionId",
        Group::Advanced,
        "Paths and launch",
        "Lead attach session id",
        "The running owner session the lead web page shows and sends to.",
    )
    .key(Kind::String, "null")
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
    .key(Kind::Any, "[]")
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
    .key(Kind::String, "null")
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
    .key(Kind::Enum(&["off", "final", "streaming"]), "\"streaming\"")
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
    .ignored(Kind::String, "null")
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
    .ignored(Kind::Bool, "false")
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
    .ignored(Kind::Bool, "true")
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
    .ignored(Kind::Bool, "true")
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
    .ignored(Kind::StringList, "null")
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
    .ignored(Kind::Enum(&["auto", "gist", "radius"]), "\"auto\"")
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
    .ignored(
        Kind::Enum(&["auto", "sse", "websocket", "websocket-cached"]),
        "\"auto\"",
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
    .ignored(Kind::Bool, "true")
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
    .ignored(Kind::Any, "15000")
    .control(Control::Ignored)
    .advanced()
    .web(WebEdit::ReadOnly),
];
