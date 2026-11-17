// No engine process or filesystem fixture is needed.
use oxc_local::launch_profile::{from_user_settings, Error, InstalledLayout, InstalledModel};
use serde_json::json;
use std::collections::BTreeMap;
use std::ffi::OsString;

fn layout() -> InstalledLayout {
    InstalledLayout {
        binary: "/opt/example/bin/oxe-server".into(),
        models: BTreeMap::from([(
            "local-model".into(),
            InstalledModel {
                model: "/opt/example/models/model.gguf".into(),
                arguments: vec!["--ctx-size".into(), "8192".into()],
            },
        )]),
        environment: BTreeMap::from([
            ("LANG".into(), "C.UTF-8".into()),
            ("GGML_OLD_TUNING".into(), "discard".into()),
            ("LLAMA_OLD_TUNING".into(), "discard".into()),
        ]),
    }
}

#[test]
fn installed_layout_needs_no_profile_or_filesystem_probe() {
    let p = from_user_settings(&json!({}), &layout(), "local-model", 8080).unwrap();
    assert_eq!(p.binary.to_str(), Some("/opt/example/bin/oxe-server"));
    assert_eq!(
        p.arguments,
        [
            "--host",
            "127.0.0.1",
            "--port",
            "8080",
            "--model",
            "/opt/example/models/model.gguf",
            "--alias",
            "local-model",
            "--ctx-size",
            "8192"
        ]
        .map(OsString::from)
    );
    assert_eq!(p.environment, BTreeMap::from([("LANG".into(), "C.UTF-8".into())]));
}

#[test]
fn user_paths_and_arguments_are_literal_and_replace_installed_tuning() {
    let settings = json!({"oxa":{"local":{"models":{"local-model":{
        "binary":"/other path/oxe-server", "model":"/other path/$(touch marker).gguf",
        "arguments":["--ctx-size","4096","--label","literal; $(false)"],
        "environment":{"LANG":"C","GGML_EXPLICIT":"yes","LITERAL":"$(false)"}
    }}}}});
    let p = from_user_settings(&settings, &layout(), "local-model", 9123).unwrap();
    assert_eq!(p.model.to_str(), Some("/other path/$(touch marker).gguf"));
    assert_eq!(
        &p.arguments[8..],
        ["--ctx-size", "4096", "--label", "literal; $(false)"].map(OsString::from)
    );
    assert_eq!(p.environment["LANG"], "C");
    assert_eq!(p.environment["GGML_EXPLICIT"], "yes");
    assert_eq!(p.environment["LITERAL"], "$(false)");
    let debug = format!("{p:?}");
    assert!(!debug.contains("other path"));
    assert!(!debug.contains("false"));
}

#[test]
fn malformed_settings_never_fall_back_to_a_different_profile() {
    for settings in [
        json!(null),
        json!({"oxa":false}),
        json!({"oxa":{"local":[]}}),
        json!({"oxa":{"local":{"models":{"local-model":null}}}}),
        json!({"oxa":{"local":{"models":{"local-model":{"argumnts":[]}}}}}),
    ] {
        assert_eq!(
            from_user_settings(&settings, &layout(), "local-model", 8080),
            Err(Error::Settings)
        );
    }
    assert_eq!(
        from_user_settings(&json!({}), &layout(), "unknown", 8080),
        Err(Error::UnknownModel)
    );
    assert_eq!(
        from_user_settings(&json!({}), &layout(), "local-model", 0),
        Err(Error::Port)
    );
}

#[test]
fn invalid_paths_and_interpreters_are_refused_without_opening_them() {
    for binary in [
        "relative",
        "/opt/../bin/oxe-server",
        "/usr/bin/python3",
        "/usr/bin/sh",
        "/opt/engine.py",
    ] {
        let settings = json!({"oxa":{"local":{"models":{"local-model":{"binary":binary}}}}});
        assert_eq!(
            from_user_settings(&settings, &layout(), "local-model", 8080),
            Err(Error::Binary)
        );
    }
    let mut installed = layout();
    installed.models.get_mut("local-model").unwrap().model = "relative.gguf".into();
    assert_eq!(
        from_user_settings(&json!({}), &installed, "local-model", 8080),
        Err(Error::Model)
    );
}

