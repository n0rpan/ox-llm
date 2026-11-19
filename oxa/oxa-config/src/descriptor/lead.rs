//! Lead and web: the lead agent, the web UI's settings and the LAN sharing list (design section 5,
//! row A2d). The fields of `oxa.lead.serve` sit below their object key.

use super::{d, Apply, Control, Descriptor, Group, Item, Options, Unit, WebEdit};

/// The Lead and web entries, in display order.
pub(super) const ENTRIES: &[Descriptor] = &[
    d(
        "oxa.lead.model",
        Group::Lead,
        "Lead",
        "Lead model",
        "Model the Lead conversation uses. Unset = the start model.",
    )
    .control(Control::Model)
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Edit),
    d(
        "oxa.lead.shadow",
        Group::Lead,
        "Lead",
        "Shadow mode",
        "Allow `oxa lead --shadow`: proposals only, no actions.",
    )
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Edit),
    d(
        "oxa.lead.serve",
        Group::Lead,
        "Web access",
        "Web access",
        "The lead web UI: on or off, identity, address, chat.",
    )
    .control(Control::Fields)
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::ReadOnly),
    d(
        "oxa.share.remoteModels",
        Group::Lead,
        "LAN sharing",
        "Models shared on the LAN",
        "Metered cloud models that may be shared.",
    )
    .control(Control::MultiSelect(Options::Models))
    .user_only()
    .apply(Apply::Reload)
    .web(WebEdit::Confirm)
    .widens("Other machines on the LAN may use these metered cloud models on your account."),
    d(
        "oxa.lead.serve.enabled",
        Group::Lead,
        "",
        "Serve the web UI",
        "Start the lead web listener.",
    )
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Turns the web UI listener on at the next start."),
    d(
        "oxa.lead.serve.owner_chat",
        Group::Lead,
        "",
        "Chat from the web",
        "Free-text chat with lead; needs one verified Tailscale owner.",
    )
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Lets the verified owner chat with lead from the web page."),
    d(
        "oxa.lead.serve.fixed_prompts",
        Group::Lead,
        "",
        "Quick prompts",
        "The three zero-token prompt buttons.",
    )
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Edit),
    d(
        "oxa.lead.serve.owner_settings",
        Group::Lead,
        "",
        "Settings from the web",
        "Allow changing settings on the web page.",
    )
    .toggle()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Lets the verified owner change settings from the web page."),
    d(
        "oxa.lead.serve.identity",
        Group::Lead,
        "",
        "Identity check",
        "`local` or `tailscale`.",
    )
    .choice(&[
        ("local", "no login check; the pages are read-only"),
        ("tailscale", "the viewer's Tailscale login must be allowed"),
    ])
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Changes who may open the web UI."),
    d(
        "oxa.lead.serve.allowed_identities",
        Group::Lead,
        "",
        "Allowed logins",
        "Logins that may open the web UI (16 at most).",
    )
    .control(Control::List(Item::Text))
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Changes which logins may open the web UI."),
    d(
        "oxa.lead.serve.bind",
        Group::Lead,
        "",
        "Listen address",
        "A loopback address.",
    )
    .text()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Changes the address the web UI listens on."),
    d("oxa.lead.serve.port", Group::Lead, "", "Port", "1 to 65535.")
        .number(Unit::Count)
        .hard(1, 65535, 1, "a port from 1 to 65535")
        .user_only()
        .apply(Apply::Restart)
        .web(WebEdit::Confirm)
        .widens("Changes the port the web UI listens on."),
    d(
        "oxa.lead.serve.proxy",
        Group::Lead,
        "Advanced",
        "Trusted proxy",
        "Proxy trust for the identity header.",
    )
    .control(Control::Raw)
    .advanced()
    .user_only()
    .apply(Apply::Restart)
    .web(WebEdit::Confirm)
    .widens("Changes which proxy's identity header is believed."),
];
