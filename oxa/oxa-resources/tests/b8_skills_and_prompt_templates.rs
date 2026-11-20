//! B8 acceptance tests: Skills and prompt templates.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{Dirs, Settings};
use oxa_fake::TestEnv;
use oxa_proto::Level;
use oxa_resources::{
    discover_skills, discover_templates, expand_skill_command, expand_template, expand_template_command,
    load_skill_file, parse_template_args, skills_section, DiscoverOpts, ResourceSource, Skill, Template,
};
use std::path::PathBuf;

fn skill_text(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n# {name}\nDo the {name} thing.\n")
}

fn skill_struct(name: &str, description: &str, path: &str, hidden: bool) -> Skill {
    Skill {
        name: name.to_string(),
        description: description.to_string(),
        path: PathBuf::from(path),
        base_dir: PathBuf::from(path).parent().unwrap().to_path_buf(),
        source: ResourceSource::Cli,
        disable_model_invocation: hidden,
    }
}

fn template(name: &str, body: &str) -> Template {
    Template {
        name: name.to_string(),
        description: String::new(),
        argument_hint: None,
        body: body.to_string(),
        path: PathBuf::from(format!("/p/{name}.md")),
        source: ResourceSource::User,
    }
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// name/description limits
#[test]
fn skill_frontmatter_rules_warn_not_fail() {
    let env = TestEnv::new();
    // Invalid name (uppercase, underscore, trailing and doubled hyphens), no `name` field, and an
    // over-long description: all warnings, the skill still loads. The name falls back to the
    // parent directory name.
    let long = "d".repeat(1025);
    let bad = env.write(
        "work/extra/Bad_Name--x-/SKILL.md",
        &format!("---\ndescription: {long}\n---\nbody\n"),
    );
    let (skill, diags) = load_skill_file(&bad, ResourceSource::Cli).expect("loads with warnings");
    assert_eq!(skill.name, "Bad_Name--x-");
    assert_eq!(skill.base_dir, bad.parent().unwrap());
    let messages: Vec<&str> = diags.iter().map(|d| d.message.as_str()).collect();
    assert!(
        messages.iter().any(|m| m.contains("invalid characters")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("start or end with a hyphen")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("consecutive hyphens")),
        "{messages:?}"
    );
    assert!(
        messages.iter().any(|m| m.contains("exceeds 1024")),
        "{messages:?}"
    );
    assert!(diags.iter().all(|d| d.level == Level::Warning));

    // A 65-character name is a warning, not a failure.
    let long_name = "a".repeat(65);
    let p = env.write(
        &format!("work/extra/{long_name}/SKILL.md"),
        &skill_text(&long_name, "ok"),
    );
    let (_skill, diags) = load_skill_file(&p, ResourceSource::Cli).unwrap();
    assert!(diags.iter().any(|d| d.message.contains("exceeds 64")));

    // A missing description means not loaded.
    let no_desc = env.write("work/extra/nodesc/SKILL.md", "---\nname: nodesc\n---\nbody\n");
    let err = load_skill_file(&no_desc, ResourceSource::Cli).unwrap_err();
    assert_eq!(err.level, Level::Warning);
    assert!(err.message.contains("description"));

    // Through discovery: the warned skills are listed, the broken one is a diagnostic only.
    let dirs = Dirs::under(env.root());
    let settings = Settings::default();
    let opts = DiscoverOpts {
        trusted: true,
        extra_skills: vec![
            env.root().join("work/extra/Bad_Name--x-"),
            env.root().join(format!("work/extra/{long_name}")),
            env.root().join("work/extra/nodesc"),
        ],
        ..Default::default()
    };
    let (skills, diags) = discover_skills(&dirs, &env.work(), &settings, &opts, &[]);
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"Bad_Name--x-"), "{names:?}");
    assert!(names.contains(&long_name.as_str()), "{names:?}");
    assert!(!names.contains(&"nodesc"), "{names:?}");
    assert!(diags.iter().any(|d| d.message.contains("description")));
}

/// matches prompt.rs format inputs
#[test]
fn skills_section_text() {
    let skills = vec![
        skill_struct("pdf", "Read <PDF> files & more", "/s/pdf/SKILL.md", false),
        skill_struct("secret", "Hidden from the prompt", "/s/secret/SKILL.md", true),
        skill_struct("zed", "Second \"quoted\" one", "/s/zed/SKILL.md", false),
    ];
    let expected = concat!(
        "<skills>\n",
        "The following skills provide specialized instructions for specific tasks.\n",
        "Use the read tool to load a skill's file when the task matches its description.\n",
        "Resolve relative paths against the skill's directory (parent of SKILL.md). Use absolute paths in tool calls.\n",
        "\n",
        "<available_skills>\n",
        "  <skill>\n",
        "    <name>pdf</name>\n",
        "    <description>Read &lt;PDF&gt; files &amp; more</description>\n",
        "    <location>/s/pdf/SKILL.md</location>\n",
        "  </skill>\n",
        "  <skill>\n",
        "    <name>zed</name>\n",
        "    <description>Second &quot;quoted&quot; one</description>\n",
        "    <location>/s/zed/SKILL.md</location>\n",
        "  </skill>\n",
        "</available_skills>\n",
        "</skills>",
    );
    assert_eq!(skills_section(&skills), expected);
    // Nothing visible: the section is omitted.
    assert_eq!(skills_section(&skills[1..2]), "");
    assert_eq!(skills_section(&[]), "");
}

