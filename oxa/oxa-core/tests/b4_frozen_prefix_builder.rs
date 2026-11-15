//! B4 acceptance tests: Frozen prefix builder. Owner: task B4.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_core::{build_prefix, prompt, tools_json};
use oxa_proto::{Exposure, FrozenPrefix, Source, ToolClass, ToolSpec};
use oxa_resources::{ContextFile, ResourceSource, Resources, Skill};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const READ_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#;
const BASH_SCHEMA: &str =
    r#"{"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}"#;

fn spec(name: &str, description: &str, schema: &str, exposure: Exposure, source: Source) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        schema_json: schema.to_string(),
        class: ToolClass::Read,
        exposure,
        source,
    }
}

/// The four built-in tools, all direct, in selection order.
fn direct_tools() -> Vec<ToolSpec> {
    vec![
        spec(
            "read",
            "Read the contents of a file.",
            READ_SCHEMA,
            Exposure::Direct,
            Source::Builtin,
        ),
        spec(
            "bash",
            "Execute a bash command in the working directory.",
            BASH_SCHEMA,
            Exposure::Direct,
            Source::Builtin,
        ),
        spec(
            "edit",
            "Make precise file edits.",
            r#"{"type":"object"}"#,
            Exposure::Direct,
            Source::Builtin,
        ),
        spec(
            "write",
            "Create or overwrite files.",
            r#"{"type":"object"}"#,
            Exposure::Direct,
            Source::Builtin,
        ),
    ]
}

/// Resources with every kind of input filled in.
fn full_resources() -> Resources {
    Resources {
        context_files: vec![ContextFile {
            path: PathBuf::from("/w/AGENTS.md"),
            content: "Be kind.".to_string(),
        }],
        system_override: None,
        append_system: vec!["Extra one.".to_string(), "Extra two.".to_string()],
        skills: vec![Skill {
            name: "pdf".to_string(),
            description: "Read <PDF> files".to_string(),
            path: PathBuf::from("/s/pdf/SKILL.md"),
            base_dir: PathBuf::from("/s/pdf"),
            source: ResourceSource::User,
            disable_model_invocation: false,
        }],
        ..Resources::default()
    }
}

/// Direct tools plus MCP tools that must not reach the prefix as declarations.
fn mixed_tools() -> Vec<ToolSpec> {
    let mut v = direct_tools();
    v.push(spec(
        "zed__search",
        "Search zed.",
        r#"{"type":"object"}"#,
        Exposure::Codemode,
        Source::Mcp("zed".into()),
    ));
    v.push(spec(
        "zed__open",
        "Open a zed doc.",
        r#"{"type":"object"}"#,
        Exposure::Codemode,
        Source::Mcp("zed".into()),
    ));
    v.push(spec(
        "abc__find",
        "Find in abc.",
        r#"{"type":"object"}"#,
        Exposure::Deferred,
        Source::Mcp("abc".into()),
    ));
    v.push(spec(
        "hid__x",
        "Hidden.",
        r#"{"type":"object"}"#,
        Exposure::Hidden,
        Source::Mcp("hid".into()),
    ));
    v
}

fn extras() -> Vec<(String, String)> {
    vec![
        ("zeta".to_string(), "Zeta body.".to_string()),
        ("alpha".to_string(), "Alpha body.".to_string()),
    ]
}

fn section_names(p: &FrozenPrefix) -> Vec<&str> {
    p.sections.iter().map(|(n, _)| n.as_str()).collect()
}

fn section<'a>(p: &'a FrozenPrefix, name: &str) -> Option<&'a str> {
    p.sections
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, t)| t.as_str())
}

/// SECTION_ORDER with extras after mcp_servers
#[test]
fn section_order_fixed() {
    let p = build_prefix(
        Path::new("/w/project"),
        &full_resources(),
        &mixed_tools(),
        &extras(),
    );
    assert_eq!(
        section_names(&p),
        vec![
            "preamble",
            "identity",
            "skills",
            "context",
            "mcp_servers",
            "alpha",
            "zeta",
            "append",
            "cwd"
        ],
        "fixed order; extras sorted by name after mcp_servers, before append"
    );
    // The order is the proto's SECTION_ORDER with the extras inserted before `append`.
    let fixed: Vec<&str> = FrozenPrefix::SECTION_ORDER
        .iter()
        .copied()
        .filter(|n| section_names(&p).contains(n))
        .collect();
    assert_eq!(
        fixed,
        vec![
            "preamble",
            "identity",
            "skills",
            "context",
            "mcp_servers",
            "append",
            "cwd"
        ]
    );
}

