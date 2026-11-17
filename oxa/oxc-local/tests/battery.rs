//! N9 fixture contract: synthetic power-supply trees and CPU-mask responses.
//!
//! Every fixture is a directory tree under the test's private `TestEnv`; nothing reads the
//! host's real battery, writes sysfs or measures energy.
use oxa_fake::TestEnv;
use oxc_local::battery::{format_cpus, parse_cpus, read_allowed_mask, read_power, Power};

/// One synthetic power-supply tree: relative paths and their file contents.
type Tree = &'static [(&'static str, &'static str)];

fn tree(env: &TestEnv, entries: &[(&str, &str)]) {
    for (path, body) in entries {
        env.write(&format!("power_supply/{path}"), body);
    }
}

#[test]
fn mains_and_system_battery_states_map_to_the_read_only_power_state() {
    let cases: &[(&str, Tree, Power)] = &[
        ("ac", &[("AC0/type", "Mains\n"), ("AC0/online", "1\n")], Power::Ac),
        (
            "discharging",
            &[
                ("AC0/type", "Mains\n"),
                ("AC0/online", "0\n"),
                ("BAT0/type", "Battery\n"),
                ("BAT0/status", "Discharging\n"),
            ],
            Power::Discharging,
        ),
        (
            "charging",
            &[
                ("AC0/type", "Mains\n"),
                ("AC0/online", "1\n"),
                ("BAT0/type", "Battery\n"),
                ("BAT0/status", "Charging\n"),
            ],
            Power::Charging,
        ),
        (
            "full",
            &[
                ("AC0/type", "Mains\n"),
                ("AC0/online", "1\n"),
                ("BAT0/type", "Battery\n"),
                ("BAT0/status", "Full\n"),
            ],
            Power::Ac,
        ),
        (
            "not-charging",
            &[("BAT0/type", "Battery\n"), ("BAT0/status", "Not charging\n")],
            Power::Ac,
        ),
        // A peripheral battery discharging does not power the machine.
        (
            "peripheral",
            &[
                ("AC0/type", "Mains\n"),
                ("AC0/online", "1\n"),
                ("hidpp_battery_0/type", "Battery\n"),
                ("hidpp_battery_0/scope", "Device\n"),
                ("hidpp_battery_0/status", "Discharging\n"),
            ],
            Power::Ac,
        ),
        (
            "peripheral-only",
            &[
                ("hidpp_battery_0/type", "Battery\n"),
                ("hidpp_battery_0/scope", "Device\n"),
                ("hidpp_battery_0/status", "Discharging\n"),
            ],
            Power::Unknown,
        ),
    ];
    for &(name, entries, want) in cases {
        let env = TestEnv::new();
        tree(&env, entries);
        assert_eq!(read_power(&env.work().join("power_supply")), want, "{name}");
    }
}

#[test]
fn missing_empty_unreadable_and_malformed_sources_are_unknown() {
    let env = TestEnv::new();
    assert_eq!(read_power(&env.work().join("absent")), Power::Unknown);
    std::fs::create_dir_all(env.work().join("empty")).unwrap();
    assert_eq!(read_power(&env.work().join("empty")), Power::Unknown);

    let malformed: &[(&str, Tree)] = &[
        // Not a valid device class.
        ("bad-type", &[("BAT0/type", "battery\n")]),
        // A driver value the policy does not know.
        (
            "bad-status",
            &[("BAT0/type", "Battery\n"), ("BAT0/status", "Calibrating\n")],
        ),
        // A system battery without a readable status.
        ("no-status", &[("BAT0/type", "Battery\n")]),
        // An empty status file.
        (
            "empty-status",
            &[("BAT0/type", "Battery\n"), ("BAT0/status", "\n")],
        ),
        // An AC source whose online value is not 0/1.
        ("bad-online", &[("AC0/type", "Mains\n"), ("AC0/online", "yes\n")]),
        // A battery whose scope cannot be classified is not assumed to be the system battery.
        (
            "bad-scope",
            &[
                ("BAT0/type", "Battery\n"),
                ("BAT0/scope", "Peripheral\n"),
                ("BAT0/status", "Discharging\n"),
            ],
        ),
    ];
    for &(name, entries) in malformed {
        let env = TestEnv::new();
        tree(&env, entries);
        assert_eq!(
            read_power(&env.work().join("power_supply")),
            Power::Unknown,
            "{name}"
        );
    }

    // An entry that is not a device directory (reading `<entry>/type` fails) is unreadable.
    let env = TestEnv::new();
    env.write("power_supply/not-a-device", "Battery\n");
    assert_eq!(read_power(&env.work().join("power_supply")), Power::Unknown);

    // A permission-denied tree is unreadable: on a non-root runner the mode removes the
    // directory, and the unprivileged fallback (no readable type) is Unknown either way.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let env = TestEnv::new();
        env.write("power_supply/BAT0/type", "Battery\n");
        let dir = env.work().join("power_supply/BAT0");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(read_power(&env.work().join("power_supply")), Power::Unknown);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[test]
fn the_cpu_mask_comes_from_the_injected_status_file() {
    let env = TestEnv::new();
    let status = env.write(
        "proc/status",
        "Name:\tox\nVmPeak:\t  123 kB\nCpus_allowed_list:\t0-3,8\n",
    );
    assert_eq!(read_allowed_mask(&status), Some(vec![0, 1, 2, 3, 8]));
    for text in [
        "Name:\tox\n",
        "Cpus_allowed_list:\t\n",
        "Cpus_allowed_list:\t0,0\n",
        "Cpus_allowed_list:\t3-1\n",
    ] {
        let status = env.write("proc/other", text);
        assert_eq!(read_allowed_mask(&status), None, "{text:?}");
    }
    assert_eq!(read_allowed_mask(&env.work().join("absent")), None);
}

#[test]
fn the_emitted_team_stays_inside_the_engine_cpu_list_grammar() {
    // `parse_cpus` mirrors engine/crates/sched's documented `--oxe-cpus` grammar; the
    // formatter must always round-trip through it.
    for cpus in [vec![0], vec![0, 1], vec![3, 5, 7], vec![0, 1, 2, 3, 4, 5, 6, 7]] {
        assert_eq!(parse_cpus(&format_cpus(&cpus)), Some(cpus.clone()), "{cpus:?}");
    }
}