/// .agents/skills
#[test]
fn agents_skills_dir_read() {
    let env = TestEnv::new();
    env.write(
        "home/.agents/skills/home-skill/SKILL.md",
        &skill_text("home-skill", "from home"),
    );
    env.write(
        "work/.agents/skills/proj-skill/SKILL.md",
        &skill_text("proj-skill", "from project"),
    );
    let dirs = Dirs::under(env.root());
    let settings = Settings::default();

    // Trusted project: both the home and the project `.agents/skills` are read.
    let trusted = DiscoverOpts {
        trusted: true,
        ..Default::default()
    };
    let (skills, diags) = discover_skills(&dirs, &env.work(), &settings, &trusted, &[]);
    let skills = without_builtins(skills);
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["home-skill", "proj-skill"], "{diags:?}");
    assert!(skills.iter().all(|s| s.source == ResourceSource::Agents));

    // Untrusted project: the project copy is not read, the home one still is.
    let untrusted = DiscoverOpts {
        trusted: false,
        ..Default::default()
    };
    let (skills, _) = discover_skills(&dirs, &env.work(), &settings, &untrusted, &[]);
    let skills = without_builtins(skills);
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["home-skill"]);

    // A subdirectory cwd finds the project `.agents/skills` of its ancestor.
    let sub = env.work().join("sub/deeper");
    std::fs::create_dir_all(&sub).unwrap();
    let (skills, _) = discover_skills(&dirs, &sub, &settings, &trusted, &[]);
    assert!(skills.iter().any(|s| s.name == "proj-skill"));
}

/// $1 $2
#[test]
fn template_positional_args() {
    assert_eq!(
        expand_template("A=$1 B=$2 C=$3", &args(&["x", "y"])),
        "A=x B=y C="
    );
    assert_eq!(expand_template("$1$1-$12", &args(&["a"])), "aa-");
    let t = template("cmp", "first=$1 second=$2");
    assert_eq!(
        expand_template_command(&[t], "/cmp alpha \"beta gamma\""),
        Some("first=alpha second=beta gamma".to_string())
    );
}

/// $@ and $ARGUMENTS
#[test]
fn template_all_args() {
    assert_eq!(
        expand_template("all: $@ | $ARGUMENTS", &args(&["a b", "c"])),
        "all: a b c | a b c"
    );
    // Slice forms.
    assert_eq!(expand_template("${@:2}", &args(&["a", "b", "c"])), "b c");
    assert_eq!(expand_template("${@:2:1}", &args(&["a", "b", "c"])), "b");
    assert_eq!(expand_template("${@:0}", &args(&["a", "b"])), "a b");
    assert_eq!(expand_template("${@:5}", &args(&["a"])), "");
    // Single pass: argument text is never expanded again.
    assert_eq!(expand_template("$1 $@", &args(&["$2", "z"])), "$2 $2 z");
    assert_eq!(expand_template("$@", &[]), "");
}

/// ${1:-x}
#[test]
fn template_default_value() {
    assert_eq!(expand_template("${1:-x}", &[]), "x");
    assert_eq!(expand_template("${1:-x}", &args(&["v"])), "v");
    // An empty argument counts as missing, as in pi.
    assert_eq!(expand_template("${1:-x}", &args(&[""])), "x");
    assert_eq!(expand_template("${3:-d}", &args(&["a"])), "d");
    assert_eq!(expand_template("${@:-none}", &[]), "none");
    assert_eq!(expand_template("${@:-none}", &args(&["a", "b"])), "a b");
    assert_eq!(expand_template("${ARGUMENTS:-none}", &[]), "none");
    // Not a placeholder without `:-` or a closing brace: literal text.
    assert_eq!(expand_template("${1}", &args(&["a"])), "${1}");
    assert_eq!(expand_template("${1:-x", &args(&["a"])), "${1:-x");
}

