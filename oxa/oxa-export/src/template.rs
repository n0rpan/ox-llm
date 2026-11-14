//! Architect-owned: the copied pi export template (see `assets/README.md`). Not a task file.
// copied from pi packages/coding-agent/src/core/export-html/{template.html,template.css,template.js,vendor/*}

/// `template.html`. Placeholders, each replaced exactly once and in this order (pi's order):
/// in the CSS `{{THEME_VARS}}`, `{{BODY_BG}}`, `{{CONTAINER_BG}}`, `{{INFO_BG}}`; then in the
/// HTML `{{CSS}}`, `{{JS}}`, `{{SESSION_DATA}}`, `{{MARKED_JS}}`, `{{MARKED_CJK_FRIENDLY_JS}}`,
/// `{{HIGHLIGHT_JS}}`.
pub const TEMPLATE_HTML: &str = include_str!("../assets/template.html");
pub const TEMPLATE_CSS: &str = include_str!("../assets/template.css");
pub const TEMPLATE_JS: &str = include_str!("../assets/template.js");
pub const MARKED_JS: &str = include_str!("../assets/vendor/marked.min.js");
pub const MARKED_CJK_FRIENDLY_JS: &str = include_str!("../assets/vendor/marked-cjk-friendly.min.js");
pub const HIGHLIGHT_JS: &str = include_str!("../assets/vendor/highlight.min.js");

/// The placeholders of [`TEMPLATE_CSS`], in replacement order.
pub const CSS_PLACEHOLDERS: [&str; 4] = ["{{THEME_VARS}}", "{{BODY_BG}}", "{{CONTAINER_BG}}", "{{INFO_BG}}"];
/// The placeholders of [`TEMPLATE_HTML`], in replacement order.
pub const HTML_PLACEHOLDERS: [&str; 6] = [
    "{{CSS}}",
    "{{JS}}",
    "{{SESSION_DATA}}",
    "{{MARKED_JS}}",
    "{{MARKED_CJK_FRIENDLY_JS}}",
    "{{HIGHLIGHT_JS}}",
];
