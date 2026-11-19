//! A10 acceptance tests: Mode policy table and path containment.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_perm::{deny_reasons, is_inside, is_read_only_command, normalize, ModePolicy};
use oxa_proto::{Access, Decision, Mode, Policy, ToolClass};
use std::path::{Path, PathBuf};

fn decide(p: &ModePolicy, mode: Mode, a: &Access, cwd: &Path) -> Decision {
    p.decide(mode, a, cwd)
}

fn exec(command: &str, net: bool) -> Access {
    Access::Exec {
        command: command.to_string(),
        net,
    }
}

/// one assertion per cell of the crate-doc table
#[test]
fn table_mode_x_access() {
    let env = TestEnv::new();
    let cwd = env.work();
    let inside = cwd.join("a.txt");
    let outside = env.tmp().join("b.txt");
    let p = ModePolicy::new();
    let deny_plan_write = Decision::Deny(deny_reasons::PLAN_WRITE);
    let deny_plan_exec = Decision::Deny(deny_reasons::PLAN_EXEC);
    let deny_plan_net = Decision::Deny(deny_reasons::PLAN_NET);
    let deny_plan_mcp = Decision::Deny(deny_reasons::PLAN_MCP);

    // Read inside cwd
    assert_eq!(
        decide(&p, Mode::Plan, &Access::Read(inside.clone()), &cwd),
        Decision::Allow,
        "plan read inside"
    );
    assert_eq!(
        decide(&p, Mode::Workspace, &Access::Read(inside.clone()), &cwd),
        Decision::Allow,
        "ws read inside"
    );
    assert_eq!(
        decide(&p, Mode::Ask, &Access::Read(inside.clone()), &cwd),
        Decision::Allow,
        "ask read inside"
    );
    // Read outside cwd
    assert_eq!(
        decide(&p, Mode::Plan, &Access::Read(outside.clone()), &cwd),
        Decision::Ask,
        "plan read outside"
    );
    assert_eq!(
        decide(&p, Mode::Workspace, &Access::Read(outside.clone()), &cwd),
        Decision::Ask,
        "ws read outside"
    );
    assert_eq!(
        decide(&p, Mode::Ask, &Access::Read(outside.clone()), &cwd),
        Decision::Ask,
        "ask read outside"
    );
    // Write inside cwd
    assert_eq!(
        decide(&p, Mode::Plan, &Access::Write(inside.clone()), &cwd),
        deny_plan_write,
        "plan write inside"
    );
    assert_eq!(
        decide(&p, Mode::Workspace, &Access::Write(inside.clone()), &cwd),
        Decision::Allow,
        "ws write inside"
    );
    assert_eq!(
        decide(&p, Mode::Ask, &Access::Write(inside.clone()), &cwd),
        Decision::Ask,
        "ask write inside"
    );
    // Write outside cwd
    assert_eq!(
        decide(&p, Mode::Plan, &Access::Write(outside.clone()), &cwd),
        deny_plan_write,
        "plan write outside"
    );
    assert_eq!(
        decide(&p, Mode::Workspace, &Access::Write(outside.clone()), &cwd),
        Decision::Ask,
        "ws write outside"
    );
    assert_eq!(
        decide(&p, Mode::Ask, &Access::Write(outside.clone()), &cwd),
        Decision::Ask,
        "ask write outside"
    );
    // Exec, no net, read-only command
    assert_eq!(
        decide(&p, Mode::Plan, &exec("ls -la", false), &cwd),
        Decision::Allow,
        "plan ro exec"
    );
    assert_eq!(
        decide(&p, Mode::Workspace, &exec("ls -la", false), &cwd),
        Decision::Allow,
        "ws ro exec"
    );
    assert_eq!(
        decide(&p, Mode::Ask, &exec("ls -la", false), &cwd),
        Decision::Ask,
        "ask ro exec"
    );
    // Exec, no net, other command
    assert_eq!(
        decide(&p, Mode::Plan, &exec("rm -rf x", false), &cwd),
        deny_plan_exec,
        "plan other exec"
    );
    assert_eq!(
        decide(&p, Mode::Workspace, &exec("rm -rf x", false), &cwd),
        Decision::Allow,
        "ws other exec (sandboxed)"
    );
    assert_eq!(
        decide(&p, Mode::Ask, &exec("rm -rf x", false), &cwd),
        Decision::Ask,
        "ask other exec"
    );
    // Exec, net
    assert_eq!(
        decide(&p, Mode::Plan, &exec("curl x", true), &cwd),
        deny_plan_net,
        "plan net exec"
    );
    assert_eq!(
        decide(&p, Mode::Workspace, &exec("curl x", true), &cwd),
        Decision::Ask,
        "ws net exec"
    );
    assert_eq!(
        decide(&p, Mode::Ask, &exec("curl x", true), &cwd),
        Decision::Ask,
        "ask net exec"
    );
    // Net
    let net = Access::Net {
        host: "api.example".to_string(),
    };
    assert_eq!(decide(&p, Mode::Plan, &net, &cwd), deny_plan_net, "plan net");
    assert_eq!(decide(&p, Mode::Workspace, &net, &cwd), Decision::Ask, "ws net");
    assert_eq!(decide(&p, Mode::Ask, &net, &cwd), Decision::Ask, "ask net");
    // Mcp class Read
    let mcp_read = Access::Mcp {
        server: "s".into(),
        tool: "t".into(),
        class: ToolClass::Read,
    };
    assert_eq!(
        decide(&p, Mode::Plan, &mcp_read, &cwd),
        Decision::Allow,
        "plan mcp read"
    );
    assert_eq!(
        decide(&p, Mode::Workspace, &mcp_read, &cwd),
        Decision::Allow,
        "ws mcp read"
    );
    assert_eq!(
        decide(&p, Mode::Ask, &mcp_read, &cwd),
        Decision::Ask,
        "ask mcp read"
    );
    // Mcp class Write / Exec / Net
    for class in [ToolClass::Write, ToolClass::Exec, ToolClass::Net] {
        let m = Access::Mcp {
            server: "s".into(),
            tool: "t".into(),
            class,
        };
        assert_eq!(
            decide(&p, Mode::Plan, &m, &cwd),
            deny_plan_mcp,
            "plan mcp {class:?}"
        );
        assert_eq!(
            decide(&p, Mode::Workspace, &m, &cwd),
            Decision::Ask,
            "ws mcp {class:?}"
        );
        assert_eq!(
            decide(&p, Mode::Ask, &m, &cwd),
            Decision::Ask,
            "ask mcp {class:?}"
        );
    }
}

