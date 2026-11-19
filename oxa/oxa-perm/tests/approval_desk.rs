//! #1512 S5: the approval desk (files, answers, grants) and the unattended answer of a dangerous
//! command in the policy. The classifier is tested in `danger_table.rs`.
use oxa_fake::TestEnv;
use oxa_perm::{ApprovalDesk, DangerHit, DangerRules, ModePolicy, UnattendedMode};
use oxa_proto::{Access, Decision, Mode, Policy};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const DANGEROUS: &str = "git push --force origin main";
const KEY: &str = "git-push-force git push --force origin main";

fn hit() -> DangerHit {
    DangerHit {
        rule: "git-push-force".into(),
        reason: "force".into(),
        segment: DANGEROUS.into(),
    }
}

fn exec(command: &str) -> Access {
    Access::Exec {
        command: command.to_string(),
        net: false,
    }
}

/// The desk under `<root>/agent`, and that agent directory.
fn open_desk(env: &TestEnv) -> (Arc<ApprovalDesk>, PathBuf) {
    let agent = env.root().join("agent");
    let desk = ApprovalDesk::open(&agent).expect("desk opens");
    (Arc::new(desk), agent)
}

fn policy(desk: &Arc<ApprovalDesk>, session: &str, mode: UnattendedMode, os_sandbox: bool) -> ModePolicy {
    ModePolicy::new()
        .with_danger(Arc::new(DangerRules::builtin()), Vec::new())
        .with_desk(desk.clone(), session.to_string(), mode, os_sandbox)
}

/// The request id named in a request text: `... (request <id>): ...`.
fn request_id(text: &str) -> String {
    text.split("(request ")
        .nth(1)
        .and_then(|rest| rest.get(..16))
        .unwrap_or_default()
        .to_string()
}

/// How many stored records of the desk carry `status`.
fn records_with(agent: &Path, status: &str) -> usize {
    let needle = format!("\"status\":\"{status}\"");
    std::fs::read_dir(agent.join("approvals"))
        .expect("desk directory")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .filter(|e| {
            std::fs::read_to_string(e.path())
                .unwrap_or_default()
                .contains(&needle)
        })
        .count()
}

#[test]
fn desk_files_are_private_and_nofollow() {
    let env = TestEnv::new();
    let (desk, agent) = open_desk(&env);
    let cwd = env.work();
    let id = desk
        .record("s1", &cwd, DANGEROUS, &[hit()], "pending")
        .expect("record");
    let dir = agent.join("approvals");
    assert_eq!(
        std::fs::metadata(&dir).expect("dir").permissions().mode() & 0o777,
        0o700
    );
    let file = dir.join(format!("{id}.json"));
    assert_eq!(
        std::fs::metadata(&file).expect("file").permissions().mode() & 0o777,
        0o600
    );
    // A symlink where a record was is never followed: the answer is refused and the target kept.
    let outside = env.root().join("outside.txt");
    std::fs::write(&outside, "keep").expect("write outside");
    std::fs::remove_file(&file).expect("remove record");
    std::os::unix::fs::symlink(&outside, &file).expect("symlink");
    assert!(desk.answer(&id, "once").is_err());
    assert_eq!(std::fs::read_to_string(&outside).expect("read outside"), "keep");
    assert!(desk.pending().expect("pending").is_empty());
}

#[test]
fn desk_record_dedups_same_session_and_keys() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    let a = desk
        .record("s1", &cwd, DANGEROUS, &[hit()], "pending")
        .expect("first");
    let b = desk
        .record("s1", &cwd, DANGEROUS, &[hit()], "pending")
        .expect("second");
    assert_eq!(a, b, "the same session and keys reuse the open request");
    let c = desk
        .record("s2", &cwd, DANGEROUS, &[hit()], "pending")
        .expect("other session");
    assert_ne!(a, c);
    assert_eq!(desk.pending().expect("pending").len(), 2);
}

