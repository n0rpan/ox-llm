//! #602: file tools must retain the sandbox's explicit host mounts beneath /tmp.
use oxa_fake::TestEnv;
use oxa_proto::{Access, CancelToken, ExecSpec, Mode, Sandbox, Tool, ToolCtx, ToolOutput, Value};
use oxa_sandbox::{Bwrap, GitBinds, Layout, NetSandbox, Scratch};
use oxa_tools::{EditTool, FindTool, GrepTool, LsTool, ReadTool, WriteTool};
use serde_json::json;
use std::path::Path;

fn tool(tool: &dyn Tool, sandbox: &dyn Sandbox, env: &TestEnv, args: Value) -> (Vec<Access>, ToolOutput) {
    let cwd = env.work();
    let spill = env.agent_dir().join("tmp/spill");
    let cancel = CancelToken::new();
    let cx = ToolCtx {
        checked: &[],
        cwd: &cwd,
        spill_dir: &spill,
        sandbox,
        mode: Mode::Workspace,
        cancel: &cancel,
        env: &[],
        tune: Default::default(),
        hidden: None,
    };
    (
        tool.access(&args, &cx).unwrap(),
        tool.run(&args, &cx, &mut |_| {}),
    )
}

#[test]
fn file_tools_preserve_tmp_mounts_git_workspace_and_agent_paths() {
    let env = TestEnv::new();
    assert!(env.root().starts_with("/tmp"));
    let roots = ["shared-ro", "shared-rw", "gitdir", "common", "objects", "repo"];
    for root in roots {
        env.write(&format!("{root}/note.txt"), "needle\n");
    }
    let layout = Layout {
        read: vec![env.root().join("shared-ro")],
        write: vec![env.root().join("shared-rw")],
        agent_dir: Some(env.agent_dir()),
        git: GitBinds {
            rw: vec![env.root().join("gitdir"), env.root().join("common")],
            ro: vec![env.root().join("objects")],
            worktree: Some(env.root().join("repo")),
            ..Default::default()
        },
        ..Default::default()
    };
    let sb = Bwrap {
        bwrap: "/usr/bin/bwrap".into(),
        home: env.home(),
        layout: layout.with_scratch(Scratch::create(&env.tmp()).unwrap()),
    };
    // Exercise the production wrapper too: it must forward the host-path exceptions.
    let sb = NetSandbox::new(std::sync::Arc::new(sb), false, false);
    for root in roots {
        let root = env.root().join(root);
        let file = root.join("note.txt");
        let (access, out) = tool(&ReadTool::new(), &sb, &env, json!({"path": file}));
        assert_eq!(access, vec![Access::Read(file)]);
        assert_eq!(out.text, "needle\n");
        for (t, args) in [
            (Box::new(LsTool::new()) as Box<dyn Tool>, json!({"path": root})),
            (
                Box::new(FindTool::new()),
                json!({"path": root, "pattern": "*.txt"}),
            ),
            (
                Box::new(GrepTool::new()),
                json!({"path": root, "pattern": "needle"}),
            ),
        ] {
            let (access, out) = tool(t.as_ref(), &sb, &env, args);
            assert_eq!(access, vec![Access::Read(root.clone())]);
            assert!(!out.is_error && out.text.contains("note.txt"), "{}", out.text);
        }
    }
    for file in [
        env.agent_dir().join("tmp/spill/out.txt"),
        env.agent_dir().join("tmp/ancestor/out.txt"),
        env.agent_dir().join("builtin-skills/example/SKILL.md"),
        env.home().join(".ssh/id_test"),
    ] {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "host output\n").unwrap();
        let (access, out) = tool(&ReadTool::new(), &sb, &env, json!({"path": file}));
        assert_eq!(access, vec![Access::Read(file)]);
        assert_eq!(out.text, "host output\n");
        // Only path identity is tested here; sensitive paths are still denied by ModePolicy.
    }
}

fn bash(sb: &Bwrap, cwd: &Path, command: String) -> String {
    let out = sb.run(
        &ExecSpec {
            command,
            cwd: cwd.to_path_buf(),
            writable: true,
            net: false,
            timeout_s: Some(10),
            env: vec![],
        },
        &CancelToken::new(),
        &mut |_| {},
    );
    assert_eq!(out.exit, Some(0), "{}", String::from_utf8_lossy(&out.output));
    String::from_utf8(out.output).unwrap()
}

#[test]
fn bash_and_file_tools_agree_on_scratch_and_explicit_tmp_binds() {
    let env = TestEnv::new();
    let sb = Bwrap::detect(&std::env::var("PATH").unwrap(), &env.home()).unwrap();
    let shared = env
        .write("shared/note.txt", "host\n")
        .parent()
        .unwrap()
        .to_path_buf();
    let sb = Bwrap {
        layout: Layout {
            write: vec![shared.clone()],
            agent_dir: Some(env.agent_dir()),
            ..Default::default()
        }
        .with_scratch(Scratch::create(&env.tmp()).unwrap()),
        ..sb
    };
    bash(
        &sb,
        &env.work(),
        format!(
            "printf 'before\\n' > /tmp/note.txt; printf 'before\\n' > '{}/note.txt'",
            shared.display()
        ),
    );
    for file in [Path::new("/tmp/note.txt").to_path_buf(), shared.join("note.txt")] {
        let (_, out) = tool(&ReadTool::new(), &sb, &env, json!({"path": file}));
        assert_eq!(out.text, "before\n");
        let (_, out) = tool(
            &EditTool::new(),
            &sb,
            &env,
            json!({"path": file, "oldText": "before", "newText": "edited"}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            bash(&sb, &env.work(), format!("cat '{}'", file.display())),
            "edited\n"
        );
        let (_, out) = tool(
            &WriteTool::new(),
            &sb,
            &env,
            json!({"path": file, "content": "written\n"}),
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            bash(&sb, &env.work(), format!("cat '{}'", file.display())),
            "written\n"
        );
    }
}