/// cwd/../x
#[test]
fn dotdot_escape_is_outside() {
    let env = TestEnv::new();
    let cwd = env.work();
    let escape = cwd.join("../x");
    assert!(!is_inside(&escape, &cwd));
    assert_eq!(
        decide(
            &ModePolicy::new(),
            Mode::Workspace,
            &Access::Write(escape.clone()),
            &cwd
        ),
        Decision::Ask
    );
    assert_eq!(
        decide(&ModePolicy::new(), Mode::Workspace, &Access::Read(escape), &cwd),
        Decision::Ask
    );
    // a `..` that stays inside is inside
    assert!(is_inside(&cwd.join("sub/../y"), &cwd));
    assert!(is_inside(&cwd.join("../work/y"), &cwd));
}

/// link inside cwd pointing out
#[test]
fn symlink_escape_is_outside() {
    let env = TestEnv::new();
    let cwd = env.work();
    let outside_dir = env.tmp().join("outside");
    std::fs::create_dir_all(&outside_dir).unwrap();
    std::os::unix::fs::symlink(&outside_dir, cwd.join("link")).unwrap();
    assert!(!is_inside(&cwd.join("link"), &cwd));
    assert!(!is_inside(&cwd.join("link/file.txt"), &cwd));
    // a not-yet-existing file below the escaping link is outside as well
    assert!(!is_inside(&cwd.join("link/new/deep.txt"), &cwd));
    assert_eq!(
        decide(
            &ModePolicy::new(),
            Mode::Workspace,
            &Access::Write(cwd.join("link/f")),
            &cwd
        ),
        Decision::Ask
    );
    // `link/..` resolves to the real parent of the target, not the lexical one
    std::fs::create_dir_all(env.tmp().join("deep/child")).unwrap();
    std::os::unix::fs::symlink(env.tmp().join("deep/child"), cwd.join("link2")).unwrap();
    assert!(!is_inside(&cwd.join("link2/../x"), &cwd));
}

/// #518: the verdict comes with the real location it covers, which the tool then opens.
#[test]
fn decide_pinned_returns_the_real_location_it_judged() {
    let env = TestEnv::new();
    let cwd = env.work();
    let real_cwd = std::fs::canonicalize(&cwd).unwrap();
    std::fs::create_dir_all(cwd.join("real")).unwrap();
    std::fs::write(cwd.join("real/f.txt"), "x").unwrap();
    std::os::unix::fs::symlink(cwd.join("real/f.txt"), cwd.join("link")).unwrap();
    let p = ModePolicy::new();
    let (d, pin) = p.decide_pinned(Mode::Workspace, &Access::Write(cwd.join("link")), &cwd);
    assert_eq!(d, Decision::Allow);
    assert_eq!(pin, Some(real_cwd.join("real/f.txt")));
    // A file that does not exist yet keeps its tail; the decision is the same as `decide`'s.
    let new = Access::Write(cwd.join("real/sub/new.txt"));
    let (d, pin) = p.decide_pinned(Mode::Workspace, &new, &cwd);
    assert_eq!(d, decide(&p, Mode::Workspace, &new, &cwd));
    assert_eq!(pin, Some(real_cwd.join("real/sub/new.txt")));
    // Only file accesses are pinned.
    let (_, pin) = p.decide_pinned(Mode::Workspace, &exec("ls", false), &cwd);
    assert_eq!(pin, None);
}

