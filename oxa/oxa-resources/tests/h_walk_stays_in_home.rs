//! Wave H lane `harden` (CT-10): project resource discovery walks from the cwd upward only
//! within the HOME the process was given. A scratch HOME below a real home must not see the
//! real home's `~/.agents/skills` or `AGENTS.md`; the project's own skills and context files
//! up to that HOME are still found (pi's walk, bounded).
//! Isolation: every path lives under `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_fake::TestEnv;
use oxa_resources::{discover, DiscoverOpts};

const SKILL: &str = "---\nname: {n}\ndescription: skill {n}\n---\nbody\n";

fn skill(env: &TestEnv, rel: &str, name: &str) {
    env.write(&format!("{rel}/{name}/SKILL.md"), &SKILL.replace("{n}", name));
}

#[test]
fn a_scratch_home_below_a_real_home_sees_nothing_of_it() {
    let env = TestEnv::new();
    // The "real" home, holding the scratch home deep below it.
    skill(&env, "realhome/.agents/skills", "real-user-skill");
    env.write("realhome/AGENTS.md", "REAL HOME CONTEXT");
    env.write("realhome/Work/AGENTS.md", "REAL WORK CONTEXT");
    let home = env.root().join("realhome/Work/scratch/home");
    let mut dirs = Dirs::under(env.root());
    dirs.home = home.clone();
    dirs.agent_dir = home.join(".oxa/agent");
    // The scratch home's own context and a project below it.
    env.write("realhome/Work/scratch/home/AGENTS.md", "SCRATCH HOME CONTEXT");
    env.write("realhome/Work/scratch/home/proj/AGENTS.md", "PROJECT CONTEXT");
    skill(
        &env,
        "realhome/Work/scratch/home/proj/.agents/skills",
        "project-skill",
    );
    skill(
        &env,
        "realhome/Work/scratch/home/.agents/skills",
        "scratch-user-skill",
    );
    let cwd = home.join("proj");
    let opts = DiscoverOpts {
        trusted: true,
        ..DiscoverOpts::default()
    };
    let r = discover(&dirs, &cwd, &Settings::default(), &opts);
    let context: Vec<&str> = r.context_files.iter().map(|c| c.content.as_str()).collect();
    assert_eq!(
        context,
        vec!["SCRATCH HOME CONTEXT", "PROJECT CONTEXT"],
        "{context:?}"
    );
    let names: Vec<&str> = r.skills.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"project-skill"), "{names:?}");
    assert!(
        names.contains(&"scratch-user-skill"),
        "the user-level skills come from Dirs: {names:?}"
    );
    assert!(
        !names.contains(&"real-user-skill"),
        "the real home leaked in: {names:?}"
    );
}

#[test]
fn a_cwd_outside_home_walks_like_pi_but_skips_the_homes_user_skills() {
    let env = TestEnv::new();
    // HOME and the cwd are siblings: outside HOME the walk is pi's (parents included), and
    // HOME's own `.agents/skills` is user-level (from `Dirs`), not a project directory.
    env.write("AGENTS.md", "COMMON PARENT CONTEXT");
    env.write("work/AGENTS.md", "WORK CONTEXT");
    skill(&env, "work/.agents/skills", "work-skill");
    let dirs = Dirs::under(env.root());
    let opts = DiscoverOpts {
        trusted: true,
        ..DiscoverOpts::default()
    };
    let r = discover(&dirs, &env.work(), &Settings::default(), &opts);
    let context: Vec<&str> = r
        .context_files
        .iter()
        .filter(|c| c.path.starts_with(env.root()))
        .map(|c| c.content.as_str())
        .collect();
    assert_eq!(
        context,
        vec!["COMMON PARENT CONTEXT", "WORK CONTEXT"],
        "{context:?}"
    );
    assert!(r.skills.iter().any(|s| s.name == "work-skill"));
}
