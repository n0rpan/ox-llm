use oxa_fake::TestEnv;
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn an_incomplete_tail_is_quarantined_without_corrupting_later_samples() {
    let env = TestEnv::new();
    let path = env.tmp().join("shadow.jsonl");
    fs::write(&path, b"{}\n{\"partial\":").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let mut log = oxa_work::shadow::open_log(&path).unwrap();
    oxa_work::shadow::run(
        &mut log,
        true,
        || Ok(oxa_work::board::Board::default()),
        |_| panic!("unexpected wait"),
    )
    .unwrap();
    let text = fs::read_to_string(path).unwrap();
    assert!(text
        .lines()
        .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok()));
    let fragment = fs::read_dir(env.tmp())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().contains("incomplete"))
        .unwrap();
    assert_eq!(fs::read(&fragment).unwrap(), b"{\"partial\":");
    assert_eq!(
        fs::metadata(fragment).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn sampling_appends_and_waits_five_minutes_and_stops_on_read_error() {
    let env = TestEnv::new();
    let path = env.tmp().join("shadow.jsonl");
    let mut log = oxa_work::shadow::open_log(&path).unwrap();
    assert!(oxa_work::shadow::open_log(&path).is_err());
    let mut reads = 0;
    let mut waits = vec![];
    assert!(oxa_work::shadow::run(
        &mut log,
        false,
        || {
            reads += 1;
            if reads == 3 {
                return Err("synthetic read failure".into());
            }
            Ok(oxa_work::board::Board {
                observed_at: reads * 300,
                entries: vec![],
            })
        },
        |duration| waits.push(duration)
    )
    .is_err());
    assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);
    assert_eq!(waits.len(), 2);
    assert!(waits
        .iter()
        .all(|d| *d <= oxa_work::shadow::INTERVAL && d.as_secs() >= 299));
    drop(log);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(oxa_work::shadow::open_log(&path).is_err());
}

#[test]
fn review_git_evidence_keeps_an_unchanged_main_merge_and_rejects_a_changed_patch() {
    use oxa_work::{board::review::GitDiff, RunPolicy};
    use std::process::Command;

    let env = TestEnv::new();
    let repo = env.work();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(&repo)
            .env_clear()
            .envs(env.envs("http://127.0.0.1:1"))
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-b", "main"]);
    env.write(
        "work/obsolete.rs",
        "const FIRST: u8 = 1;\nconst SECOND: u8 = 2;\n",
    );
    env.write("work/old_name.rs", "fn existing() {}\n");
    git(&["add", "obsolete.rs", "old_name.rs"]);
    git(&["commit", "-m", "initial"]);
    let old_main = git(&["rev-parse", "HEAD"]);
    git(&["switch", "-c", "review"]);
    env.write("work/protected.rs", "fn check() {}\n");
    git(&["add", "protected.rs"]);
    git(&["commit", "-m", "reviewed change"]);
    let reviewed = git(&["rev-parse", "HEAD"]);
    git(&["switch", "main"]);
    env.write("work/README.md", "new base\n");
    git(&["add", "README.md"]);
    git(&["commit", "-m", "unrelated main change"]);
    let main = git(&["rev-parse", "HEAD"]);
    git(&["switch", "review"]);
    git(&["merge", "--no-edit", "main"]);
    let merged = git(&["rev-parse", "HEAD"]);

    let host_path = std::env::var_os("PATH").unwrap();
    let git_binary = std::env::split_paths(&host_path)
        .map(|directory| directory.join("git"))
        .find(|path| path.is_absolute() && path.is_file())
        .unwrap();
    let policy = RunPolicy {
        git_binary,
        host_path,
        offline: true,
        gh_binary: None,
        push_approved: false,
        content_scan: None,
        runner_owns_github: false,
        autonomy: Default::default(),
        autonomy_now: None,
        terminate_binary: None,
        storage: env.tmp(),
    };
    let read = |base: &str, head: &str| {
        GitDiff::read(
            &policy,
            &repo,
            base,
            head,
            &[(oxa_work::board::review::Reviewer::Codex, &reviewed)],
            |_| false,
        )
        .unwrap()
        .unwrap()
    };
    let original = read(&old_main, &reviewed);
    let carried = read(&main, &merged);
    assert_eq!(carried.patch_id, original.patch_id);
    assert_eq!(
        carried.main_merge_predecessors.as_slice(),
        std::slice::from_ref(&reviewed)
    );

    env.write("work/protected.rs", "fn check() { }\n");
    git(&["add", "protected.rs"]);
    git(&["commit", "-m", "adjust protected change"]);
    let edited = git(&["rev-parse", "HEAD"]);
    let current = read(&main, &edited);
    assert_ne!(current.patch_id, carried.patch_id);
    assert!(current.main_merge_predecessors.is_empty());
    assert!(GitDiff::read(&policy, &repo, &main, "HEAD", &[], |_| false).is_err());

    git(&["mv", "old_name.rs", "renamed.md"]);
    git(&["rm", "obsolete.rs"]);
    git(&["commit", "-m", "rename code and remove a file"]);
    let changed = read(&main, &git(&["rev-parse", "HEAD"]));
    let renamed = changed
        .files
        .iter()
        .find(|file| file.path == "renamed.md")
        .unwrap();
    assert_eq!(renamed.previous_path.as_deref(), Some("old_name.rs"));
    assert!(!renamed.deleted_file);
    let removed = changed
        .files
        .iter()
        .find(|file| file.path == "obsolete.rs")
        .unwrap();
    assert!(removed.deleted_file);
    assert_eq!(removed.deleted_lines, 2);
}