#[test]
fn profile_cannot_override_binding_model_or_alias_through_extra_arguments() {
    for flag in [
        "--host",
        "--host=0.0.0.0",
        "--port",
        "--port=42",
        "--listen",
        "--bind",
        "--model",
        "--model=other",
        "--alias",
        "-m",
        "-mother",
    ] {
        let settings = json!({"oxa":{"local":{"models":{"local-model":{"arguments":[flag]}}}}});
        assert_eq!(
            from_user_settings(&settings, &layout(), "local-model", 8080),
            Err(Error::OwnedArgument)
        );
    }
}

#[test]
fn argument_and_environment_types_are_checked() {
    for profile in [
        json!({"arguments":false}),
        json!({"arguments":[1]}),
        json!({"arguments":["a\u{0000}b"]}),
    ] {
        let settings = json!({"oxa":{"local":{"models":{"local-model":profile}}}});
        assert_eq!(
            from_user_settings(&settings, &layout(), "local-model", 8080),
            Err(Error::Arguments)
        );
    }
    for profile in [
        json!({"environment":[]}),
        json!({"environment":{"KEY":1}}),
        json!({"environment":{"":"value"}}),
        json!({"environment":{"A=B":"value"}}),
        json!({"environment":{"KEY":"a\u{0000}b"}}),
    ] {
        let settings = json!({"oxa":{"local":{"models":{"local-model":profile}}}});
        assert_eq!(
            from_user_settings(&settings, &layout(), "local-model", 8080),
            Err(Error::Environment)
        );
    }
}

#[test]
fn resolving_one_model_does_not_mutate_or_reuse_another_models_overrides() {
    let mut installed = layout();
    installed.models.insert(
        "other".into(),
        InstalledModel {
            model: "/opt/example/models/other.gguf".into(),
            arguments: vec![],
        },
    );
    let settings =
        json!({"oxa":{"local":{"models":{"local-model":{"arguments":[],"environment":{"ONE":"1"}}}}}});
    let before = settings.clone();
    let one = from_user_settings(&settings, &installed, "local-model", 8080).unwrap();
    let other = from_user_settings(&settings, &installed, "other", 8081).unwrap();
    assert_eq!(one.arguments.len(), 8);
    assert!(!other.environment.contains_key("ONE"));
    assert_eq!(other.model.to_str(), Some("/opt/example/models/other.gguf"));
    assert_eq!(settings, before);
}

#[test]
fn invalid_layout_values_fail_before_any_supervision_work() {
    let mut installed = layout();
    installed.binary = "/".into();
    assert_eq!(
        from_user_settings(&json!({}), &installed, "local-model", 8080),
        Err(Error::Binary)
    );
    installed = layout();
    installed.environment.insert("BAD=NAME".into(), "value".into());
    assert_eq!(
        from_user_settings(&json!({}), &installed, "local-model", 8080),
        Err(Error::Environment)
    );
    assert_eq!(
        from_user_settings(&json!({}), &layout(), "bad\nmodel", 8080),
        Err(Error::Alias)
    );
}

#[test]
fn short_alias_cannot_replace_selected_identity_from_either_argument_source() {
    let mut installed = layout();
    installed.models.get_mut("local-model").unwrap().arguments = vec!["-a".into(), "other-model".into()];
    assert_eq!(
        from_user_settings(&json!({}), &installed, "local-model", 8080),
        Err(Error::OwnedArgument)
    );
    let settings = json!({"oxa":{"local":{"models":{"local-model":{
        "arguments":["-a","other-model"]
    }}}}});
    assert_eq!(
        from_user_settings(&settings, &layout(), "local-model", 8080),
        Err(Error::OwnedArgument)
    );
}

