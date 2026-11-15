//! O-152: oxa's built-in skills, the system (Omarchy) skills directory as the lowest-precedence
//! source, and the identity file. Isolation: `TestEnv` only; the system directory is a test dir.

use oxa_config::{dirs_from_env, Dirs, Env, Settings, OMARCHY_SKILLS_DIR};
use oxa_fake::TestEnv;
use oxa_resources::{
    builtin_skill_path, discover, discover_skills, expand_skill_command, install_builtin_skills,
    load_skill_file, read_identity, validate_skill_name, DiscoverOpts, ResourceSource, Skill, BUILTIN_SKILLS,
    MAX_SKILL_DESCRIPTION,
};
use serde_json::json;

fn skill_text(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\nBody of {name}.\n")
}

fn dirs_with_system(env: &TestEnv) -> Dirs {
    let mut dirs = Dirs::under(env.root());
    dirs.system_skills_dir = Some(env.root().join("system-skills"));
    dirs
}

fn find<'a>(skills: &'a [Skill], name: &str) -> &'a Skill {
    skills
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no skill {name}"))
}

#[test]
fn builtin_skills_are_valid_and_listed() {
    let names: Vec<&str> = BUILTIN_SKILLS.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, vec!["ox-llm", "oxa-extend", "report-issue"]);
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let (skills, diags) = discover_skills(
        &dirs,
        &env.work(),
        &Settings::default(),
        &DiscoverOpts::default(),
        &[],
    );
    assert!(diags.is_empty(), "{diags:?}");
    for (name, _) in BUILTIN_SKILLS {
        let s = find(&skills, name);
        assert_eq!(s.source, ResourceSource::Builtin);
        assert_eq!(s.path, builtin_skill_path(&dirs, name));
        assert!(validate_skill_name(name).is_empty());
        assert!(!s.description.is_empty() && s.description.chars().count() <= MAX_SKILL_DESCRIPTION);
        assert!(!s.disable_model_invocation);
    }
    // `oxa.builtinSkills: false` and `-ns` turn them off.
    let off = Settings::from_values(json!({"oxa": {"builtinSkills": false}}), json!({}));
    let (skills, _) = discover_skills(&dirs, &env.work(), &off, &DiscoverOpts::default(), &[]);
    assert!(skills.iter().all(|s| s.source != ResourceSource::Builtin));
    let ns = DiscoverOpts {
        no_skills: true,
        ..Default::default()
    };
    let (skills, _) = discover_skills(&dirs, &env.work(), &Settings::default(), &ns, &[]);
    assert!(skills.is_empty());
}

#[test]
fn builtin_files_install_once_and_expand_without_install() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    let (skills, _) = discover_skills(
        &dirs,
        &env.work(),
        &Settings::default(),
        &DiscoverOpts::default(),
        &[],
    );
    // Not written yet: `/skill:ox-llm` expands from the embedded copy.
    let text = expand_skill_command(find(&skills, "ox-llm"), "status?").expect("expand");
    assert!(
        text.starts_with("<skill name=\"ox-llm\"")
            && text.contains("oxc status")
            && text.ends_with("status?")
    );

    let written = install_builtin_skills(&dirs).expect("install");
    assert_eq!(written.len(), BUILTIN_SKILLS.len());
    assert!(
        install_builtin_skills(&dirs).expect("again").is_empty(),
        "current files are not rewritten"
    );
    for (name, content) in BUILTIN_SKILLS {
        let path = builtin_skill_path(&dirs, name);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), *content);
        // The installed file loads like any SKILL.md, with the same name and description.
        let (loaded, diags) = load_skill_file(&path, ResourceSource::Builtin).expect("load");
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(&loaded, find(&skills, name));
    }
    // A stale file (an older oxa) is replaced.
    std::fs::write(builtin_skill_path(&dirs, "ox-llm"), "old").unwrap();
    assert_eq!(
        install_builtin_skills(&dirs).unwrap(),
        vec![builtin_skill_path(&dirs, "ox-llm")]
    );
}