#[test]
fn extras_skip_fixed_names_invalid_names_and_empty_text() {
    let extra = vec![
        ("append".to_string(), "impostor".to_string()),
        ("bad name".to_string(), "x".to_string()),
        ("preamble".to_string(), "x".to_string()),
        ("empty".to_string(), String::new()),
        ("dup".to_string(), "first".to_string()),
        ("dup".to_string(), "second".to_string()),
    ];
    let p = build_prefix(Path::new("/w"), &full_resources(), &direct_tools(), &extra);
    let names = section_names(&p);
    assert_eq!(names.iter().filter(|n| **n == "append").count(), 1);
    assert_eq!(
        names.iter().filter(|n| **n == "preamble").count(),
        1,
        "the extra named preamble is refused"
    );
    assert!(!names.contains(&"bad name") && !names.contains(&"empty"));
    assert_eq!(
        section(&p, "dup"),
        Some("<dup>\nfirst\n</dup>"),
        "first occurrence wins"
    );
    assert!(!p.system_text().contains("impostor"));
}

/// determinism
#[test]
fn same_inputs_same_sha() {
    let a = build_prefix(
        Path::new("/w/project"),
        &full_resources(),
        &mixed_tools(),
        &extras(),
    );
    let b = build_prefix(
        Path::new("/w/project"),
        &full_resources(),
        &mixed_tools(),
        &extras(),
    );
    assert_eq!(a, b);
    assert_eq!(a.sha256, b.sha256);
    assert_eq!(a.system_text(), b.system_text());
    assert_eq!(a.tools_json, b.tools_json);
}

/// last section and last line
#[test]
fn cwd_is_last() {
    let p = build_prefix(
        Path::new("/w/project"),
        &full_resources(),
        &mixed_tools(),
        &extras(),
    );
    let last = p.sections.last().expect("sections");
    assert_eq!(last.0, "cwd");
    assert_eq!(last.1, "<cwd>\n/w/project\n</cwd>");
    assert_eq!(p.system_text().lines().last(), Some("</cwd>"));
    assert_eq!(p.system_text().lines().rev().nth(1), Some("/w/project"));
    // a session moved to another directory changes only the last section
    let moved = build_prefix(
        Path::new("/w/other"),
        &full_resources(),
        &mixed_tools(),
        &extras(),
    );
    for (a, b) in p
        .sections
        .iter()
        .zip(moved.sections.iter())
        .take(p.sections.len() - 1)
    {
        assert_eq!(a, b);
    }
}

/// negative test greps the bytes
#[test]
fn no_date_or_git_text() {
    let p = build_prefix(
        Path::new("/w/project"),
        &full_resources(),
        &mixed_tools(),
        &extras(),
    );
    let bytes = format!("{}\n{}", p.system_text(), p.tools_json).to_lowercase();
    // O-152: the identity names the models the install serves (all of them, stable across a
    // model switch); the current model's name stays out of every other section.
    let without_identity: String = p
        .sections
        .iter()
        .filter(|(n, _)| n != "identity")
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
        .to_lowercase();
    assert!(
        !without_identity.contains("model"),
        "W3: no model name outside the identity"
    );
    for banned in [
        "date",
        "today",
        "git",
        "branch",
        "plan mode",
        "workspace mode",
        "hostname",
        "current time",
    ] {
        assert!(
            !bytes.contains(banned),
            "W3: prefix bytes must not contain {banned:?}"
        );
    }
    // no ISO-like calendar date anywhere in the bytes
    let chars: Vec<char> = bytes.chars().collect();
    for w in chars.windows(10) {
        let s: String = w.iter().collect();
        let date_like = s.chars().enumerate().all(|(i, c)| match i {
            4 | 7 => c == '-',
            _ => c.is_ascii_digit(),
        });
        assert!(!date_like, "W3: date-like text {s:?}");
    }
}

/// tools_json golden
#[test]
fn tools_sorted_and_schema_verbatim() {
    let tools = vec![
        spec(
            "write",
            "Create \"files\"",
            r#"{"type":"object"}"#,
            Exposure::Direct,
            Source::Builtin,
        ),
        spec(
            "read",
            "Read é\n\tfile",
            READ_SCHEMA,
            Exposure::Direct,
            Source::Builtin,
        ),
        spec("bash", "Run.", BASH_SCHEMA, Exposure::Direct, Source::Builtin),
    ];
    let golden = concat!(
        r#"[{"type":"function","function":{"name":"bash","description":"Run.","parameters":"#,
        r#"{"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}}},"#,
        r#"{"type":"function","function":{"name":"read","description":"Read é\n\tfile","parameters":"#,
        r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}},"#,
        r#"{"type":"function","function":{"name":"write","description":"Create \"files\"","parameters":{"type":"object"}}}]"#,
    );
    assert_eq!(tools_json(&tools), golden);
    // the array is valid JSON with the same content
    let parsed: serde_json::Value = serde_json::from_str(&tools_json(&tools)).expect("valid JSON");
    assert_eq!(parsed.as_array().map(Vec::len), Some(3));
    assert_eq!(tools_json(&[]), "[]");
    assert_eq!(
        build_prefix(Path::new("/w"), &Resources::default(), &[], &[]).tools_json,
        "[]"
    );
}