#[test]
fn desk_answer_only_from_pending() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    let refused = desk
        .record("s1", &cwd, DANGEROUS, &[hit()], "refused")
        .expect("refused");
    assert!(
        desk.answer(&refused, "once").is_err(),
        "a refused record cannot be answered"
    );
    let id = desk
        .record("s1", &cwd, DANGEROUS, &[hit()], "pending")
        .expect("pending");
    assert!(desk.answer(&id, "maybe").is_err(), "only the four answers");
    let answered = desk.answer(&id, "once").expect("answer");
    assert_eq!(answered.status, "once");
    assert!(
        desk.answer(&id, "deny").is_err(),
        "an answered record is not pending any more"
    );
}

#[test]
fn desk_once_grant_is_consumed() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    let keys = vec![KEY.to_string()];
    let id = desk
        .record("s1", &cwd, DANGEROUS, &[hit()], "pending")
        .expect("pending");
    desk.answer(&id, "once").expect("answer");
    assert!(desk.granted("s1", &cwd, &keys), "the once grant covers the retry");
    assert!(!desk.granted("s1", &cwd, &keys), "and is spent by it");
}

/// A probe must not spend the owner's single approval: `covers` only looks, `consume` spends it
/// once the command is going to run.
#[test]
fn desk_covers_does_not_spend_a_once_grant() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    let keys = vec![KEY.to_string()];
    let id = desk
        .record("s1", &cwd, DANGEROUS, &[hit()], "pending")
        .expect("pending");
    desk.answer(&id, "once").expect("answer");
    assert!(desk.covers("s1", &cwd, &keys));
    assert!(desk.covers("s1", &cwd, &keys), "a probe leaves it available");
    assert!(desk.consume("s1", &cwd, &keys), "the final decision spends it");
    assert!(!desk.covers("s1", &cwd, &keys));
    assert!(!desk.consume("s1", &cwd, &keys), "and never twice");
}

#[test]
fn desk_session_grant_is_scoped_to_session() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    let keys = vec![KEY.to_string()];
    let id = desk
        .record("s1", &cwd, DANGEROUS, &[hit()], "pending")
        .expect("pending");
    desk.answer(&id, "session").expect("answer");
    assert!(!desk.granted("s2", &cwd, &keys), "another session is not covered");
    assert!(desk.granted("s1", &cwd, &keys));
    assert!(desk.granted("s1", &cwd, &keys), "a session grant stays");
}

/// A reusable grant is bound to the command line the owner reviewed. Approving a compound
/// command must not authorize its bare segment, and a target the shell computes at run time is
/// never covered by a reusable grant at all.
/// A wipe removes one session's records, or all of them, and leaves no desk behind when there
/// was none.
#[test]
fn desk_wipe_removes_one_session_or_all_and_creates_nothing() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    assert_eq!(ApprovalDesk::wipe(&agent, Some("s1")).expect("wipe"), 0);
    assert!(!agent.join("approvals").exists(), "wipe created the desk");
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    let one = desk
        .record("s1", &cwd, DANGEROUS, &[hit()], "pending")
        .expect("pending");
    let two = desk
        .record("s2", &cwd, DANGEROUS, &[hit()], "session")
        .expect("session");
    assert_eq!(ApprovalDesk::wipe(&agent, Some("s1")).expect("wipe"), 1);
    let left = desk.records().expect("records");
    assert!(!left.iter().any(|r| r.id == one));
    assert!(left.iter().any(|r| r.id == two));
    assert_eq!(ApprovalDesk::wipe(&agent, None).expect("wipe all"), 1);
    assert!(desk.records().expect("records").is_empty());
}

#[test]
fn desk_grants_cover_only_the_reviewed_command_line() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    let mut p = policy(&desk, "s1", UnattendedMode::Request, false);

    // The standalone push, approved for the session.
    let own = exec(DANGEROUS);
    assert_eq!(p.decide(Mode::Workspace, &own, &cwd), Decision::Ask);
    let text = p.danger_unattended(&own, &cwd).expect("request");
    desk.answer(&request_id(&text), "session")
        .expect("owner answers session");
    assert_eq!(p.decide(Mode::Workspace, &own, &cwd), Decision::Allow);

    // A compound command that merely contains the same segment is another command line.
    let compound = exec(&format!("true || {DANGEROUS}"));
    assert_eq!(
        p.decide(Mode::Workspace, &compound, &cwd),
        Decision::Ask,
        "a session grant never covers another command line"
    );
    let text = p.danger_unattended(&compound, &cwd).expect("asks");
    desk.answer(&request_id(&text), "session")
        .expect("owner answers session");
    // Its own line stays covered; the standalone one is untouched by the new approval.
    assert_eq!(p.decide(Mode::Workspace, &compound, &cwd), Decision::Allow);
    assert_eq!(p.decide(Mode::Workspace, &own, &cwd), Decision::Allow);

    // A target the shell computes at run time can name another path next time: it asks again.
    let computed = exec(r#"rm -rf "$(cat target.txt)""#);
    assert_eq!(p.decide(Mode::Workspace, &computed, &cwd), Decision::Ask);
    let text = p.danger_unattended(&computed, &cwd).expect("asks");
    desk.answer(&request_id(&text), "session")
        .expect("owner answers session");
    assert_eq!(
        p.decide(Mode::Workspace, &computed, &cwd),
        Decision::Ask,
        "a computed segment is never covered by a reusable grant"
    );
}

