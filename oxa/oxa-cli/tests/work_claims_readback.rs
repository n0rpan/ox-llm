use oxa_fake::TestEnv;
use oxa_work::board::{assignment::*, *};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn private(path: &Path, directory: bool) {
    if directory {
        fs::create_dir(path).unwrap();
    }
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
    )
    .unwrap();
}

fn invoke(env: &TestEnv, config: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_oxa"))
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .args(["--offline", "work", "board", "--claims-readback", "--config"])
        .arg(config)
        .output()
        .unwrap()
}

fn fixture() -> (TestEnv, std::path::PathBuf, Value) {
    let env = TestEnv::new();
    assert!(Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "init", "-b", "main"])
        .current_dir(env.work())
        .output()
        .unwrap()
        .status
        .success());
    oxa_perm::TrustStore::default()
        .save(&env.agent_dir().join("trust.json"), &env.work(), true)
        .unwrap();
    let controls = env.agent_dir().join("board-controls");
    private(&controls, true);
    let claims = controls.join("claims");
    private(&claims, true);
    let pause = controls.join("paused");
    fs::write(&pause, b"owner paused").unwrap();
    let config = controls.join("refill.json");
    let value = json!({
        "enabled":false,"claims":claims,"queue":controls.join("uncreated-queue"),"pause":pause,
        "worker":{"id":"worker-a","kind":"oxa","roles":["builder"],"skills":[]},
        "host_id":"host-a","partition":{"modulus":22,"residues":(0..12).collect::<Vec<_>>()}
    });
    write_config(&config, &value);
    (env, config, value)
}

fn write_config(path: &Path, value: &Value) {
    fs::write(path, value.to_string()).unwrap();
    private(path, false);
}

fn files(path: &Path) -> BTreeMap<std::ffi::OsString, Vec<u8>> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), fs::read(entry.path()).unwrap())
        })
        .collect()
}

#[test]
fn same_store_readback_preserves_full_claims_and_unknown_legacy_coverage_while_paused() {
    let (env, config, mut value) = fixture();
    let root = Path::new(value["claims"].as_str().unwrap());
    let claims = Claims::open(root).unwrap();
    let worker: Worker = serde_json::from_value(value["worker"].clone()).unwrap();
    let entry = Entry {
        number: 22,
        head: "a".repeat(40),
        release_pinned: false,
        action: Action::FixRequiredCi,
        lease: None,
        attempts_today: 0,
        eligible: true,
        evidence_incomplete: None,
    };
    let receipt = claims
        .claim_item_for(
            &entry,
            &Policy {
                enabled: true,
                ..Policy::default()
            },
            &worker,
            "host-a",
            (10, 60),
            |_| Ok(None),
        )
        .unwrap()
        .unwrap();
    let binding = NativeBinding::new_reviewed(
        22,
        &"b".repeat(64),
        &receipt.lease,
        "agent/synthetic",
        &entry.head,
    )
    .unwrap();
    assert!(claims.bind_native(&worker.id, &binding).unwrap());
    claims
        .import_snapshot(
            Some(ManualAuthority::Owner),
            &[34, 44]
                .into_iter()
                .map(|number| {
                    (
                        number,
                        ImportState {
                            state: AttemptState {
                                attempts: 7,
                                ..AttemptState::default()
                            },
                            excluded: true,
                        },
                    )
                })
                .collect(),
            10,
        )
        .unwrap();
    let expected = serde_json::to_value(claims.assignments().unwrap()).unwrap();
    let marker = root.join("22.board-claim.created");
    fs::remove_file(&marker).unwrap();
    let before = files(root);
    let output = invoke(&env, &config);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let readback: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(readback["assignments"], expected);
    assert_eq!(readback["worker"], value["worker"]);
    assert_eq!(readback["host_id"], "host-a");
    assert_eq!(readback["partition"], value["partition"]);
    assert_eq!(readback["source"], "Claims::assignments_read_only");
    assert_eq!(readback["partition_source"], "protected-owner-configuration");
    assert_eq!(
        readback["partition_membership"],
        json!({"22":true,"34":false,"44":true})
    );
    assert_eq!(readback["legacy_coverage"], "unknown");
    assert_eq!(readback["refill_enabled"], false);
    assert!(!marker.exists(), "readback must not repair a missing marker");
    assert_eq!(files(root), before);
    assert!(!Path::new(value["queue"].as_str().unwrap()).exists());
    assert_eq!(
        fs::read(value["pause"].as_str().unwrap()).unwrap(),
        b"owner paused"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(root.to_str().unwrap()));
    claims.assignments().unwrap();
    assert!(marker.exists(), "normal Claims queries retain witness repair");
    fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
    let preserved = files(root);
    assert_eq!(invoke(&env, &config).status.code(), Some(2));
    assert_eq!(files(root), preserved);
    private(&marker, false);
    fs::remove_file(root.join("22.board-claim.json")).unwrap();
    let preserved = files(root);
    assert_eq!(invoke(&env, &config).status.code(), Some(2));
    assert_eq!(files(root), preserved);
    let other = config.parent().unwrap().join("other-claims");
    private(&other, true);
    value["claims"] = json!(other);
    write_config(&config, &value);
    let output = invoke(&env, &config);
    assert!(output.status.success());
    let readback: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(readback["assignments"], json!({}));
    assert_eq!(readback["legacy_coverage"], "unknown");
    assert!(files(&other).is_empty());
}

#[test]
fn readback_refuses_project_configuration_and_invalid_partition_without_writing() {
    let (env, config, mut value) = fixture();
    let project = env.work().join("project-config.json");
    write_config(&project, &value);
    assert_eq!(invoke(&env, &project).status.code(), Some(2));
    for partition in [
        json!({"modulus":0,"residues":[0]}),
        json!({"modulus":22,"residues":[]}),
        json!({"modulus":22,"residues":[22]}),
    ] {
        value["partition"] = partition;
        write_config(&config, &value);
        assert_eq!(invoke(&env, &config).status.code(), Some(2));
    }
    assert!(files(Path::new(value["claims"].as_str().unwrap())).is_empty());
    assert!(!Path::new(value["queue"].as_str().unwrap()).exists());
}