#[test]
fn symlink_inside_stays_inside() {
    let env = TestEnv::new();
    let cwd = env.work();
    std::fs::create_dir_all(cwd.join("real")).unwrap();
    std::os::unix::fs::symlink(cwd.join("real"), cwd.join("alias")).unwrap();
    assert!(is_inside(&cwd.join("alias/file.txt"), &cwd));
    assert!(is_inside(&cwd.join("alias"), &cwd));
}

/// with the fixed reason
#[test]
fn plan_denies_write() {
    let env = TestEnv::new();
    let cwd = env.work();
    let mut p = ModePolicy::new();
    let a = Access::Write(cwd.join("f.txt"));
    assert_eq!(
        decide(&p, Mode::Plan, &a, &cwd),
        Decision::Deny("plan mode does not allow writing files")
    );
    assert_eq!(deny_reasons::PLAN_WRITE, "plan mode does not allow writing files");
    // a grant never overrides a Deny
    p.remember(&a);
    assert_eq!(
        decide(&p, Mode::Plan, &a, &cwd),
        Decision::Deny(deny_reasons::PLAN_WRITE)
    );
    assert_eq!(
        decide(&p, Mode::Plan, &Access::Write(env.tmp().join("g")), &cwd),
        Decision::Deny(deny_reasons::PLAN_WRITE)
    );
}

/// remember turns Ask into Allow for the scope only; never overrides Deny
#[test]
fn grants_are_per_session_and_scoped() {
    let env = TestEnv::new();
    let cwd = env.work();
    let mut p = ModePolicy::new();

    // Write: scope is the directory of the file.
    let f = cwd.join("sub/file.txt");
    let other_in_scope = Access::Write(cwd.join("sub/other.txt"));
    let elsewhere = Access::Write(cwd.join("elsewhere/x.txt"));
    assert_eq!(
        decide(&p, Mode::Ask, &Access::Write(f.clone()), &cwd),
        Decision::Ask
    );
    p.remember(&Access::Write(f.clone()));
    p.remember(&Access::Write(f.clone())); // duplicates are not stored twice
    assert_eq!(p.grants().len(), 1);
    assert_eq!(
        decide(&p, Mode::Ask, &Access::Write(f.clone()), &cwd),
        Decision::Allow
    );
    assert_eq!(decide(&p, Mode::Ask, &other_in_scope, &cwd), Decision::Allow);
    assert_eq!(decide(&p, Mode::Ask, &elsewhere, &cwd), Decision::Ask);
    // Write outside cwd, granted for its directory subtree only
    let outside = env.tmp().join("dirA/file.txt");
    p.remember(&Access::Write(outside.clone()));
    assert_eq!(
        decide(
            &p,
            Mode::Workspace,
            &Access::Write(env.tmp().join("dirA/deeper/z.txt")),
            &cwd
        ),
        Decision::Allow
    );
    assert_eq!(
        decide(
            &p,
            Mode::Workspace,
            &Access::Write(env.tmp().join("dirB/z.txt")),
            &cwd
        ),
        Decision::Ask
    );

    // Read grant is separate from Write grant.
    let r = env.tmp().join("reads/one.txt");
    assert_eq!(
        decide(&p, Mode::Ask, &Access::Read(r.clone()), &cwd),
        Decision::Ask
    );
    p.remember(&Access::Read(r.clone()));
    assert_eq!(
        decide(
            &p,
            Mode::Ask,
            &Access::Read(env.tmp().join("reads/two.txt")),
            &cwd
        ),
        Decision::Allow
    );
    assert_eq!(
        decide(
            &p,
            Mode::Ask,
            &Access::Write(env.tmp().join("reads/two.txt")),
            &cwd
        ),
        Decision::Ask
    );

    // Exec, net: exact command only.
    p.remember(&exec("curl https://a", true));
    assert_eq!(
        decide(&p, Mode::Ask, &exec("curl https://a", true), &cwd),
        Decision::Allow
    );
    assert_eq!(
        decide(&p, Mode::Ask, &exec("curl https://b", true), &cwd),
        Decision::Ask
    );

    // Exec, no net: any command in ask mode.
    assert_eq!(decide(&p, Mode::Ask, &exec("rm x", false), &cwd), Decision::Ask);
    p.remember(&exec("rm x", false));
    assert_eq!(
        decide(&p, Mode::Ask, &exec("make all", false), &cwd),
        Decision::Allow
    );
    // the any-command grant never covers network execs
    assert_eq!(decide(&p, Mode::Ask, &exec("curl x", true), &cwd), Decision::Ask);
    assert_eq!(
        decide(&p, Mode::Ask, &exec("curl https://a", true), &cwd),
        Decision::Allow
    );

    // Net host.
    let host = Access::Net {
        host: "api.example".into(),
    };
    p.remember(&host);
    assert_eq!(decide(&p, Mode::Ask, &host, &cwd), Decision::Allow);
    assert_eq!(
        decide(
            &p,
            Mode::Ask,
            &Access::Net {
                host: "other.example".into()
            },
            &cwd
        ),
        Decision::Ask
    );
    assert_eq!(
        decide(&p, Mode::Plan, &host, &cwd),
        Decision::Deny(deny_reasons::PLAN_NET)
    );

    // Mcp: server + tool.
    let mcp_write = Access::Mcp {
        server: "s".into(),
        tool: "t".into(),
        class: ToolClass::Write,
    };
    p.remember(&mcp_write);
    assert_eq!(decide(&p, Mode::Ask, &mcp_write, &cwd), Decision::Allow);
    assert_eq!(
        decide(
            &p,
            Mode::Ask,
            &Access::Mcp {
                server: "s".into(),
                tool: "u".into(),
                class: ToolClass::Write
            },
            &cwd
        ),
        Decision::Ask
    );
    assert_eq!(
        decide(&p, Mode::Plan, &mcp_write, &cwd),
        Decision::Deny(deny_reasons::PLAN_MCP)
    );

    // Per session: a new policy starts with no grants.
    let fresh = ModePolicy::new();
    assert!(fresh.grants().is_empty());
    assert_eq!(decide(&fresh, Mode::Ask, &Access::Write(f), &cwd), Decision::Ask);
}