/// The Omarchy fallback: read last, deduped by name with the documented precedence (every
/// other source, then the built-ins, then the system directory), losers of the two fallback
/// sources dropped without a warning.
#[test]
fn omarchy_system_dir_is_the_lowest_precedence_fallback() {
    let env = TestEnv::new();
    let dirs = dirs_with_system(&env);
    env.write(
        "system-skills/omarchy/SKILL.md",
        &skill_text("omarchy", "from the system"),
    );
    env.write(
        "system-skills/shared/SKILL.md",
        &skill_text("shared", "from the system"),
    );
    env.write(
        "system-skills/ox-llm/SKILL.md",
        &skill_text("ox-llm", "system copy"),
    );
    env.write("system-skills/mine/SKILL.md", &skill_text("mine", "system copy"));
    env.write(
        "home/.agents/skills/shared/SKILL.md",
        &skill_text("shared", "from ~/.agents"),
    );
    env.write(
        "home/.oxa/agent/skills/mine/SKILL.md",
        &skill_text("mine", "from the agent dir"),
    );
    env.write(
        "work/.agents/skills/oxa-extend/SKILL.md",
        &skill_text("oxa-extend", "project override"),
    );

    let trusted = DiscoverOpts {
        trusted: true,
        ..Default::default()
    };
    let (skills, diags) = discover_skills(&dirs, &env.work(), &Settings::default(), &trusted, &[]);
    assert!(diags.is_empty(), "fallback losers are silent: {diags:?}");
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "mine",
            "omarchy",
            "ox-llm",
            "oxa-extend",
            "report-issue",
            "shared"
        ],
        "deduped by name, sorted"
    );
    assert_eq!(find(&skills, "omarchy").source, ResourceSource::System);
    assert_eq!(
        find(&skills, "omarchy").path,
        env.root().join("system-skills/omarchy/SKILL.md")
    );
    assert_eq!(find(&skills, "shared").description, "from ~/.agents");
    assert_eq!(find(&skills, "mine").source, ResourceSource::User);
    assert_eq!(
        find(&skills, "ox-llm").source,
        ResourceSource::Builtin,
        "a built-in beats the system copy"
    );
    assert_eq!(
        find(&skills, "oxa-extend").description,
        "project override",
        "a user skill replaces a built-in"
    );
    assert_eq!(find(&skills, "report-issue").source, ResourceSource::Builtin);

    // A ~/.agents/skills entry that links into the system directory is one skill (first wins).
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            env.root().join("system-skills/omarchy"),
            env.home().join(".agents/skills/omarchy"),
        )
        .unwrap();
        let (skills, diags) = discover_skills(&dirs, &env.work(), &Settings::default(), &trusted, &[]);
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(skills.iter().filter(|s| s.name == "omarchy").count(), 1);
        assert_eq!(find(&skills, "omarchy").source, ResourceSource::Agents);
    }

    // No system directory (tests, other distributions): nothing from it, no warning.
    let plain = Dirs::under(env.root());
    let (skills, diags) = discover_skills(&plain, &env.work(), &Settings::default(), &trusted, &[]);
    assert!(diags.is_empty(), "{diags:?}");
    assert!(skills.iter().all(|s| s.source != ResourceSource::System));
    // A configured but missing directory is not an error either.
    let mut missing = Dirs::under(env.root());
    missing.system_skills_dir = Some(env.root().join("does-not-exist"));
    let (_, diags) = discover_skills(&missing, &env.work(), &Settings::default(), &trusted, &[]);
    assert!(diags.is_empty(), "{diags:?}");
}

#[test]
fn system_skills_dir_from_the_environment() {
    let env = TestEnv::new();
    let home = env.home().to_string_lossy().into_owned();
    let d = dirs_from_env(&Env::from_pairs(&[("HOME", &home)]), None).unwrap();
    assert_eq!(
        d.system_skills_dir.as_deref(),
        Some(std::path::Path::new(OMARCHY_SKILLS_DIR))
    );
    let d = dirs_from_env(
        &Env::from_pairs(&[("HOME", &home), ("OXA_SYSTEM_SKILLS_DIR", "")]),
        None,
    )
    .unwrap();
    assert_eq!(d.system_skills_dir, None, "empty = none");
    let d = dirs_from_env(
        &Env::from_pairs(&[("HOME", &home), ("OXA_SYSTEM_SKILLS_DIR", "~/sk")]),
        None,
    )
    .unwrap();
    assert_eq!(d.system_skills_dir, Some(env.home().join("sk")));
    assert_eq!(Dirs::under(env.root()).system_skills_dir, None);
}