#[test]
fn shipped_moe_aliases_default_to_sampled_mtp_with_their_existing_cutoffs() {
    for (alias, cutoff) in [
        ("ornith-35b-a3b-local", "32768"),
        ("qwen3.6-35b-a3b-local", "16384"),
    ] {
        let mut installed = layout();
        installed
            .models
            .insert(alias.into(), installed.models["local-model"].clone());
        let profile = from_user_settings(&json!({}), &installed, alias, 8080).unwrap();
        assert_eq!(
            &profile.arguments[8..],
            ["--ctx-size", "8192", "--mtp-max-ctx", cutoff, "--mtp-sampled"].map(OsString::from)
        );
    }
}

#[test]
fn model_mtp_settings_override_defaults_without_changing_other_models() {
    let alias = "ornith-35b-a3b-local";
    let mut installed = layout();
    installed
        .models
        .insert(alias.into(), installed.models["local-model"].clone());
    let user = json!({"oxa":{"local":{"models":{(alias):{"mtp_max_context":4096,"sampled_mtp":false}}}}});
    let profile = from_user_settings(&user, &installed, alias, 8080).unwrap();
    assert_eq!(
        &profile.arguments[8..],
        ["--ctx-size", "8192", "--mtp-max-ctx", "4096"].map(OsString::from)
    );
    let unrelated = from_user_settings(&user, &installed, "local-model", 8080).unwrap();
    assert_eq!(
        &unrelated.arguments[8..],
        ["--ctx-size", "8192"].map(OsString::from)
    );
}

#[test]
fn explicit_literal_mtp_flags_remain_the_callers_choice() {
    let alias = "qwen3.6-35b-a3b-local";
    let mut installed = layout();
    installed.models.insert(
        alias.into(),
        InstalledModel {
            model: "/models/synthetic.gguf".into(),
            arguments: vec!["--mtp-max-ctx".into(), "8192".into(), "--mtp-sampled".into()],
        },
    );
    let profile = from_user_settings(&json!({}), &installed, alias, 8080).unwrap();
    assert_eq!(
        &profile.arguments[8..],
        ["--mtp-max-ctx", "8192", "--mtp-sampled"].map(OsString::from)
    );
}

#[test]
fn malformed_or_ambiguous_mtp_settings_are_refused() {
    for settings in [
        json!({"sampled_mtp":"true"}),
        json!({"sampled_mtp":null}),
        json!({"mtp_max_context":0}),
        json!({"mtp_max_context":-1}),
        json!({"mtp_max_context":1.5}),
        json!({"mtp_max_context":4294967296_u64}),
        json!({"sampled_mtp":false,"arguments":["--mtp-sampled"]}),
        json!({"mtp_max_context":4096,"arguments":["--mtp-max-ctx=8192"]}),
    ] {
        let user = json!({"oxa":{"local":{"models":{"local-model":settings}}}});
        assert!(from_user_settings(&user, &layout(), "local-model", 8080).is_err());
    }
}

#[test]
fn equals_form_mtp_flags_are_rejected_before_launch() {
    for arguments in [vec!["--mtp-max-ctx=8192"], vec!["--mtp-sampled=true"]] {
        let user = json!({"oxa":{"local":{"models":{"local-model":{"arguments":arguments}}}}});
        assert_eq!(
            from_user_settings(&user, &layout(), "local-model", 8080),
            Err(Error::Arguments)
        );
    }
}

// ----- N9: opt-in battery decode team (design + fixture contract) ---------------------------

use oxc_local::battery::{Decision, Keep, Power};
use oxc_local::launch_profile::{resolve, BatteryTeam, LaunchPower};

const TEAM: &[usize] = &[0, 1, 2, 3, 4, 5, 6, 7];

