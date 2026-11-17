use oxa_cli::lead_access::*;
use oxa_config::{dirs_from_env, Env};
use oxa_fake::TestEnv;
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr};

struct Local;
impl IdentityProvider for Local {
    fn verified(&self) -> Option<VerifiedIdentity<'_>> {
        Some(VerifiedIdentity::LocalOwner)
    }
}
struct Proxy(&'static str);
impl IdentityProvider for Proxy {
    fn verified(&self) -> Option<VerifiedIdentity<'_>> {
        Some(VerifiedIdentity::TailscaleLogin(self.0, AUTHORITY))
    }
}
const OWNER: &str = "owner@example.invalid";
const OWNER_PROXY: Proxy = Proxy(OWNER);
const AUTHORITY: &str = "127.0.0.1:8743";
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
fn load(env: &TestEnv) -> Result<Config, &'static str> {
    let test_home = env.home().display().to_string();
    let pairs = [("HOME", test_home.as_str())];
    let dirs = dirs_from_env(&Env::from_pairs(&pairs), None).unwrap();
    Config::load(&dirs, &env.work())
}
fn user(env: &TestEnv, value: serde_json::Value) -> Config {
    let document = json!({"oxa":{"lead":{"serve":value}}});
    assert!(oxa_config::validate(&document, "user settings").is_empty());
    env.write("home/.oxa/agent/settings.json", &document.to_string());
    load(env).unwrap()
}
fn unread() {
    panic!("refusal must precede observation")
}
fn check(config: &Config, header: Option<&str>, identity: &impl IdentityProvider) -> Result<(), Refusal> {
    check_host(config, AUTHORITY, header, identity)
}
fn check_host(
    config: &Config,
    host: &str,
    header: Option<&str>,
    identity: &impl IdentityProvider,
) -> Result<(), Refusal> {
    read_only(config, "GET", LOOPBACK, host, header, identity, unread)
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
    assert_eq!(config.endpoint(), (LOOPBACK, 8743));
    assert_eq!(check(&config, None, &Local), Err(Refusal::Disabled));
    let config = user(&env, json!({"enabled":true}));
    assert_eq!(
        check(&config, Some(OWNER), &OWNER_PROXY),
        Err(Refusal::NotAllowed)
    );
    assert_eq!(check(&config, None, &Unverified), Err(Refusal::Unauthenticated));
    let mut reads = 0;
    assert_eq!(
        read_only(
            &config,
            "GET",
            "::1".parse().unwrap(),
            AUTHORITY,
            None,
            &Local,
            || {
                reads += 1;
                7
            }
        ),
        Ok(7)
    );
    assert_eq!(reads, 1);
    assert_eq!(
        check_host(&config, "attacker.invalid:8743", None, &Local),
        Err(Refusal::NotAllowed)
    );
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
    assert_eq!(check(&config, None, &OWNER_PROXY), Err(Refusal::NotAllowed));
    assert_eq!(
        check(&config, Some("other@example.invalid"), &OWNER_PROXY),
        Err(Refusal::NotAllowed)
    );
    assert_eq!(
        read_only(
            &config,
            "GET",
            LOOPBACK,
            AUTHORITY,
            Some(OWNER),
            &OWNER_PROXY,
            || 7
        ),
        Ok(7)
    );
    assert_eq!(
        check_host(&config, "attacker.invalid:8743", Some(OWNER), &OWNER_PROXY),
        Err(Refusal::NotAllowed)
    );
    let config = user(&env, json!({"enabled":true,"identity":"tailscale"}));
    assert_eq!(
        check(&config, Some(OWNER), &OWNER_PROXY),
        Err(Refusal::NotAllowed)
    );
}

#[test]
fn remote_peers_and_mutations_refuse_without_observations() {
    let env = TestEnv::new();
    let config = user(&env, json!({"enabled":true}));
    assert_eq!(
        read_only(
            &config,
            "GET",
            "192.0.2.1".parse().unwrap(),
            AUTHORITY,
            None,
            &Local,
            unread
        ),
        Err(Refusal::LocalOnly)
    );
    for method in ["POST", "PUT", "DELETE", "PATCH"] {
        assert_eq!(
            read_only(&config, method, LOOPBACK, AUTHORITY, None, &Local, unread),
            Err(Refusal::ReadOnly)
        );
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
