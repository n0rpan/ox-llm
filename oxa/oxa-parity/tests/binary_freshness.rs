//! Child binary lookup refreshes bare Cargo runs, but respects prebuilt test artifacts.
#![cfg(unix)]

use oxa_fake::TestEnv;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn bare_cargo_test_refreshes_existing_child_binaries() {
    const PROBE: &str = "OXA_BINARY_FRESHNESS_PROBE";
    if let Ok(expected) = std::env::var(PROBE) {
        assert_eq!(
            std::fs::read_to_string(oxa_parity::oxa_bin()).unwrap(),
            format!("{expected} oxa")
        );
        assert_eq!(
            std::fs::read_to_string(oxa_parity::fake_bin()).unwrap(),
            format!("{expected} fake")
        );
        return;
    }

    for profile_name in ["debug", "ci", "release"] {
        for mode in ["bare", "nextest", "overrides"] {
            let env = TestEnv::new();
            // Copy this harness so sibling lookup uses isolated binaries. The Cargo stand-in
            // records the build request; it never builds or modifies the real workspace.
            let profile = env.root().join("target").join(profile_name);
            let deps = profile.join("deps");
            std::fs::create_dir_all(&deps).unwrap();
            let probe = deps.join("freshness-probe");
            std::fs::copy(std::env::current_exe().unwrap(), &probe).unwrap();
            std::fs::write(profile.join("oxa"), "stale oxa").unwrap();
            std::fs::write(profile.join("oxa-fake"), "stale fake").unwrap();
            let cargo = env.root().join("cargo");
            std::fs::write(
                &cargo,
                "#!/bin/sh\nset -eu\nprintf 'fresh oxa' > \"$PROBE_PROFILE_DIR/oxa\"\nprintf 'fresh fake' > \"$PROBE_PROFILE_DIR/oxa-fake\"\nprintf '%s\\n' \"$*\" >> \"$PROBE_PROFILE_DIR/builds\"\n",
            )
            .unwrap();
            std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut cmd = Command::new(probe);
            cmd.args([
                "--exact",
                "bare_cargo_test_refreshes_existing_child_binaries",
                "--nocapture",
            ])
            .env_clear()
            .env("HOME", env.home())
            .env("CARGO", cargo)
            .env("PROBE_PROFILE_DIR", &profile);
            match mode {
                "bare" => {
                    cmd.env(PROBE, "fresh");
                }
                "nextest" => {
                    cmd.env(PROBE, "stale").env("NEXTEST", "1");
                }
                "overrides" => {
                    let oxa = env.root().join("override-oxa");
                    let fake = env.root().join("override-fake");
                    std::fs::write(&oxa, "override oxa").unwrap();
                    std::fs::write(&fake, "override fake").unwrap();
                    cmd.env(PROBE, "override")
                        .env("OXA_BIN", oxa)
                        .env("OXA_FAKE_BIN", fake);
                }
                _ => unreachable!(),
            }
            let output = cmd.output().unwrap();
            assert!(
                output.status.success(),
                "{profile_name}/{mode} probe failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let builds = profile.join("builds");
            if mode == "bare" {
                let profile_arg = if profile_name == "debug" {
                    String::new()
                } else {
                    format!(" --profile {profile_name}")
                };
                assert_eq!(
                    std::fs::read_to_string(builds).unwrap(),
                    format!("build -p oxa-cli -p oxa-fake{profile_arg}\n"),
                    "both sibling lookups share one Cargo freshness check"
                );
            } else {
                assert!(!builds.exists(), "{mode} must use its prebuilt binaries");
            }
        }
    }
}