/// Two processes can create the desk at the same instant: the second must keep working with the
/// directory the first made instead of disabling approval requests for its whole process.
#[test]
fn desk_open_survives_a_concurrent_create() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    let desk = ApprovalDesk::open(&agent).expect("first open");
    let dir = agent.join("approvals");
    // The loser of the create sees `AlreadyExists` after its `NotFound` probe: replay that.
    std::fs::remove_dir_all(&dir).expect("remove");
    std::thread::scope(|scope| {
        let a = &agent;
        scope.spawn(move || {
            for _ in 0..8 {
                let _ = ApprovalDesk::open(a);
            }
        });
        for _ in 0..8 {
            ApprovalDesk::open(&agent).expect("a concurrent create is not an error");
        }
    });
    drop(desk);
    ApprovalDesk::open(&agent).expect("the desk still opens");
}

#[test]
fn desk_symlinked_dir_is_refused() {
    let env = TestEnv::new();
    let agent = env.root().join("agent");
    let elsewhere = env.root().join("elsewhere");
    std::fs::create_dir_all(&agent).expect("agent");
    std::fs::create_dir_all(&elsewhere).expect("elsewhere");
    std::os::unix::fs::symlink(&elsewhere, agent.join("approvals")).expect("symlink");
    assert!(ApprovalDesk::open(&agent).is_err());
}

#[test]
fn unattended_auto_with_os_sandbox_refuses_and_records() {
    let env = TestEnv::new();
    let (desk, agent) = open_desk(&env);
    let cwd = env.work();
    let mut p = policy(&desk, "s1", UnattendedMode::Auto, true);
    assert_eq!(p.decide(Mode::Workspace, &exec(DANGEROUS), &cwd), Decision::Ask);
    let text = p
        .danger_unattended(&exec(DANGEROUS), &cwd)
        .expect("a refusal text");
    assert!(
        text.starts_with("Refused: dangerous command (git-push-force: "),
        "{text}"
    );
    assert!(
        text.contains("Unattended runs with the OS sandbox refuse dangerous commands"),
        "{text}"
    );
    assert!(
        desk.pending().expect("pending").is_empty(),
        "a refusal is not a request"
    );
    assert_eq!(records_with(&agent, "refused"), 1);
}

#[test]
fn unattended_auto_without_os_sandbox_requests() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    let mut p = policy(&desk, "s1", UnattendedMode::Auto, false);
    let text = p
        .danger_unattended(&exec(DANGEROUS), &cwd)
        .expect("a request text");
    assert!(text.starts_with("Needs owner approval (request "), "{text}");
    assert!(text.contains("oxa send s1 --approve "), "{text}");
    let waiting = desk.pending().expect("pending");
    assert_eq!(waiting.len(), 1);
    assert_eq!(request_id(&text), waiting[0].id);
}

#[test]
fn unattended_refuse_setting_overrides_auto() {
    let env = TestEnv::new();
    let (desk, agent) = open_desk(&env);
    let cwd = env.work();
    // Without the OS sandbox `auto` would request; `refuse` refuses.
    let mut p = policy(&desk, "s1", UnattendedMode::Refuse, false);
    let text = p.danger_unattended(&exec(DANGEROUS), &cwd).expect("refusal");
    assert!(text.starts_with("Refused: dangerous command ("), "{text}");
    // Honest text: this run has no OS sandbox, so the refusal names the owner setting instead.
    assert!(!text.contains("OS sandbox"), "{text}");
    assert!(text.contains("oxa.approval.unattended"), "{text}");
    assert!(desk.pending().expect("pending").is_empty());
    assert_eq!(records_with(&agent, "refused"), 1);
}

