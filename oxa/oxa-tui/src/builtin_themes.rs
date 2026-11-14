//! Architect-owned: pi's two built-in themes, copied unchanged (MIT). Not a task file.
// copied from pi packages/coding-agent/src/modes/interactive/theme/{dark,light}.json

/// pi's `dark.json`, byte for byte.
pub const THEME_DARK_JSON: &str = include_str!("../assets/themes/dark.json");
/// pi's `light.json`, byte for byte.
pub const THEME_LIGHT_JSON: &str = include_str!("../assets/themes/light.json");
/// Names of the built-in themes, in selector order. `system` uses the terminal's own colours.
pub const BUILTIN_THEME_NAMES: [&str; 3] = ["dark", "light", "system"];