/// other exposures absent
#[test]
fn only_direct_tools_declared() {
    let tools = mixed_tools();
    let json = tools_json(&tools);
    for absent in ["zed__search", "zed__open", "abc__find", "hid__x"] {
        assert!(!json.contains(absent), "{absent} must not be declared");
    }
    let p = build_prefix(Path::new("/w"), &full_resources(), &tools, &[]);
    assert!(!p.system_text().contains("zed__search") && !p.system_text().contains("hid__x"));
    assert!(!p.tools_json.contains("abc__find"));
    // the servers are listed in mcp_servers instead (codemode / tool_search), hidden ones not at all
    let mcp = section(&p, "mcp_servers").expect("mcp_servers present");
    assert!(mcp.contains("- abc (tool_search)") && mcp.contains("- zed (codemode)"));
    assert!(!mcp.contains("hid"));
    assert!(p.tools_json.contains("\"name\":\"read\"") && p.tools_json.contains("\"name\":\"bash\""));
}

/// SYSTEM.md
#[test]
fn system_override_replaces_preamble() {
    let mut r = full_resources();
    r.system_override = Some("CUSTOM ONLY".to_string());
    let p = build_prefix(Path::new("/w"), &r, &direct_tools(), &[]);
    assert_eq!(section(&p, "preamble"), Some("CUSTOM ONLY"));
    let all = p.system_text();
    assert!(!all.contains("<tools>") && !all.contains("<rules>") && !all.contains(prompt::PREAMBLE_LINE));
    // the other sections and the tool list are still there
    assert!(section(&p, "context").is_some() && section(&p, "cwd").is_some());
    assert!(p.tools_json.contains("\"name\":\"edit\""));
    // an empty override is no override
    r.system_override = Some("   ".to_string());
    let q = build_prefix(Path::new("/w"), &r, &direct_tools(), &[]);
    assert!(section(&q, "preamble")
        .unwrap()
        .starts_with(prompt::PREAMBLE_LINE));
}

/// sha256(hash_input)
#[test]
fn sha_matches_hash_input() {
    let p = build_prefix(
        Path::new("/w/project"),
        &full_resources(),
        &mixed_tools(),
        &extras(),
    );
    let expected: [u8; 32] = Sha256::digest(p.hash_input()).into();
    assert_eq!(p.sha256, expected);
    // hash_input is system_text, a NUL byte, then tools_json
    let mut manual = p.system_text().into_bytes();
    manual.push(0);
    manual.extend_from_slice(p.tools_json.as_bytes());
    assert_eq!(p.hash_input(), manual);
    // a change in a declared tool changes the sha
    let mut tools = mixed_tools();
    tools[0].description.push('!');
    let q = build_prefix(Path::new("/w/project"), &full_resources(), &tools, &extras());
    assert_ne!(p.sha256, q.sha256);
}

#[test]
fn skills_hint_names_the_declared_reader_and_hides_disabled_skills() {
    let mut r = full_resources();
    r.skills.push(Skill {
        name: "hidden".to_string(),
        description: "Not listed".to_string(),
        path: PathBuf::from("/s/hidden/SKILL.md"),
        base_dir: PathBuf::from("/s/hidden"),
        source: ResourceSource::User,
        disable_model_invocation: true,
    });
    let with_read = build_prefix(Path::new("/w"), &r, &direct_tools(), &[]);
    let skills = section(&with_read, "skills").unwrap();
    assert!(skills.contains("Use the read tool to load"));
    assert!(!skills.contains("hidden") && !skills.contains("Not listed"));
    let no_read: Vec<ToolSpec> = direct_tools()
        .into_iter()
        .filter(|t| t.name != "read" && t.name != "bash")
        .collect();
    let indirect = build_prefix(Path::new("/w"), &r, &no_read, &[]);
    assert!(section(&indirect, "skills")
        .unwrap()
        .contains("Load a skill's file"));
}

#[test]
fn append_is_joined_and_context_keeps_load_order() {
    let p = build_prefix(Path::new("/w"), &full_resources(), &direct_tools(), &[]);
    assert_eq!(
        section(&p, "append"),
        Some("<addendum>\nExtra one.\n\nExtra two.\n</addendum>")
    );
    assert!(section(&p, "context")
        .unwrap()
        .contains("<project_instructions path=\"/w/AGENTS.md\">"));
}

#[test]
fn empty_resources_give_only_preamble_identity_and_cwd() {
    let p = build_prefix(Path::new("/w"), &Resources::default(), &direct_tools(), &[]);
    // O-152: no identity file = the generic identity, never an error.
    assert_eq!(section_names(&p), vec!["preamble", "identity", "cwd"]);
    assert_eq!(
        section(&p, "identity"),
        Some(prompt::identity_section(None).as_str())
    );
    // No `<tools>` section: the chat template renders the tool list from the tool JSON.
    assert_eq!(
        p.sections[0].1,
        prompt::preamble_with(
            &[
                ("read", "Read the contents of a file."),
                ("bash", "Execute a bash command in the working directory."),
                ("edit", "Make precise file edits."),
                ("write", "Create or overwrite files.")
            ],
            &[],
            true,
            false,
            false,
        )
    );
}