#[test]
fn unattended_request_setting_overrides_auto() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    // With the OS sandbox `auto` would refuse; `request` files a request.
    let mut p = policy(&desk, "s1", UnattendedMode::Request, true);
    let text = p.danger_unattended(&exec(DANGEROUS), &cwd).expect("request");
    assert!(text.starts_with("Needs owner approval (request "), "{text}");
    assert_eq!(desk.pending().expect("pending").len(), 1);
}

#[test]
fn retry_after_once_approval_runs_then_asks_again() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    let mut p = policy(&desk, "s1", UnattendedMode::Request, false);
    let cmd = exec(DANGEROUS);
    assert_eq!(p.decide(Mode::Workspace, &cmd, &cwd), Decision::Ask);
    let first = p.danger_unattended(&cmd, &cwd).expect("request");
    let id = request_id(&first);
    desk.answer(&id, "once").expect("owner answers once");
    // The retry runs: the once grant covers it and is spent.
    assert_eq!(p.decide(Mode::Workspace, &cmd, &cwd), Decision::Allow);
    // The next attempt asks again.
    assert_eq!(p.decide(Mode::Workspace, &cmd, &cwd), Decision::Ask);
    let second = p.danger_unattended(&cmd, &cwd).expect("asks again");
    assert!(second.starts_with("Needs owner approval (request "), "{second}");
    assert_ne!(request_id(&second), id);
}

/// A grant is bound to the directory the owner reviewed: the same text in another directory is
/// a different command (`rm -rf ..` from two places deletes two different parents).
#[test]
fn desk_grants_are_bound_to_the_reviewed_directory() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let here = env.work();
    let there = env.work().join("sub");
    std::fs::create_dir_all(&there).expect("sub dir");
    let keys = vec![KEY.to_string()];
    let id = desk
        .record("s1", &here, DANGEROUS, &[hit()], "pending")
        .expect("record");
    desk.answer(&id, "session").expect("answer");
    assert!(
        desk.covers("s1", &here, &keys),
        "the reviewed directory is covered"
    );
    assert!(!desk.covers("s1", &there, &keys), "another directory is not");
    // A second directory files (and dedups) its own request.
    let first = desk
        .record("s1", &there, DANGEROUS, &[hit()], "pending")
        .expect("record");
    let again = desk
        .record("s1", &there, DANGEROUS, &[hit()], "pending")
        .expect("record");
    assert_eq!(first, again, "the same directory reuses its open request");
    let other = desk
        .record("s1", &here, DANGEROUS, &[hit()], "pending")
        .expect("record");
    assert_ne!(first, other, "another directory files its own request");
}

/// Answering and consuming are read-modify-write transitions on one file. Two owner processes
/// answering the same request, or an answer racing the retry that spends it, must not resurrect
/// a spent `once` grant: exactly one answer can win, and a spent record stays spent.
#[test]
fn desk_answers_and_consumption_are_serialised() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    let keys = vec![KEY.to_string()];
    let id = desk
        .record("s1", &cwd, DANGEROUS, &[hit()], "pending")
        .expect("pending");
    let answers = 8;
    let wins: usize = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..answers)
            .map(|_| {
                let desk = desk.clone();
                let id = id.clone();
                let cwd = cwd.clone();
                let keys = keys.clone();
                scope.spawn(move || {
                    let answered = desk.answer(&id, "once").is_ok();
                    // The retry spends the grant, if this thread's answer is the one that won.
                    if answered {
                        desk.consume("s1", &cwd, &keys);
                    }
                    answered
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or(false))
            .filter(|won| *won)
            .count()
    });
    assert_eq!(wins, 1, "one answer wins, the rest see an answered request");
    assert!(
        !desk.covers("s1", &cwd, &keys),
        "the spent grant was not resurrected"
    );
    let rec = desk
        .records()
        .expect("records")
        .into_iter()
        .find(|r| r.id == id)
        .expect("the record");
    assert!(rec.used, "the record still says used");
    assert_eq!(rec.status, "once");
}

