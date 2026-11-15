//! C5 acceptance tests: Slash-command registry.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{
    all_commands, builtin_commands, expand_input, Expanded, Queue, SessionState, Shared, Stats, Toolset,
};
use oxa_fake::{ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    CancelToken, CommandSpec, Engine, FrozenPrefix, HookPoint, HookReply, Hooks, Mode, OxaError, Policy,
    Source, ThinkingLevel, Tool, Ui, Value,
};
use oxa_resources::{expand_skill_command, ResourceSource, Resources, Skill, Template};
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore, Transcript, Tree};
use sha2::{Digest, Sha256};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------------
// Harness

/// A hook set with a fixed command list and no other behaviour.
struct FakeHooks {
    cmds: Vec<CommandSpec>,
}

impl Hooks for FakeHooks {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        self.cmds.clone()
    }
    fn run_command(&self, _name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

fn hook_cmd(name: &str, description: &str) -> CommandSpec {
    CommandSpec {
        name: name.to_string(),
        description: description.to_string(),
        argument_hint: None,
        source: Source::Extension("ext.js".to_string()),
    }
}

fn template(env: &TestEnv, name: &str, description: &str, hint: Option<&str>, body: &str) -> Template {
    Template {
        name: name.to_string(),
        description: description.to_string(),
        argument_hint: hint.map(str::to_string),
        body: body.to_string(),
        path: env.root().join(format!("prompts/{name}.md")),
        source: ResourceSource::User,
    }
}

/// A skill whose `SKILL.md` is written under the test root.
fn skill(env: &TestEnv, name: &str, description: &str, body: &str, hidden: bool) -> Skill {
    let path = env.write(
        &format!("skills/{name}/SKILL.md"),
        &format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    );
    Skill {
        name: name.to_string(),
        description: description.to_string(),
        base_dir: path.parent().expect("skill dir").to_path_buf(),
        path,
        source: ResourceSource::User,
        disable_model_invocation: hidden,
    }
}

fn state(
    env: &TestEnv,
    hooks: Vec<Arc<dyn Hooks>>,
    resources: Resources,
    settings: Settings,
) -> SessionState {
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let store = Arc::new(MemStore::new());
    let cwd = env.work();
    let sref = store.create(&cwd, None, None).expect("create session");
    let info = engine.info(None).expect("engine info");
    let sections = vec![("preamble".to_string(), "You are a test agent.".to_string())];
    let mut prefix = FrozenPrefix {
        sections,
        tools_json: "[]".to_string(),
        sha256: [0u8; 32],
    };
    prefix.sha256 = Sha256::digest(prefix.hash_input()).into();
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue::default()),
        cancel: CancelToken::new(),
        cancel_retry: CancelToken::new(),
        busy: AtomicBool::new(false),
        modes: Mutex::new((QueueMode::All, QueueMode::OneAtATime)),
        dirty: AtomicBool::new(false),
        gate: Default::default(),
    });
    SessionState {
        engine,
        store,
        hooks,
        policy: Box::new(ModePolicy::new()) as Box<dyn Policy>,
        sandbox: Arc::new(NoSandbox),
        settings,
        dirs: Dirs::under(env.root()),
        cwd: cwd.clone(),
        all_tools: Vec::new(),
        selection: Default::default(),
        active: Toolset {
            direct: Vec::new(),
            indirect: Vec::new(),
        },
        resources,
        sref,
        tree: Tree::new(Vec::new()),
        transcript: Transcript::default(),
        prefix,
        info,
        model: "fake-model".to_string(),
        thinking: ThinkingLevel::Off,
        mode: Mode::Workspace,
        compact: CompactPolicy {
            soft: u32::MAX,
            hard: u32::MAX,
            keep_tokens: 4000,
            summary_max: 1024,
            enabled: true,
        },
        shared,
        next_turn: Vec::new(),
        last_usage: None,
        stats: Stats::default(),
        spill_dir: env.tmp(),
        tool_env: Vec::new(),
        tune: Default::default(),
        auto_compaction: true,
        auto_retry: true,
        host_rx: None,
        host_deferred: Vec::new(),
        compact_memo: Default::default(),
        jobs: oxa_core::fresh_jobs(),
    }
}

