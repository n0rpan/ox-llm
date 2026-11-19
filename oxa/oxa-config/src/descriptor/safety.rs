//! Safety: guards on permissions, network and the bash sandbox (design section 5, row A2d).

use super::{d, Apply, Control, Descriptor, Group, Item, WebEdit};

/// The Safety entries, in display order.
pub(super) const ENTRIES: &[Descriptor] = &[
    d(
        "permissions.defaultMode",
        Group::Safety,
        "Permissions",
        "Permission mode at start",
        "standard, auto, yolo or plan. A project may only lower it.",
    )
    .choice(&[("standard", ""), ("auto", ""), ("yolo", ""), ("plan", "")])
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Changes how much oxa does without asking first."),
    d(
        "oxa.mode",
        Group::Safety,
        "Permissions",
        "Work mode at start",
        "plan, workspace or ask.",
    )
    .choice(&[("plan", ""), ("workspace", ""), ("ask", "")])
    .apply(Apply::Reload)
    .web(WebEdit::Edit),
    d(
        "defaultProjectTrust",
        Group::Safety,
        "Permissions",
        "New projects",
        "Ask, always trust or never trust a project not seen before.",
    )
    .choice(&[("ask", ""), ("always", ""), ("never", "")])
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Trusting a new project lets its settings apply without asking."),
    d(
        "oxa.allowHomeCwd",
        Group::Safety,
        "Permissions",
        "Start in the home folder",
        "Allow oxa to start in home, / or a parent of home.",
    )
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Lets oxa start in the home folder or above it, so tools reach all of it."),
    d(
        "oxa.offline",
        Group::Safety,
        "Network",
        "Offline",
        "No network use by oxa.",
    )
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Turning this off lets oxa use the network again."),
    d(
        "oxa.engine.allowRemote",
        Group::Safety,
        "Network",
        "Engines on other hosts",
        "Allow an engine that is not on this machine.",
    )
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Lets a model engine on another host receive requests."),
    d(
        "httpProxy",
        Group::Safety,
        "Network",
        "HTTP proxy for MCP",
        "Proxy for MCP HTTP requests.",
    )
    .text()
    .secret()
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Sends MCP HTTP requests through this proxy."),
    d(
        "oxa.sandbox.net",
        Group::Safety,
        "Bash sandbox",
        "Network in bash",
        "Network for bash in workspace and ask modes.",
    )
    .choice(&[("on", "bash may use the network"), ("off", "bash has no network")])
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Lets bash commands reach the network in workspace and ask modes."),
    d(
        "oxa.sandbox.allowHomeWrite",
        Group::Safety,
        "Bash sandbox",
        "Home writable in bash",
        "Make home the bash workspace when oxa starts there.",
    )
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Lets bash write across the home folder when oxa starts there."),
    d(
        "oxa.sandbox.readPaths",
        Group::Safety,
        "Bash sandbox",
        "Extra readable folders",
        "More read-only paths for bash.",
    )
    .control(Control::List(Item::Path))
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Gives bash read access to these extra folders."),
    d(
        "oxa.sandbox.writePaths",
        Group::Safety,
        "Bash sandbox",
        "Extra writable folders",
        "More writable paths for bash.",
    )
    .control(Control::List(Item::Path))
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Gives bash write access to these extra folders."),
];