/// CHANGELOG cases
#[test]
fn apostrophe_and_quote_cases() {
    assert_eq!(parse_template_args("doesn't stop"), vec!["doesn't", "stop"]);
    assert_eq!(
        parse_template_args("\"hello world\" 'x y'"),
        vec!["hello world", "x y"]
    );
    // Unterminated quote: the quote is text and the rest is split as usual.
    assert_eq!(
        parse_template_args("\"unterminated arg"),
        vec!["\"unterminated", "arg"]
    );
    assert_eq!(parse_template_args("'a b"), vec!["'a", "b"]);
    // Escaped quote inside a quoted argument.
    assert_eq!(parse_template_args(r#""say \"hi\"""#), vec![r#"say "hi""#]);
    // Empty quoted arguments are kept, so positions do not shift.
    assert_eq!(parse_template_args("\"\" x"), vec!["", "x"]);
    assert_eq!(parse_template_args("'' ''"), vec!["", ""]);
    // A quote in the middle of a word is text.
    assert_eq!(parse_template_args("it's ok"), vec!["it's", "ok"]);
    assert_eq!(parse_template_args("   "), Vec::<String>::new());

    // Through the command path, an apostrophe reaches the template intact.
    let t = template("note", "$1|$2");
    assert_eq!(
        expand_template_command(std::slice::from_ref(&t), "/note doesn't matter"),
        Some("doesn't|matter".to_string())
    );
    // Not a template name, or no name at all: no expansion.
    assert_eq!(
        expand_template_command(std::slice::from_ref(&t), "/other x"),
        None
    );
    assert_eq!(
        expand_template_command(std::slice::from_ref(&t), "no slash"),
        None
    );
    assert_eq!(expand_template_command(&[t], "/ x"), None);
}

/// first wins
#[test]
fn name_collision_diagnostic() {
    let env = TestEnv::new();
    // Skills: the `--skill` path (extra) wins over the agent directory copy.
    env.write("work/extra/dup/SKILL.md", &skill_text("dup", "winner"));
    env.write("home/.oxa/agent/skills/dup/SKILL.md", &skill_text("dup", "loser"));
    let dirs = Dirs::under(env.root());
    let settings = Settings::default();
    let opts = DiscoverOpts {
        extra_skills: vec![env.root().join("work/extra/dup")],
        ..Default::default()
    };
    let (skills, diags) = discover_skills(&dirs, &env.work(), &settings, &opts, &[]);
    let skills = without_builtins(skills);
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].description, "winner");
    assert_eq!(skills[0].path, env.root().join("work/extra/dup/SKILL.md"));
    let collision = diags
        .iter()
        .find(|d| d.message.contains("collision"))
        .expect("collision diagnostic");
    assert_eq!(collision.path, Some(env.agent_dir().join("skills/dup/SKILL.md")));
    assert_eq!(collision.level, Level::Warning);

    // Templates: the `--prompt-template` file wins over the agent directory copy.
    env.write("work/extra/p.md", "winning body $1");
    env.write("home/.oxa/agent/prompts/p.md", "losing body $1");
    let opts = DiscoverOpts {
        extra_templates: vec![env.root().join("work/extra/p.md")],
        ..Default::default()
    };
    let (templates, diags) = discover_templates(&dirs, &env.work(), &settings, &opts, &[]);
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].body, "winning body $1");
    assert!(diags.iter().any(|d| d.message.contains("collision")));
}

/// still a command
#[test]
fn disable_model_invocation_hidden_from_prompt() {
    let env = TestEnv::new();
    env.write(
        "work/extra/hidden/SKILL.md",
        "---\nname: hidden\ndescription: Only on request\ndisable-model-invocation: true\n---\nSecret steps\n",
    );
    env.write(
        "work/extra/visible/SKILL.md",
        &skill_text("visible", "Shown to the model"),
    );
    let dirs = Dirs::under(env.root());
    let settings = Settings::default();
    let opts = DiscoverOpts {
        extra_skills: vec![
            env.root().join("work/extra/hidden"),
            env.root().join("work/extra/visible"),
        ],
        ..Default::default()
    };
    let (skills, diags) = discover_skills(&dirs, &env.work(), &settings, &opts, &[]);
    assert!(diags.is_empty(), "{diags:?}");
    let hidden = skills
        .iter()
        .find(|s| s.name == "hidden")
        .expect("still discovered");
    assert!(hidden.disable_model_invocation);

    // Not in the prompt section.
    let section = skills_section(&skills);
    assert!(section.contains("<name>visible</name>"));
    assert!(!section.contains("hidden"), "{section}");

    // Still a command: `/skill:hidden` expands to the wrapped instructions plus the request.
    let expanded = expand_skill_command(hidden, "  run it now  ").expect("expands");
    let expected = format!(
        "<skill name=\"hidden\" location=\"{}\">\nReferences are relative to {}.\n\nSecret steps\n</skill>\n\nrun it now",
        env.root().join("work/extra/hidden/SKILL.md").display(),
        env.root().join("work/extra/hidden").display(),
    );
    assert_eq!(expanded, expected);
    // Without a request, the block alone.
    let bare = expand_skill_command(hidden, "").unwrap();
    assert!(bare.ends_with("\n</skill>"));
}

/// O-152: oxa's built-in skills are always discovered; these tests are about the other sources.
fn without_builtins(skills: Vec<oxa_resources::Skill>) -> Vec<oxa_resources::Skill> {
    skills
        .into_iter()
        .filter(|s| s.source != ResourceSource::Builtin)
        .collect()
}
