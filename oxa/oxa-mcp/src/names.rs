//! E9: tool and server naming. Owner: task E9. Signatures below are frozen; bodies are yours.
// ported from pi packages/mcp/src/naming.ts
use sha2::{Digest, Sha256};

/// Prefix of every MCP tool name.
pub const MCP_PREFIX: &str = "mcp__";
/// Names of the resource tools.
pub const RESOURCE_TOOLS: [&str; 3] = [
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
];
/// Provider tool names are limited to 64 characters of `[A-Za-z0-9_-]`.
const MAX_TOOL_NAME_LEN: usize = 64;
/// `_` plus the 8 hex digits of the hash suffix.
const HASH_SUFFIX_LEN: usize = 9;
const SEPARATOR: &str = "__";

/// `true` when `name` contains only letters, digits, `_` and `-` and is not empty.
pub fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Server names that differ only in `-` versus `_` count as the same server.
pub fn same_server(a: &str, b: &str) -> bool {
    a.replace('-', "_") == b.replace('-', "_")
}

/// Every character other than ASCII letters, digits and `_` becomes `_`.
fn sanitize(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `mcp__<server>__<tool>` with every character other than ASCII letters, digits and `_`
/// replaced by `_` (in both parts).
pub fn mangle(server: &str, tool: &str) -> String {
    format!("{MCP_PREFIX}{}{SEPARATOR}{}", sanitize(server), sanitize(tool))
}

/// The full names for all tools of one server, in the given order. When two or more mangled
/// names collide, *all* of the colliding tools get the suffix `_<8 hex of sha256(original tool
/// name)>`. Deterministic.
///
/// A mangled name longer than 64 characters (the provider limit) gets the same suffix, its head
/// cut to fit. The suffix comes from the original tool name, which is unique within a server, so
/// suffixed names stay distinct.
pub fn tool_names(server: &str, tools: &[String]) -> Vec<String> {
    let bases: Vec<String> = tools.iter().map(|t| mangle(server, t)).collect();
    let mut counts = std::collections::HashMap::<&str, usize>::new();
    for base in &bases {
        *counts.entry(base.as_str()).or_insert(0) += 1;
    }
    tools
        .iter()
        .zip(&bases)
        .map(|(tool, base)| {
            let collides = counts.get(base.as_str()).copied().unwrap_or(0) > 1;
            if collides || base.len() > MAX_TOOL_NAME_LEN {
                // `base` is ASCII (sanitized), so byte slicing cannot split a character.
                let keep = MAX_TOOL_NAME_LEN - HASH_SUFFIX_LEN;
                let head = &base[..base.len().min(keep)];
                format!("{head}_{}", tool_hash(tool))
            } else {
                base.clone()
            }
        })
        .collect()
}

/// First 4 bytes of sha256(tool name) as 8 lowercase hex digits.
fn tool_hash(tool: &str) -> String {
    let digest = Sha256::digest(tool.as_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Split a full name back into `(mangled server, mangled tool)`; `None` when it does not start
/// with `mcp__` or has no second `__`.
///
/// The split is at the first `__` after the prefix, so a server name that itself contains `__`
/// cannot be recovered from the full name; callers match against known server names instead.
pub fn split_full_name(full: &str) -> Option<(&str, &str)> {
    let rest = full.strip_prefix(MCP_PREFIX)?;
    let at = rest.find(SEPARATOR)?;
    let (server, tool) = (&rest[..at], &rest[at + SEPARATOR.len()..]);
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

/// Match a tool name against a `toolExposure` / `--tools` pattern where `*` matches any
/// characters (also none); everything else is literal.
pub fn pattern_matches(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let first = parts[0];
    let last = parts[parts.len() - 1];
    if name.len() < first.len() + last.len() || !name.starts_with(first) || !name.ends_with(last) {
        return false;
    }
    // The middle parts must occur in order between the fixed prefix and suffix.
    let mut rest = &name[first.len()..name.len() - last.len()];
    for middle in &parts[1..parts.len() - 1] {
        match rest.find(middle) {
            Some(at) => rest = &rest[at + middle.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mangle_sanitizes_both_parts() {
        assert_eq!(mangle("dev-radius", "get.items"), "mcp__dev_radius__get_items");
        assert_eq!(mangle("s", "é"), "mcp__s___");
    }

    #[test]
    fn tool_names_suffix_all_colliding() {
        let names = tool_names("s", &["a-b".to_string(), "a_b".to_string(), "c".to_string()]);
        assert!(names[0].starts_with("mcp__s__a_b_") && names[0].len() == "mcp__s__a_b".len() + 9);
        assert!(names[1].starts_with("mcp__s__a_b_"));
        assert_ne!(names[0], names[1]);
        assert_eq!(names[2], "mcp__s__c");
    }

    #[test]
    fn tool_names_cap_length_at_64() {
        let long = "x".repeat(100);
        let names = tool_names("server", &[long]);
        assert_eq!(names[0].len(), 64);
        assert!(names[0].starts_with("mcp__server__xxx"));
    }

    #[test]
    fn split_and_pattern() {
        assert_eq!(split_full_name("mcp__s__t"), Some(("s", "t")));
        assert_eq!(split_full_name("mcp__s"), None);
        assert_eq!(split_full_name("other__s__t"), None);
        assert!(pattern_matches("get_*", "get_items"));
        assert!(pattern_matches("*_items", "get_items"));
        assert!(pattern_matches("a*b*c", "axxbyyc"));
        assert!(!pattern_matches("a*b*c", "acb"));
        assert!(pattern_matches("*", ""));
        assert!(!pattern_matches("get", "gets"));
    }
}