/// A missing identity is `None` (the prefix then uses the generic text), never an error.
#[test]
fn identity_file_is_read_once_or_missing() {
    let env = TestEnv::new();
    let dirs = Dirs::under(env.root());
    assert_eq!(read_identity(&dirs), None);
    let r = discover(&dirs, &env.work(), &Settings::default(), &DiscoverOpts::default());
    assert_eq!(r.identity, None);
    assert!(r.diagnostics.is_empty(), "{:?}", r.diagnostics);
    env.write("home/.oxa/agent/identity.md", "   \n");
    assert_eq!(read_identity(&dirs), None, "blank = missing");
    env.write(
        "home/.oxa/agent/identity.md",
        "oxa is the agent.\n- Machine: test\n",
    );
    assert_eq!(
        read_identity(&dirs).as_deref(),
        Some("oxa is the agent.\n- Machine: test")
    );
    let r = discover(&dirs, &env.work(), &Settings::default(), &DiscoverOpts::default());
    assert_eq!(r.identity.as_deref(), Some("oxa is the agent.\n- Machine: test"));
}

/// A fake HOME laid out like a real one: `~/.agents/skills` holds two plain skills (one a
/// symlink into the system directory, as Omarchy installs them) and a `synced/<id>/` tree of
/// skills another product wrote for its own tools, with its `manifest.json`. The synced ones
/// are skipped by default and listed with `oxa.skills.includeSynced: true`; a sync manifest
/// outside a `synced/` folder is recognised too.
#[test]
fn synced_skills_under_agents_home_are_opt_in() {
    let env = TestEnv::new();
    let dirs = dirs_with_system(&env);
    let write = |p: std::path::PathBuf, name: &str| {
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("SKILL.md"), skill_text(name, &format!("Use {name}."))).unwrap();
    };
    let agents = dirs.home.join(".agents").join("skills");
    let system = dirs.system_skills_dir.clone().unwrap();
    write(system.join("omarchy"), "omarchy");
    std::fs::create_dir_all(&agents).unwrap();
    std::os::unix::fs::symlink(system.join("omarchy"), agents.join("omarchy")).unwrap();
    write(agents.join("diagnose-crash"), "diagnose-crash");
    let bucket = agents.join("synced").join("4d5e-aecf");
    for name in ["computer-use", "docx", "pdf"] {
        write(bucket.join(name), name);
    }
    std::fs::write(
        bucket.join("manifest.json"),
        r#"{"skills":[{"skillId":"pdf","name":"pdf"}]}"#,
    )
    .unwrap();
    std::fs::write(agents.join("synced").join(".bucket-4d5e-aecf"), "").unwrap();
    // The same kind of sync tree under another folder name, found by its manifest.
    let other = agents.join("from-elsewhere");
    write(other.join("xlsx"), "xlsx");
    std::fs::write(other.join("manifest.json"), r#"{"skills":[{"skillId":"xlsx"}]}"#).unwrap();

    let names = |settings: &Settings| -> Vec<String> {
        let (skills, diags) = discover_skills(&dirs, &env.work(), settings, &DiscoverOpts::default(), &[]);
        assert!(diags.is_empty(), "{diags:?}");
        skills
            .into_iter()
            .filter(|s| !matches!(s.source, ResourceSource::Builtin))
            .map(|s| s.name)
            .collect()
    };
    assert_eq!(names(&Settings::default()), ["diagnose-crash", "omarchy"]);
    let on = Settings::from_values(json!({"oxa": {"skills": {"includeSynced": true}}}), json!({}));
    assert_eq!(
        names(&on),
        ["computer-use", "diagnose-crash", "docx", "omarchy", "pdf", "xlsx"]
    );
}
