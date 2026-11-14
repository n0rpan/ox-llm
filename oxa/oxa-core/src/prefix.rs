//! B4: the frozen prefix builder (W3). Owner: task B4. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/system-prompt.ts (text comes from prompt.rs)
use oxa_proto::{Exposure, FrozenPrefix, Profile, Source, ToolSpec};
use oxa_resources::Resources;
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::prompt::{self, McpReach, SkillReader};

/// What an epoch's prefix depends on besides the inputs of [`build_prefix`]: the profile (T-#6:
/// `local` leaves `<tools>` out of the preamble) and `oxa.diet.output` (V020-OPTIMIZE #1, #10),
/// resolved by the caller from the settings. `child` (L4-SUBVERIFY): this process is a subagent
/// child; it adds [`crate::prompt::RULE_CHILD_VERIFY`] under `local` with the diet on. `Default` =
/// `pi`, diet off, not a child: [`build_prefix`]'s bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PrefixOpts {
    pub profile: Profile,
    pub diet_output: bool,
    pub child: bool,
}

/// Build the prefix of an epoch under the default options (`pi`, diet off). Pure: same inputs =
/// same bytes = same sha. See [`build_prefix_with`] for the sections.
pub fn build_prefix(
    cwd: &Path,
    r: &Resources,
    tools: &[ToolSpec],
    extra: &[(String, String)],
) -> FrozenPrefix {
    build_prefix_with(cwd, r, tools, extra, &PrefixOpts::default())
}

