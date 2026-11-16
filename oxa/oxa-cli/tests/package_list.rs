//! Package listing must explain the project trust gate without bypassing it.
use oxa_cli::run_pkg;
use oxa_config::{Dirs, Settings};
use oxa_fake::TestEnv;
use oxa_perm::TrustStore;
use oxa_resources::{declared_packages, Scope};

fn command(env: &TestEnv, verb: &str, words: &[&str]) -> String {
    let dirs = Dirs::under(env.root());
    let mut out = Vec::new();
    let mut err = Vec::new();
    let words = words.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        run_pkg(verb, &words, &dirs, &env.work(), &mut out, &mut err).unwrap(),
        0
    );
    assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    String::from_utf8(out).unwrap()
}

#[test]
fn package_list_explains_hidden_project_declarations_after_install() {
    let env = TestEnv::new();
    env.write(
        "package/skills/synthetic/SKILL.md",
        "---\nname: synthetic\ndescription: Synthetic fixture.\n---\nReturn a short answer.\n",
    );
    let source = env.root().join("package");
    let installed = command(&env, "install", &["-l", source.to_str().unwrap()]);
    assert!(installed.contains("(project)"), "{installed}");
    let dirs = Dirs::under(env.root());
    let settings_before = std::fs::read(dirs.project_settings_file(&env.work())).unwrap();
    for words in [&[][..], &["-l"][..]] {
        let listed = command(&env, "list", words);
        assert!(listed.contains("project is not trusted"), "{listed}");
        assert!(listed.contains("run oxa once in it to decide"), "{listed}");
        assert!(!listed.contains("No packages declared."), "{listed}");
        assert!(!listed.contains("installed at"), "{listed}");
    }
    assert_eq!(
        std::fs::read(dirs.project_settings_file(&env.work())).unwrap(),
        settings_before
    );
    assert!(!dirs.trust_file().exists(), "listing must not grant trust");
    let settings = Settings::load(&dirs, &env.work(), false);
    assert!(declared_packages(&dirs, &env.work(), &settings, false).is_empty());

    // Once explicitly trusted and content-approved, the normal installed state is shown.
    let mut trust = TrustStore::load(&dirs.trust_file());
    trust.save(&dirs.trust_file(), &env.work(), true).unwrap();
    trust.approve(&dirs.trust_file(), &env.work()).unwrap();
    assert!(trust.trusted_as_approved(&env.work()));
    let listed = command(&env, "list", &[]);
    assert!(listed.contains("project "), "{listed}");
    assert!(listed.contains("installed at"), "{listed}");
    assert!(!listed.contains("hidden"), "{listed}");
}

#[test]
fn package_list_keeps_user_packages_visible_and_empty_state_accurate() {
    let env = TestEnv::new();
    assert_eq!(command(&env, "list", &[]), "No packages declared.\n");
    env.write("work/.oxa/settings.json", r#"{"packages":[]}"#);
    assert_eq!(command(&env, "list", &["-l"]), "No packages declared.\n");
    env.write("package/skills/synthetic/SKILL.md", "Synthetic skill.");
    let source = env.root().join("package");
    command(&env, "install", &[source.to_str().unwrap()]);
    command(&env, "install", &["-l", source.to_str().unwrap()]);
    let listed = command(&env, "list", &[]);
    assert!(listed.contains("user "), "{listed}");
    assert!(listed.contains("installed at"), "{listed}");
    assert!(listed.contains("project is not trusted"), "{listed}");
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), false);
    let visible = declared_packages(&dirs, &env.work(), &settings, false);
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].0, Scope::User);
    command(&env, "remove", &[source.to_str().unwrap()]);
    command(&env, "remove", &["-l", source.to_str().unwrap()]);
    assert_eq!(command(&env, "list", &[]), "No packages declared.\n");
}