/// allow and reject cases incl. redirection and substitution
#[test]
fn read_only_command_list() {
    for ok in [
        "ls -la",
        "cat a.txt | head -n 3",
        "git status",
        "git log --oneline && git diff",
        "git branch -a",
        "find . -name '*.rs'",
        "rg foo src",
        "echo 'a > b'",
        "echo \"quoted; not a split\"",
        "pwd",
        "env",
        "uniq in",
        "sort -r x",
        "ls;",
        "tree -L 2",
        "date +%F",
    ] {
        assert!(is_read_only_command(ok), "should be read-only: {ok}");
    }
    for bad in [
        "echo hi > out.txt",
        "cat a >> b",
        "cat < a",
        "echo $(rm -rf x)",
        "echo \"$(id)\"",
        "echo `id`",
        "echo \"`id`\"",
        "ls & rm x",
        "ls |& sh",
        "ls && rm x",
        "ls; rm x",
        "rm x",
        "find . -delete",
        "find . -exec rm {} ;",
        "find . -execdir sh {} ;",
        "sort -o out in",
        "tree -o x",
        "file -C x",
        "date -s 2020-01-01",
        "rg --pre sh x",
        "git push",
        "git branch newbranch",
        "git branch -D old",
        "git -C x status",
        "git diff --output=f",
        "git log --output=f",
        "env rm x",
        "uniq a b",
        "xargs rm",
        "/bin/ls",
        "(ls)",
        "{ ls; }",
        "ls ;; pwd",
        "ls\n\npwd",
        "ls &&",
        "ls |",
        "",
        "   ",
        "echo 'unterminated",
        "echo \"unterminated",
        "ls \\",
    ] {
        assert!(!is_read_only_command(bad), "should NOT be read-only: {bad:?}");
    }
}

/// new file in cwd is inside
#[test]
fn nonexistent_path_judged_by_parent() {
    let env = TestEnv::new();
    let cwd = env.work();
    assert!(is_inside(&cwd.join("new.txt"), &cwd));
    assert!(is_inside(&cwd.join("new/deep/file.txt"), &cwd));
    assert!(!is_inside(&env.tmp().join("new.txt"), &cwd));
    assert_eq!(
        decide(
            &ModePolicy::new(),
            Mode::Workspace,
            &Access::Write(cwd.join("new.txt")),
            &cwd
        ),
        Decision::Allow
    );
}

#[test]
fn normalize_is_lexical() {
    let cwd = Path::new("/x");
    assert_eq!(normalize(Path::new("a/../b"), cwd), PathBuf::from("/x/b"));
    assert_eq!(normalize(Path::new("/a/./b/../c"), cwd), PathBuf::from("/a/c"));
    assert_eq!(normalize(Path::new("../../.."), cwd), PathBuf::from("/"));
}
