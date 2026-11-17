use oxa_cli::lead_access::*;
use oxa_config::{dirs_from_env, Env, Settings};
use oxa_fake::TestEnv;
use serde_json::json;

struct Local;
impl IdentityProvider for Local {
    fn verified(&self) -> Option<VerifiedIdentity<'_>> {
        Some(VerifiedIdentity::LocalOwner)
    }
}
struct Proxy(&'static str);
impl IdentityProvider for Proxy {
    fn verified(&self) -> Option<VerifiedIdentity<'_>> {
        Some(VerifiedIdentity::TailscaleLogin(self.0))
    }
}
const OWNER: &str = "owner@example.invalid";
fn load(env: &TestEnv) -> Result<Config, &'static str> {
    let test_home = env.home().display().to_string();
    let pairs = [("HOME", test_home.as_str())];
    let dirs = dirs_from_env(&Env::from_pairs(&pairs), None).unwrap();
    Config::load(&dirs, &env.work())
}
fn user(env: &TestEnv, value: serde_json::Value) -> Config {
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({"oxa":{"lead":{"serve":value}}}).to_string(),
    );
    load(env).unwrap()
}
fn unread() {
    panic!("refusal must precede observation")
}
fn check(config: &Config, header: Option<&str>, identity: &impl IdentityProvider) -> Result<(), Refusal> {
    read_only(
        config,
        "GET",
        "127.0.0.1".parse().unwrap(),
        header,
        identity,
        unread,
    )
}

#[test]
fn disabled_local_defaults_and_project_cannot_grant_access() {
    let env = TestEnv::new();
    env.write(
        "work/.oxa/settings.json",
        &json!({"oxa":{"lead":{"serve":{
            "enabled":true,"identity":"tailscale","allowed_identities":[OWNER]}}}})
        .to_string(),
    );
    let config = load(&env).unwrap();
    assert_eq!(config.mode(), Mode::Local);
    assert_eq!(config.endpoint(), ("127.0.0.1".parse().unwrap(), 8743));
    assert_eq!(check(&config, None, &Local), Err(Refusal::Disabled));
    let config = user(&env, json!({"enabled":true}));
    assert_eq!(
        check(&config, Some(OWNER), &Proxy(OWNER)),
        Err(Refusal::NotAllowed)
    );
    assert_eq!(check(&config, None, &Unverified), Err(Refusal::Unauthenticated));
    let mut reads = 0;
    assert_eq!(
        read_only(&config, "GET", "::1".parse().unwrap(), None, &Local, || {
            reads += 1;
            7
        }),
        Ok(7)
    );
    assert_eq!(reads, 1);
}

#[test]
fn tailscale_requires_verified_request_identity_and_exact_user_allowlist() {
    let env = TestEnv::new();
    let config = user(
        &env,
        json!({"enabled":true,"identity":"tailscale","allowed_identities":[OWNER]}),
    );
    assert_eq!(
        check(&config, Some(OWNER), &Unverified),
        Err(Refusal::Unauthenticated)
    );
    assert_eq!(check(&config, Some(OWNER), &Local), Err(Refusal::NotAllowed));
    assert_eq!(
        check(&config, Some(OWNER), &Proxy("other@example.invalid")),
        Err(Refusal::NotAllowed)
    );
    assert_eq!(check(&config, None, &Proxy(OWNER)), Err(Refusal::NotAllowed));
    assert_eq!(
        check(&config, Some("other@example.invalid"), &Proxy(OWNER)),
        Err(Refusal::NotAllowed)
    );
    assert_eq!(
        read_only(
            &config,
            "GET",
            "127.0.0.1".parse().unwrap(),
            Some(OWNER),
            &Proxy(OWNER),
            || 7
        ),
        Ok(7)
    );
    let config = user(&env, json!({"enabled":true,"identity":"tailscale"}));
    assert_eq!(
        check(&config, Some(OWNER), &Proxy(OWNER)),
        Err(Refusal::NotAllowed)
    );
}

#[test]
fn remote_peers_and_mutations_refuse_without_observations() {
    let env = TestEnv::new();
    let config = user(&env, json!({"enabled":true}));
    assert_eq!(
        read_only(&config, "GET", "192.0.2.1".parse().unwrap(), None, &Local, unread),
        Err(Refusal::LocalOnly)
    );
    for method in ["POST", "PUT", "DELETE", "PATCH"] {
        assert_eq!(
            read_only(
                &config,
                method,
                "127.0.0.1".parse().unwrap(),
                None,
                &Local,
                unread
            ),
            Err(Refusal::ReadOnly)
        );
    }
}

#[test]
fn endpoint_settings_are_generic_and_errors_never_reflect_input() {
    let env = TestEnv::new();
    for port in [80, 443, 8743, 9119] {
        assert_eq!(user(&env, json!({"port":port})).endpoint().1, port);
    }
    for value in [
        json!({"port":0}),
        json!({"port":65536}),
        json!({"enabled":"PRIVATE_VALUE"}),
        json!({"identity":"PRIVATE_VALUE"}),
    ] {
        env.write(
            "home/.oxa/agent/settings.json",
            &json!({"oxa":{"lead":{"serve":value}}}).to_string(),
        );
        let Err(error) = load(&env) else {
            panic!("invalid settings accepted")
        };
        assert!(!error.contains("PRIVATE_VALUE"));
    }
}

#[test]
fn an_untrusted_forwarded_header_is_not_an_identity() {
    let env = TestEnv::new();
    // A loopback peer that presents the header without a verified adapter is unauthenticated,
    // not merely "not allowed": the header alone proves nothing.
    for value in [
        json!({"enabled":true}),
        json!({"enabled":true,"identity":"tailscale","allowed_identities":[OWNER]}),
    ] {
        let config = user(&env, value);
        assert_eq!(
            check(&config, Some(OWNER), &Unverified),
            Err(Refusal::Unauthenticated)
        );
    }
}

#[test]
fn only_a_loopback_bind_is_accepted() {
    let env = TestEnv::new();
    assert_eq!(
        user(&env, json!({"enabled":true,"bind":"::1"})).endpoint(),
        ("::1".parse().unwrap(), 8743)
    );
    for bind in ["0.0.0.0", "192.0.2.1", "::"] {
        env.write(
            "home/.oxa/agent/settings.json",
            &json!({"oxa":{"lead":{"serve":{"enabled":true,"bind":bind}}}}).to_string(),
        );
        assert_eq!(
            load(&env).err(),
            Some("lead serve bind must be a loopback address"),
            "accepted bind {bind}"
        );
    }
}

#[test]
fn documented_setting_is_a_known_schema_key() {
    let env = TestEnv::new();
    // The page tells the user to put `oxa.lead.serve` in settings.json; an unknown key would log
    // a diagnostic on every start, in every mode, for a user who did as documented.
    user(
        &env,
        json!({"enabled":true,"identity":"tailscale","allowed_identities":[OWNER]}),
    );
    let test_home = env.home().display().to_string();
    let pairs = [("HOME", test_home.as_str())];
    let dirs = dirs_from_env(&Env::from_pairs(&pairs), None).unwrap();
    let settings = Settings::load(&dirs, &env.work(), false);
    let warnings: Vec<&str> = settings
        .diagnostics()
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();
    assert!(
        warnings.is_empty(),
        "documented settings must not warn: {warnings:?}"
    );
    assert!(settings.get(SETTING).is_some(), "the user layer must be read");
}