fn cpu_layout(cpus: &str) -> InstalledLayout {
    let mut installed = layout();
    installed.models.get_mut("local-model").unwrap().arguments = vec![
        "--oxe-cpus".into(),
        cpus.into(),
        "--ctx-size".into(),
        "8192".into(),
    ];
    installed
}

fn at(power: Power, allowed: Option<&[usize]>) -> LaunchPower<'_> {
    LaunchPower {
        state: power,
        allowed,
    }
}

fn cpu_value(profile: &oxc_local::launch_profile::LaunchProfile) -> String {
    let words: Vec<&str> = profile.arguments.iter().map(|w| w.to_str().unwrap()).collect();
    let index = words
        .iter()
        .position(|w| *w == "--oxe-cpus")
        .expect("--oxe-cpus retained");
    words[index + 1].to_owned()
}

fn opt_in(team: &str) -> serde_json::Value {
    json!({"oxa":{"local":{"models":{"local-model":{"battery_decode_team":team}}}}})
}

#[test]
fn battery_policy_is_inert_until_opted_in_and_never_changes_other_words() {
    let installed = cpu_layout("0-7");
    let plain = from_user_settings(&json!({}), &installed, "local-model", 8080).unwrap();
    assert_eq!(plain.battery, BatteryTeam::Off);
    for power in [Power::Ac, Power::Charging, Power::Discharging, Power::Unknown] {
        let profile = resolve(
            &json!({}),
            &installed,
            "local-model",
            8080,
            Some(&at(power, Some(TEAM))),
        )
        .unwrap();
        assert_eq!(profile.arguments, plain.arguments, "{power:?}");
        assert_eq!(profile.environment, plain.environment);
        assert_eq!(profile.battery, BatteryTeam::Off);
    }
    // An explicit user CPU list wins over the declared team, at any power state.
    let user = json!({"oxa":{"local":{"models":{"local-model":{
        "arguments":["--oxe-cpus","2,3","--ctx-size","8192"],
        "battery_decode_team":"0,1"
    }}}}});
    let profile = resolve(
        &user,
        &installed,
        "local-model",
        8080,
        Some(&at(Power::Discharging, Some(TEAM))),
    )
    .unwrap();
    assert_eq!(profile.battery, BatteryTeam::YieldsToExplicit);
    assert_eq!(cpu_value(&profile), "2,3");
    // A user list identical to the installed team is not an override: the opt-in applies
    // (this is the resident factory's shape, where both come from the same user object).
    let user = json!({"oxa":{"local":{"models":{"local-model":{
        "arguments":["--oxe-cpus","0-7","--ctx-size","8192"],
        "battery_decode_team":"0-3"
    }}}}});
    let profile = resolve(
        &user,
        &installed,
        "local-model",
        8080,
        Some(&at(Power::Discharging, Some(TEAM))),
    )
    .unwrap();
    assert_eq!(profile.battery, BatteryTeam::Decided { changed: true });
    assert_eq!(cpu_value(&profile), "0,1,2,3");
}

#[test]
fn discharging_selects_the_declared_smaller_team_and_only_that_word_changes() {
    let installed = cpu_layout("0-7");
    let ac = resolve(
        &opt_in("0-3"),
        &installed,
        "local-model",
        8080,
        Some(&at(Power::Ac, Some(TEAM))),
    )
    .unwrap();
    let battery = resolve(
        &opt_in("0-3"),
        &installed,
        "local-model",
        8080,
        Some(&at(Power::Discharging, Some(TEAM))),
    )
    .unwrap();
    assert_eq!(ac.battery, BatteryTeam::Decided { changed: false });
    assert_eq!(battery.battery, BatteryTeam::Decided { changed: true });
    assert_eq!(cpu_value(&ac), "0-7");
    assert_eq!(cpu_value(&battery), "0,1,2,3");
    assert_eq!(battery.model, ac.model);
    assert_eq!(battery.binary, ac.binary);
    assert_eq!(battery.environment, ac.environment);
    let differing: Vec<usize> = (0..ac.arguments.len())
        .filter(|i| ac.arguments[*i] != battery.arguments[*i])
        .collect();
    assert_eq!(differing, [9], "only the --oxe-cpus value may change");
    // The selected CPUs are a strict subset of the installed, already-supported team.
    let chosen = oxc_local::battery::parse_cpus(&cpu_value(&battery)).unwrap();
    assert!(chosen.iter().all(|cpu| TEAM.contains(cpu)) && chosen.len() < TEAM.len());
}