/// Build the prefix of an epoch. Pure: same inputs = same bytes = same sha.
///
/// Sections, in `FrozenPrefix::SECTION_ORDER`, empty ones omitted: `preamble`
/// (`prompt::preamble_with` over the `Direct` tools, or `r.system_override` when set), `identity`
/// (`prompt::identity_section` over `r.identity`, O-152), `skills` (`prompt::skills_section`),
/// `context` (`prompt::context_section`), `mcp_servers` and any
/// other `extra` sections (sorted by name, after `skills`), `append` (`r.append_system`
/// joined), `cwd` (`prompt::cwd_section`, always last). `tools_json` = [`tools_json`] over
/// the `Direct` tools. `sha256` = SHA-256 of `FrozenPrefix::hash_input`. No date, no git
/// state, no mode, no model name, no host name (a negative test greps the bytes).
/// `opts` changes only the preamble (see [`PrefixOpts`]).
pub fn build_prefix_with(
    cwd: &Path,
    r: &Resources,
    tools: &[ToolSpec],
    extra: &[(String, String)],
    opts: &PrefixOpts,
) -> FrozenPrefix {
    let direct: Vec<&ToolSpec> = tools.iter().filter(|t| t.exposure == Exposure::Direct).collect();
    let mut sections: Vec<(String, String)> = Vec::new();

    // preamble: SYSTEM.md replaces the whole default preamble (pi's customPrompt).
    let preamble = match r.system_override.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(custom) => prompt::custom_preamble(custom),
        None => {
            let snippets: Vec<(&str, String)> = direct
                .iter()
                .map(|t| (t.name.as_str(), first_line(&t.description)))
                .collect();
            let pairs: Vec<(&str, &str)> = snippets.iter().map(|(n, s)| (*n, s.as_str())).collect();
            let local = opts.profile == Profile::Local;
            prompt::preamble_with(&pairs, &[], local, opts.diet_output, local, opts.child)
        }
    };
    push(&mut sections, "preamble", preamble);

    // identity (O-152): the installed identity file, else the generic fallback.
    push(
        &mut sections,
        "identity",
        prompt::identity_section(r.identity.as_deref()),
    );

    // skills: the reader named in the hint is the first of read, bash that is declared.
    let has_direct = |n: &str| direct.iter().any(|t| t.name == n);
    let reader = if has_direct("read") {
        SkillReader::Read
    } else if has_direct("bash") {
        SkillReader::Bash
    } else {
        SkillReader::Indirect
    };
    let skills: Vec<(String, String, String)> = r
        .skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .map(|s| {
            (
                s.name.clone(),
                s.description.clone(),
                s.path.to_string_lossy().into_owned(),
            )
        })
        .collect();
    push(
        &mut sections,
        "skills",
        prompt::skills_section_for(&skills, reader),
    );

    // context: context files in load order (after the anchor: project-specific).
    let files: Vec<(String, String)> = r
        .context_files
        .iter()
        .map(|f| (f.path.to_string_lossy().into_owned(), f.content.clone()))
        .collect();
    push(&mut sections, "context", prompt::context_section(&files));

    // mcp_servers: servers whose tools are not declared (codemode or deferred exposure).
    let mut servers: Vec<(String, McpReach, String)> = Vec::new();
    for t in tools {
        let reach = match t.exposure {
            Exposure::Codemode => McpReach::Codemode,
            Exposure::Deferred => McpReach::ToolSearch,
            _ => continue,
        };
        let Source::Mcp(server) = &t.source else { continue };
        if !servers.iter().any(|(n, _, _)| n == server) {
            servers.push((server.clone(), reach, String::new()));
        }
    }
    // Sorted by name: the section must not depend on the order servers connected in (W3).
    servers.sort_by(|a, b| a.0.cmp(&b.0));
    // E-INT: a hook set that knows the configuration (the MCP manager) supplies the section
    // itself, from configuration only, so the prefix does not depend on which servers happened
    // to be connected when the epoch opened (W3). The tool-derived text is the fallback.
    let configured = extra
        .iter()
        .find(|(n, t)| n == "mcp_servers" && !t.is_empty())
        .map(|(_, t)| t.clone());
    push(
        &mut sections,
        "mcp_servers",
        configured.unwrap_or_else(|| prompt::mcp_servers_section(&servers)),
    );

    // extension / hook sections: valid names only, not a fixed name, sorted, first wins.
    let mut ext: Vec<(&str, &str)> = extra
        .iter()
        .filter(|(n, t)| {
            prompt::valid_section_name(n)
                && !t.is_empty()
                && !FrozenPrefix::SECTION_ORDER.contains(&n.as_str())
        })
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();
    ext.sort_by(|a, b| a.0.cmp(b.0));
    ext.dedup_by(|a, b| a.0 == b.0);
    for (name, text) in ext {
        push(&mut sections, name, prompt::tagged(name, text));
    }

    // append: APPEND_SYSTEM.md (user, then project) and --append-system-prompt values.
    let appended: Vec<&str> = r
        .append_system
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    push(
        &mut sections,
        "append",
        prompt::append_section(&appended.join("\n\n")),
    );

    // cwd: always last (W3).
    push(&mut sections, "cwd", prompt::cwd_section(cwd));

    let mut prefix = FrozenPrefix {
        sections,
        tools_json: tools_json(tools),
        sha256: [0; 32],
    };
    prefix.sha256 = Sha256::digest(prefix.hash_input()).into();
    prefix
}

/// The request's `tools` array for the `Exposure::Direct` specs, sorted by name:
/// `[{"type":"function","function":{"name":N,"description":D,"parameters":<schema_json
/// verbatim>}},..]`, no whitespace added. `[]` when there are none.
pub fn tools_json(tools: &[ToolSpec]) -> String {
    let mut direct: Vec<&ToolSpec> = tools.iter().filter(|t| t.exposure == Exposure::Direct).collect();
    direct.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = String::from("[");
    for (i, t) in direct.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"type\":\"function\",\"function\":{\"name\":");
        out.push_str(&json_string(&t.name));
        out.push_str(",\"description\":");
        out.push_str(&json_string(&t.description));
        out.push_str(",\"parameters\":");
        out.push_str(&t.schema_json);
        out.push_str("}}");
    }
    out.push(']');
    out
}

/// Append a section unless its text is empty (empty sections are omitted, as the proto says).
fn push(sections: &mut Vec<(String, String)>, name: &str, text: String) {
    if !text.is_empty() {
        sections.push((name.to_string(), text));
    }
}

/// The first line of a tool description, trimmed: the one-line snippet the preamble lists.
fn first_line(description: &str) -> String {
    description.lines().next().unwrap_or("").trim().to_string()
}

/// A JSON string literal (serde_json escaping is deterministic).
fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| String::from("\"\""))
}