/// A probe that only inspects a covered request must not spend the owner's `once` approval: the
/// retry still runs, and the final decision is what spends it.
#[test]
fn a_probe_does_not_spend_a_once_approval() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    let mut p = policy(&desk, "s1", UnattendedMode::Request, false);
    let cmd = exec(DANGEROUS);
    let first = p.danger_unattended(&cmd, &cwd).expect("request");
    desk.answer(&request_id(&first), "once")
        .expect("owner answers once");
    assert!(
        p.danger_ask(&cmd, &cwd).is_empty(),
        "the once grant covers the retry"
    );
    assert_eq!(
        p.decide(Mode::Workspace, &cmd, &cwd),
        Decision::Allow,
        "the retry still runs after the probe"
    );
    assert_eq!(
        p.decide(Mode::Workspace, &cmd, &cwd),
        Decision::Ask,
        "then asks again"
    );
}

/// In a headless `Mode::Ask` run the table still says `Ask` for the retry, so the once grant has
/// to survive the verdict and the `danger_ask` that follows it: otherwise each `--approve ... once`
/// is spent by a probe and the same command is requested forever.
#[test]
fn a_once_approval_survives_a_headless_ask_retry() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    let mut p = policy(&desk, "s1", UnattendedMode::Request, false);
    let cmd = exec(DANGEROUS);
    let first = p.danger_unattended(&cmd, &cwd).expect("request");
    desk.answer(&request_id(&first), "once")
        .expect("owner answers once");
    // The table stays `Ask` in `Mode::Ask`: the retry is not allowed, so nothing may be spent.
    assert_eq!(p.decide(Mode::Ask, &cmd, &cwd), Decision::Ask);
    assert!(
        p.danger_ask(&cmd, &cwd).is_empty(),
        "the approved command runs instead of asking again"
    );
    assert!(
        !desk.covers("s1", &cwd, &[KEY.to_string()]),
        "and the final allow spent it"
    );
}

/// The CLI accepts `--offline` before `send`, so the self-approve rule must look past leading
/// switches: `oxa --offline send <s> --approve <id> once` is the same forged answer.
#[test]
fn oxa_self_approve_rule_sees_send_after_leading_switches() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    let mut p = policy(&desk, "s1", UnattendedMode::Request, false);
    for forged in [
        "oxa --offline send s1 --approve 0123456789abcdef once",
        "oxa --offline --offline send s1 --approve 0123456789abcdef session",
        "/usr/local/bin/oxa --offline send --approvals",
    ] {
        assert_eq!(
            p.decide(Mode::Workspace, &exec(forged), &cwd),
            Decision::Ask,
            "{forged}"
        );
        let text = p.danger_unattended(&exec(forged), &cwd).expect(forged);
        assert!(text.contains("oxa-self-approve"), "{forged}: {text}");
    }
}

#[test]
fn oxa_self_approve_rule_blocks_model_running_oxa_send_approve() {
    let env = TestEnv::new();
    let (desk, _) = open_desk(&env);
    let cwd = env.work();
    // Unattended request mode: the model tries to approve its own request.
    let mut p = policy(&desk, "s1", UnattendedMode::Request, true);
    let forged = exec("oxa send s1 --approve 0123456789abcdef once");
    assert_eq!(p.decide(Mode::Workspace, &forged, &cwd), Decision::Ask);
    let text = p.danger_unattended(&forged, &cwd).expect("request text");
    assert!(text.contains("oxa-self-approve"), "{text}");
    assert!(text.starts_with("Needs owner approval (request "), "{text}");
    // Nothing was granted by the attempt.
    assert!(!desk.granted(
        "s1",
        &cwd,
        &["oxa-self-approve oxa send s1 --approve 0123456789abcdef once".to_string()]
    ));
}

#[test]
fn no_desk_keeps_the_ui_path() {
    let env = TestEnv::new();
    let cwd = env.work();
    let mut p = ModePolicy::new().with_danger(Arc::new(DangerRules::builtin()), Vec::new());
    assert_eq!(p.decide(Mode::Workspace, &exec(DANGEROUS), &cwd), Decision::Ask);
    assert!(p.danger_unattended(&exec(DANGEROUS), &cwd).is_none());
}