#[test]
fn ac_charging_unknown_and_no_mask_keep_the_installed_team() {
    let installed = cpu_layout("0-7");
    for (power, allowed) in [
        (Power::Ac, Some(TEAM)),
        (Power::Charging, Some(TEAM)),
        (Power::Unknown, Some(TEAM)),
        (Power::Discharging, None),
        (Power::Discharging, Some(&[4usize, 5, 6, 7][..])),
    ] {
        let profile = resolve(
            &opt_in("0-3"),
            &installed,
            "local-model",
            8080,
            Some(&at(power, allowed)),
        )
        .unwrap();
        assert_eq!(cpu_value(&profile), "0-7", "{power:?} {allowed:?}");
        assert_eq!(profile.battery, BatteryTeam::Decided { changed: false });
    }
    // A mask that leaves part of the declared team keeps only the usable CPUs.
    let profile = resolve(
        &opt_in("0-3"),
        &installed,
        "local-model",
        8080,
        Some(&at(Power::Discharging, Some(&[0, 1][..]))),
    )
    .unwrap();
    assert_eq!(cpu_value(&profile), "0,1");
}

#[test]
fn a_declared_team_must_be_a_smaller_subset_of_one_installed_team() {
    let installed = cpu_layout("0-7");
    for team in ["", "0-7", "0-8", "8", "0-3,0-1", "x"] {
        let user = json!({"oxa":{"local":{"models":{"local-model":{"battery_decode_team":team}}}}});
        assert_eq!(
            resolve(
                &user,
                &installed,
                "local-model",
                8080,
                Some(&at(Power::Discharging, Some(TEAM)))
            ),
            Err(Error::Battery),
            "{team:?}"
        );
    }
    let user = json!({"oxa":{"local":{"models":{"local-model":{"battery_decode_team":7}}}}});
    assert_eq!(
        resolve(
            &user,
            &installed,
            "local-model",
            8080,
            Some(&at(Power::Discharging, Some(TEAM)))
        ),
        Err(Error::Battery)
    );
    // No installed team to shrink, a repeated flag, or an `=` spelling: fail closed.
    for arguments in [
        vec!["--ctx-size", "8192"],
        vec!["--oxe-cpus", "0-7", "--oxe-cpus", "0-7"],
        vec!["--oxe-cpus=0-7"],
    ] {
        let mut installed = layout();
        installed.models.get_mut("local-model").unwrap().arguments =
            arguments.iter().map(|w| w.to_string()).collect();
        assert_eq!(
            resolve(
                &opt_in("0-3"),
                &installed,
                "local-model",
                8080,
                Some(&at(Power::Discharging, Some(TEAM)))
            ),
            Err(Error::Battery),
            "{arguments:?}"
        );
    }
}

#[test]
fn the_mixed_state_decision_is_pure_and_ordered() {
    // The same inputs always give the same decision; the policy never reads a clock or fs.
    for power in [Power::Ac, Power::Charging, Power::Discharging, Power::Unknown] {
        let first = oxc_local::battery::decide(power, TEAM, Some(&[0, 1][..]), false, Some(TEAM));
        let second = oxc_local::battery::decide(power, TEAM, Some(&[0, 1][..]), false, Some(TEAM));
        assert_eq!(first, second);
        assert_eq!(
            first,
            if power == Power::Discharging {
                Decision::Reduced(vec![0, 1])
            } else {
                Decision::Installed(Keep::Power(power))
            }
        );
    }
}