fn plain(env: &TestEnv) -> SessionState {
    state(env, Vec::new(), Resources::default(), Settings::default())
}

fn names(cmds: &[CommandSpec]) -> Vec<String> {
    cmds.iter().map(|c| c.name.clone()).collect()
}

// ---------------------------------------------------------------------------------------------
// Tests

/// every kept command of slash-commands.md + /engine + /mode
#[test]
fn builtins_list() {
    let got = names(&builtin_commands());
    let expected: Vec<String> = [
        "settings",
        "model",
        "tree",
        "thinking",
        "engine",
        "mode",
        "scoped-models",
        "export",
        "bug",
        "copy",
        "name",
        "session",
        "hotkeys",
        "fork",
        "clone",
        "trust",
        "mcp",
        "new",
        "compact",
        "resume",
        "reload",
        "help",
        "usage",
        "quit",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(got, expected);
    for c in builtin_commands() {
        assert!(
            matches!(c.source, Source::Builtin),
            "{} must be Source::Builtin",
            c.name
        );
        assert!(!c.description.is_empty(), "{} needs a description", c.name);
    }
    for omitted in ["login", "logout", "share", "llama"] {
        assert!(
            !got.iter().any(|n| n == omitted),
            "/{omitted} must not be a built-in"
        );
    }
}

/// Expanded::Prompt with arguments applied
#[test]
fn templates_expand_before_message() {
    let env = TestEnv::new();
    let tpl = template(
        &env,
        "greet",
        "Greet",
        Some("<who> <what>"),
        "Hello $1, review ${2:-carefully}: $@",
    );
    let st = state(
        &env,
        Vec::new(),
        Resources {
            templates: vec![tpl],
            ..Resources::default()
        },
        Settings::default(),
    );
    assert_eq!(
        expand_input(&st, "/greet Ann code"),
        Expanded::Prompt("Hello Ann, review code: Ann code".to_string())
    );
    assert_eq!(
        expand_input(&st, "/greet Ann"),
        Expanded::Prompt("Hello Ann, review carefully: Ann".to_string())
    );
    let listed = all_commands(&st);
    let greet = listed
        .iter()
        .find(|c| c.name == "greet")
        .expect("template listed");
    assert_eq!(greet.argument_hint.as_deref(), Some("<who> <what>"));
}

/// /skill:name
#[test]
fn skill_command() {
    let env = TestEnv::new();
    let sk = skill(&env, "review", "Review code", "Check the diff.", true);
    let expected = expand_skill_command(&sk, "look at main.rs").expect("skill reads");
    let st = state(
        &env,
        Vec::new(),
        Resources {
            skills: vec![sk.clone()],
            ..Resources::default()
        },
        Settings::default(),
    );
    let got = expand_input(&st, "/skill:review look at main.rs");
    assert_eq!(got, Expanded::Prompt(expected.clone()));
    assert!(expected.starts_with("<skill name=\"review\""));
    assert!(expected.contains("Check the diff."));
    assert!(expected.ends_with("\n\nlook at main.rs"));

    // Hidden from the model, but still a command.
    let listed = all_commands(&st);
    let cmd = listed
        .iter()
        .find(|c| c.name == "skill:review")
        .expect("skill command listed");
    assert_eq!(cmd.description, "Review code");

    // enableSkillCommands = false: pi still lists the skills (agent-session.ts getCommands) and a
    // typed /skill:name still expands (`_expandSkillCommand`); only the interactive autocomplete
    // leaves them out (interactive-mode.ts), which is oxa-interactive's business.
    let off = state(
        &env,
        Vec::new(),
        Resources {
            skills: vec![sk.clone()],
            ..Resources::default()
        },
        Settings::from_values(serde_json::json!({ "enableSkillCommands": false }), Value::Null),
    );
    assert!(names(&all_commands(&off)).iter().any(|n| n == "skill:review"));
    assert_ne!(
        expand_input(&off, "/skill:review x"),
        Expanded::Prompt("/skill:review x".to_string()),
        "a typed /skill:name still expands"
    );
}

/// Expanded::Hook
#[test]
fn hook_commands_listed_and_dispatched() {
    let env = TestEnv::new();
    let hooks: Vec<Arc<dyn Hooks>> = vec![Arc::new(FakeHooks {
        cmds: vec![hook_cmd("greet", "Say hi")],
    })];
    let st = state(&env, hooks, Resources::default(), Settings::default());
    assert_eq!(
        expand_input(&st, "/greet  world now"),
        Expanded::Hook {
            name: "greet".to_string(),
            args: "world now".to_string()
        }
    );
    let listed = all_commands(&st);
    let greet = listed
        .iter()
        .find(|c| c.name == "greet")
        .expect("hook command listed");
    assert_eq!(greet.description, "Say hi");
    assert!(matches!(greet.source, Source::Extension(_)));
}

/// builtin > extension > template > skill
#[test]
fn name_clash_order_as_pi() {
    let env = TestEnv::new();
    let hooks: Vec<Arc<dyn Hooks>> = vec![Arc::new(FakeHooks {
        cmds: vec![hook_cmd("model", "hook model"), hook_cmd("deploy", "hook deploy")],
    })];
    let resources = Resources {
        templates: vec![
            template(&env, "deploy", "tmpl deploy", None, "tmpl deploy body"),
            template(&env, "skill:review", "tmpl review", None, "from template"),
        ],
        skills: vec![skill(&env, "review", "skill review", "from skill", false)],
        ..Resources::default()
    };
    let st = state(&env, hooks, resources, Settings::default());

    let listed = all_commands(&st);
    let model: Vec<&CommandSpec> = listed.iter().filter(|c| c.name == "model").collect();
    assert_eq!(model.len(), 1);
    assert!(
        matches!(model[0].source, Source::Builtin),
        "builtin beats extension"
    );

    let deploy: Vec<&CommandSpec> = listed.iter().filter(|c| c.name == "deploy").collect();
    assert_eq!(deploy.len(), 1);
    assert_eq!(deploy[0].description, "hook deploy", "extension beats template");

    let review: Vec<&CommandSpec> = listed.iter().filter(|c| c.name == "skill:review").collect();
    assert_eq!(review.len(), 1);
    assert_eq!(review[0].description, "tmpl review", "template beats skill");

    // Order: builtins, then extensions, then templates, then skills.
    let pos = |n: &str| listed.iter().position(|c| c.name == n).expect(n);
    assert!(
        pos("quit") < pos("deploy"),
        "every built-in comes before extensions"
    );
    assert!(pos("model") < pos("deploy"));
    assert!(pos("deploy") < pos("skill:review"));

    // Dispatch follows the same order.
    assert_eq!(
        expand_input(&st, "/model"),
        Expanded::Builtin {
            name: "model".into(),
            args: String::new()
        }
    );
    assert_eq!(
        expand_input(&st, "/deploy now"),
        Expanded::Hook {
            name: "deploy".into(),
            args: "now".into()
        }
    );
    assert_eq!(
        expand_input(&st, "/skill:review"),
        Expanded::Prompt("from template".to_string())
    );
}

/// Command shape: name without slash, description, argument hint, source.
#[test]
fn get_commands_shape() {
    let env = TestEnv::new();
    let tpl = template(&env, "tidy", "Tidy the file", Some("<file>"), "Tidy $1");
    let tpl_path = tpl.path.clone();
    let sk = skill(&env, "lint", "Lint code", "Run the linter.", false);
    let st = state(
        &env,
        Vec::new(),
        Resources {
            templates: vec![tpl],
            skills: vec![sk],
            ..Resources::default()
        },
        Settings::default(),
    );
    let listed = all_commands(&st);
    assert!(
        listed.iter().all(|c| !c.name.starts_with('/')),
        "names carry no slash"
    );

    let model = listed.iter().find(|c| c.name == "model").expect("model");
    assert_eq!(model.argument_hint.as_deref(), Some("[model]"));
    assert!(matches!(model.source, Source::Builtin));

    let tidy = listed.iter().find(|c| c.name == "tidy").expect("template");
    assert_eq!(tidy.description, "Tidy the file");
    assert_eq!(tidy.argument_hint.as_deref(), Some("<file>"));
    assert_eq!(tidy.source, Source::Extension(tpl_path.display().to_string()));

    let lint = listed.iter().find(|c| c.name == "skill:lint").expect("skill");
    assert_eq!(lint.description, "Lint code");
    assert_eq!(lint.argument_hint, None);
}

/// pi behaviour
#[test]
fn unknown_slash_is_prompt_text() {
    let env = TestEnv::new();
    let st = plain(&env);
    assert_eq!(
        expand_input(&st, "/nosuch with args"),
        Expanded::Prompt("/nosuch with args".to_string())
    );
    assert_eq!(
        expand_input(&st, "  /nosuch"),
        Expanded::Prompt("  /nosuch".to_string())
    );
    assert_eq!(expand_input(&st, "/"), Expanded::Prompt("/".to_string()));
    assert_eq!(
        expand_input(&st, "/skill:missing x"),
        Expanded::Prompt("/skill:missing x".to_string())
    );
    assert_eq!(
        expand_input(&st, "just words"),
        Expanded::Prompt("just words".to_string())
    );
}

/// Expanded::Bash
#[test]
fn bang_and_double_bang() {
    let env = TestEnv::new();
    let st = plain(&env);
    assert_eq!(
        expand_input(&st, "!ls -la"),
        Expanded::Bash {
            command: "ls -la".to_string(),
            excluded: false
        }
    );
    assert_eq!(
        expand_input(&st, "!!git status"),
        Expanded::Bash {
            command: "git status".to_string(),
            excluded: true
        }
    );
    assert_eq!(
        expand_input(&st, "  !! echo hi  "),
        Expanded::Bash {
            command: "echo hi".to_string(),
            excluded: true
        }
    );
    // A bare `!` has no command: it is ordinary text.
    assert_eq!(expand_input(&st, "!"), Expanded::Prompt("!".to_string()));
}

/// /login /share
#[test]
fn omitted_commands_unsupported() {
    let env = TestEnv::new();
    let st = plain(&env);
    for (line, name, args) in [
        ("/login", "login", ""),
        ("/share", "share", ""),
        ("/logout now", "logout", "now"),
    ] {
        assert_eq!(
            expand_input(&st, line),
            Expanded::Builtin {
                name: name.to_string(),
                args: args.to_string()
            },
            "{line}"
        );
    }
    // Not listed, so the front end reports unsupported_in_oxa.
    let listed = names(&all_commands(&st));
    for omitted in ["login", "logout", "share"] {
        assert!(!listed.iter().any(|n| n == omitted));
        assert!(!builtin_commands().iter().any(|c| c.name == omitted));
    }
    // Not reserved: a template with the same name still runs.
    let tpl = template(&env, "share", "Share notes", None, "notes please");
    let with_tpl = state(
        &env,
        Vec::new(),
        Resources {
            templates: vec![tpl],
            ..Resources::default()
        },
        Settings::default(),
    );
    assert_eq!(
        expand_input(&with_tpl, "/share"),
        Expanded::Prompt("notes please".to_string())
    );
}

#[test]
fn engine_and_mode_have_argument_hints() {
    let cmds = builtin_commands();
    let engine = cmds.iter().find(|c| c.name == "engine").expect("engine");
    assert!(engine.argument_hint.as_deref().unwrap_or("").contains("load"));
    let mode = cmds.iter().find(|c| c.name == "mode").expect("mode");
    assert_eq!(mode.argument_hint.as_deref(), Some("<plan|workspace|ask>"));
}

#[test]
fn bare_slash_builtin_with_args_is_trimmed() {
    let env = TestEnv::new();
    let st = plain(&env);
    assert_eq!(
        expand_input(&st, "/model   fake-model  "),
        Expanded::Builtin {
            name: "model".to_string(),
            args: "fake-model".to_string()
        }
    );
    assert_eq!(
        expand_input(&st, "/thinking high"),
        Expanded::Builtin {
            name: "thinking".to_string(),
            args: "high".to_string()
        }
    );
}
