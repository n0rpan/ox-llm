//! Safety: guards on permissions, network and the bash sandbox (design section 5, row A2d).

use super::{d, Apply, Control, Descriptor, Group, Item, WebEdit};
use crate::schema::Kind;

/// The Safety entries, in display order.
pub(super) const ENTRIES: &[Descriptor] = &[
    // #202: the permission mode at start; a project's value may only lower it
    // (`Settings::default_permission_mode`).
    d(
        "permissions.defaultMode",
        Group::Safety,
        "Permissions",
        "Permission mode at start",
        "standard, auto, yolo or plan. A project may only lower it.",
    )
    .key(Kind::Enum(&["standard", "auto", "yolo", "plan"]), "\"auto\"")
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
    .key(Kind::Enum(&["plan", "workspace", "ask"]), "\"workspace\"")
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
    .key(Kind::Enum(&["ask", "always", "never"]), "\"ask\"")
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
    .key(Kind::Bool, "false")
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
    .key(Kind::Bool, "false")
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
    .key(Kind::Bool, "false")
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
    .key(Kind::String, "null")
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
    .key(Kind::Enum(&["on", "off"]), "\"on\"")
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
    .key(Kind::Bool, "false")
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Lets bash write across the home folder when oxa starts there."),
    // B2 (lane harden): extra binds of the bash sandbox; user settings only.
    d(
        "oxa.sandbox.readPaths",
        Group::Safety,
        "Bash sandbox",
        "Extra readable folders",
        "More read-only paths for bash.",
    )
    .key(Kind::StringList, "[]")
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
    .key(Kind::StringList, "[]")
    .control(Control::List(Item::Path))
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Gives bash write access to these extra folders."),
    d(
        "oxa.sandbox.sockets",
        Group::Safety,
        "Bash sandbox",
        "Host sockets for bash",
        "Runtime sockets (docker, podman) bash may reach.",
    )
    .key(Kind::StringList, "[]")
    .control(Control::List(Item::Path))
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Lets bash use these host sockets; the docker socket gives root on this machine."),
];
