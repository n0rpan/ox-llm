//! Pasted images live directly under tmp, outside the session's output directory.
use oxa_fake::TestEnv;
use oxa_perm::{deny_reasons, ModePolicy, PathGuard};
use oxa_proto::{Access, Autonomy, Decision, Mode, Policy, YoloSwitch};

#[test]
fn pasted_images_keep_normal_read_approval_without_exposing_other_state() {
    let env = TestEnv::new();
    let tmp = env.agent_dir().join("tmp");
    let mut policy = ModePolicy::new().with_guard(PathGuard::new(&env.home(), &env.agent_dir(), &tmp));
    policy.set_spill_dir(&tmp.join("current"));
    // The filter predates the paste, just as it does in a running session.
    let filter = policy.path_filter().unwrap();
    for ext in ["png", "jpg", "gif", "webp"] {
        let image = tmp.join(format!("paste-0123456789abcdef.{ext}"));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&image, "synthetic image").unwrap();
        for mode in Mode::ALL {
            assert_eq!(
                policy.decide(mode, &Access::Read(image.clone()), &env.work()),
                Decision::Ask,
                "pasted images use the ordinary outside-project approval ({mode:?})"
            );
            assert_eq!(
                policy.decide(mode, &Access::Write(image.clone()), &env.work()),
                Decision::Deny(deny_reasons::SENSITIVE),
                "the image exception is read-only"
            );
        }
        assert!(!filter.hides(&image), "a permitted image is not hidden");
    }
    let image = Access::Read(tmp.join("paste-0123456789abcdef.png"));
    policy.remember(&image);
    assert_eq!(policy.decide(Mode::Ask, &image, &env.work()), Decision::Allow);

    let other = tmp.join("other/paste-0123456789abcdef.png");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::write(&other, "other session").unwrap();
    let linked = tmp.join("paste-1111111111111111.png");
    std::os::unix::fs::symlink(&other, &linked).unwrap();
    let key = env.home().join(".ssh/paste-2222222222222222.png");
    std::fs::create_dir_all(key.parent().unwrap()).unwrap();
    std::fs::write(&key, "synthetic credential").unwrap();
    let key_link = tmp.join("paste-2222222222222222.png");
    std::os::unix::fs::symlink(&key, &key_link).unwrap();
    // Even a remembered grant on tmp and auto cannot expose unrelated agent state.
    let policy = policy.with_yolo(YoloSwitch::new(Autonomy::Auto));
    for path in [
        other.clone(),
        tmp.join("paste-0123456789abcdef.png/../other/out.txt"),
        tmp.join("paste-short.png"),
        tmp.join("paste-0123456789abcdeg.png"),
        tmp.join("paste-0123456789abcdef.txt"),
        tmp.join("out.txt"),
        linked,
        key_link,
        env.agent_dir().join("trust.json"),
    ] {
        for mode in Mode::ALL {
            assert_eq!(
                policy.decide(mode, &Access::Read(path.clone()), &env.work()),
                Decision::Deny(deny_reasons::SENSITIVE),
                "{mode:?} {path:?}"
            );
        }
    }
    assert!(filter.hides(&other));
    assert!(filter.hides(&key));
    assert_eq!(policy.decide(Mode::Ask, &image, &env.work()), Decision::Allow);
}

#[test]
fn pasted_image_names_cannot_declassify_symlinked_credentials() {
    let env = TestEnv::new();
    let tmp = env.agent_dir().join("tmp");
    let image = tmp.join("paste-0123456789abcdef.png");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(&image, "synthetic credential").unwrap();
    let credential = env.home().join(".config/oxc/llm/cloud-secrets.json");
    std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&image, &credential).unwrap();
    let policy = ModePolicy::new()
        .with_guard(PathGuard::new(&env.home(), &env.agent_dir(), &tmp))
        .with_yolo(YoloSwitch::new(Autonomy::Auto));
    for path in [&image, &credential] {
        for mode in Mode::ALL {
            assert_eq!(
                policy.decide(mode, &Access::Read(path.clone()), &env.work()),
                Decision::Deny(deny_reasons::SENSITIVE),
                "a known credential target stays denied regardless of its name"
            );
        }
    }
    assert!(policy.path_filter().unwrap().hides(&image));
}
