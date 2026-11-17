use oxa_cli::lead_access::{proxy, Config};
use oxa_config::{dirs_from_env, Env};
use oxa_fake::TestEnv;
use serde_json::{json, Value};

const OWNER: &str = "owner@example.invalid";
const AUTHORITY: &str = "lead.example.invalid:9448";

fn load(env: &TestEnv) -> Result<Config, &'static str> {
    let home = env.home().display().to_string();
    let dirs = dirs_from_env(&Env::from_pairs(&[("HOME", home.as_str())]), None).unwrap();
    Config::load(&dirs, &env.work())
}

fn configure(env: &TestEnv, proxy: Value) -> Result<Config, &'static str> {
    env.write(
        "home/.oxa/agent/settings.json",
        &json!({"oxa":{"lead":{"serve":{
            "enabled":true,"identity":"tailscale","allowed_identities":[OWNER],"proxy":proxy
        }}}})
        .to_string(),
    );
    load(env)
}

#[test]
fn pre_bind_requires_explicit_user_proxy_trust() {
    let env = TestEnv::new();
    assert!(proxy::pre_bind(&load(&env).unwrap()).is_err());
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"lead":{"serve":{"enabled":true}}}}"#,
    );
    assert_eq!(proxy::pre_bind(&load(&env).unwrap()), Ok(()));
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"lead":{"serve":{"enabled":true,"identity":"tailscale"}}}}"#,
    );
    env.write(
        "work/.oxa/settings.json",
        &json!({"oxa":{"lead":{"serve":{
            "proxy":{"uid":12345,"authority":AUTHORITY}
        }}}})
        .to_string(),
    );
    assert_eq!(
        proxy::pre_bind(&load(&env).unwrap()),
        Err("trusted proxy is not configured")
    );
    let config = configure(&env, json!({"uid":12345,"authority":AUTHORITY})).unwrap();
    if cfg!(target_os = "linux") {
        assert_eq!(proxy::pre_bind(&config), Ok(()));
    } else {
        assert_eq!(
            proxy::pre_bind(&config),
            Err("trusted proxy identity requires Linux")
        );
    }
}

#[test]
fn malformed_trust_fails_without_reflecting_private_values() {
    for value in [
        Value::Null,
        json!([]),
        json!({}),
        json!({"uid":-1,"authority":AUTHORITY}),
        json!({"uid":4294967296_u64,"authority":AUTHORITY}),
        json!({"uid":12345,"authority":AUTHORITY,"SYNTHETIC_PRIVATE_FIELD":true}),
    ] {
        let error = configure(&TestEnv::new(), value).err().unwrap();
        assert!(!error.contains("SYNTHETIC") && !error.contains(AUTHORITY));
    }
    for authority in [
        "",
        "SYNTHETIC_PRIVATE_AUTHORITY/path",
        "user@host.invalid",
        "host.invalid:0",
        "host.invalid:65536",
        "host.invalid:abc",
        "host.invalid\r\nInjected: value",
        "::1",
        "host..invalid",
        "-host.invalid",
        "host.invalid?query",
        "host.invalid#fragment",
    ] {
        let error = configure(&TestEnv::new(), json!({"uid":12345,"authority":authority}))
            .err()
            .unwrap();
        assert_eq!(error, "invalid lead proxy authority");
    }
    for authority in [
        "host.example.invalid",
        "HOST.example.invalid:9448",
        "127.0.0.1:9448",
        "[::1]",
        "[::1]:9448",
    ] {
        assert!(configure(&TestEnv::new(), json!({"uid":12345,"authority":authority})).is_ok());
    }
}

#[cfg(target_os = "linux")]
mod loopback {
    use super::*;
    use oxa_cli::lead_access::{read_only, Refusal};
    use std::cell::Cell;
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::MetadataExt;

    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let server = listener.accept().unwrap().0;
        (client, server)
    }

    #[test]
    fn kernel_principal_and_configured_authority_gate_every_read() {
        let env = TestEnv::new();
        let uid = std::fs::metadata(env.root()).unwrap().uid();
        let config = configure(&env, json!({"uid":uid,"authority":AUTHORITY})).unwrap();
        let (_client, server) = pair();
        let reads = Cell::new(0);
        let access = |method, authority, login| {
            let proof = proxy::verify(&config, &server, login)?;
            read_only(
                &config,
                method,
                server.peer_addr().unwrap().ip(),
                authority,
                login,
                &proof,
                || {
                    reads.set(reads.get() + 1);
                    "snapshot"
                },
            )
        };
        assert_eq!(
            access("GET", "LEAD.EXAMPLE.INVALID:9448", Some(OWNER)),
            Ok("snapshot")
        );
        assert_eq!(
            access("GET", "attacker.example.invalid:9448", Some(OWNER)),
            Err(Refusal::NotAllowed)
        );
        assert_eq!(
            access("GET", AUTHORITY, Some("guest@example.invalid")),
            Err(Refusal::NotAllowed)
        );
        assert_eq!(access("GET", AUTHORITY, None), Err(Refusal::Unauthenticated));
        assert_eq!(
            access("GET", AUTHORITY, Some("invalid\r\nlogin")),
            Err(Refusal::Unauthenticated)
        );
        assert_eq!(access("POST", AUTHORITY, Some(OWNER)), Err(Refusal::ReadOnly));
        assert_eq!(reads.get(), 1, "every refusal precedes observation");
    }

    #[test]
    fn a_forged_header_and_project_setting_cannot_replace_kernel_provenance() {
        let env = TestEnv::new();
        let uid = std::fs::metadata(env.root()).unwrap().uid();
        configure(&env, json!({"uid":uid ^ 1,"authority":AUTHORITY})).unwrap();
        env.write(
            "work/.oxa/settings.json",
            &json!({"oxa":{"lead":{"serve":{
                "proxy":{"uid":uid,"authority":AUTHORITY}
            }}}})
            .to_string(),
        );
        let config = load(&env).unwrap();
        let (_client, server) = pair();
        assert_eq!(
            proxy::verify(&config, &server, Some(OWNER)).err(),
            Some(Refusal::Unauthenticated)
        );
        let fallback = std::fs::read_to_string("/proc/sys/kernel/overflowuid")
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let config = configure(&env, json!({"uid":fallback,"authority":AUTHORITY})).unwrap();
        assert_eq!(
            proxy::pre_bind(&config),
            Err("kernel fallback proxy principal is refused")
        );
        assert_eq!(
            proxy::verify(&config, &server, Some(OWNER)).err(),
            Some(Refusal::Unauthenticated)
        );
    }
}
