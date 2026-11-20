//! Default-off lead access scaffold. No listener or production identity adapter is installed.
use oxa_config::{Dirs, Settings};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;

pub const SETTING: &str = "oxa.lead.serve";
pub mod proxy;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Local,
    Tailscale,
}

/// Identity settings are deliberately not Debug/Display output.
pub struct Config {
    enabled: bool,
    fixed_prompts: bool,
    owner_chat: bool,
    mode: Mode,
    bind: IpAddr,
    port: u16,
    allowed: Vec<String>,
    proxy: Option<proxy::Trust>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            fixed_prompts: false,
            owner_chat: false,
            mode: Mode::Local,
            bind: Ipv4Addr::LOCALHOST.into(),
            port: 8743,
            allowed: vec![],
            proxy: None,
        }
    }
}
impl Config {
    /// Never merge project settings into listener or identity authority.
    pub fn load(dirs: &Dirs, cwd: &Path) -> Result<Self, &'static str> {
        let settings = Settings::load(dirs, cwd, false);
        let Some(value) = settings.get(SETTING) else {
            return Ok(Self::default());
        };
        let values = value.as_object().ok_or("invalid lead serve settings")?;
        if values.keys().any(|key| {
            ![
                "enabled",
                "fixed_prompts",
                "owner_chat",
                "identity",
                "bind",
                "port",
                "allowed_identities",
                "proxy",
            ]
            .contains(&key.as_str())
        }) {
            return Err("unknown lead serve setting");
        }
        let mut config = Self::default();
        if let Some(value) = values.get("enabled") {
            config.enabled = value.as_bool().ok_or("invalid lead serve enable flag")?;
        }
        if let Some(value) = values.get("fixed_prompts") {
            config.fixed_prompts = value.as_bool().ok_or("invalid fixed prompt flag")?;
        }
        if let Some(value) = values.get("owner_chat") {
            config.owner_chat = value.as_bool().ok_or("invalid owner chat flag")?;
        }
        if let Some(value) = values.get("identity") {
            config.mode = match value.as_str() {
                Some("local") => Mode::Local,
                Some("tailscale") => Mode::Tailscale,
                _ => return Err("invalid lead identity provider"),
            };
        }
        if let Some(value) = values.get("bind") {
            let bind: IpAddr = value
                .as_str()
                .and_then(|text| text.parse().ok())
                .ok_or("invalid lead serve bind")?;
            // The access check refuses every non-loopback peer, and the documented operator step
            // proxies to a loopback port, so a wider bind is a mistake rather than a choice.
            if !bind.is_loopback() {
                return Err("lead serve bind must be a loopback address");
            }
            config.bind = bind;
        }
        if let Some(value) = values.get("port") {
            config.port = value
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port > 0)
                .ok_or("invalid lead serve port")?;
        }
        if let Some(value) = values.get("allowed_identities") {
            let identities = value.as_array().ok_or("invalid lead identities")?;
            if identities.len() > 16 {
                return Err("too many lead identities");
            }
            for value in identities {
                let login = value.as_str().ok_or("invalid lead identity")?;
                if login.is_empty()
                    || login.len() > 128
                    || login.chars().any(char::is_whitespace)
                    || login.chars().any(char::is_control)
                {
                    return Err("invalid lead identity");
                }
                config.allowed.push(login.to_owned());
            }
        }
        config.proxy = proxy::Trust::parse(values.get("proxy"))?;
        if config.owner_chat && (config.mode != Mode::Tailscale || config.allowed.len() != 1) {
            return Err("owner chat requires one verified Tailscale owner");
        }
        Ok(config)
    }
    /// Pre-bind feature selection only; requests still require `read_only` authorization.
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    pub fn endpoint(&self) -> (IpAddr, u16) {
        (self.bind, self.port)
    }
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub(crate) fn owner_chat(&self) -> bool {
        self.owner_chat
    }

    pub(crate) fn fixed_prompts(&self) -> bool {
        self.fixed_prompts
    }
}

/// Trusted request-bound proof: local ownership, or login plus independently verified authority.
pub enum VerifiedIdentity<'a> {
    LocalOwner,
    TailscaleLogin(&'a str, &'a str),
}
/// Future local adapters must verify OS ownership; proxy adapters must authenticate proxy
/// provenance and expected authority, binding the login to this request. Never echo raw headers.
/// Production implementation and listener wiring require the owner's source read first.
pub trait IdentityProvider {
    fn verified(&self) -> Option<VerifiedIdentity<'_>>;
}
pub struct Unverified;
impl IdentityProvider for Unverified {
    fn verified(&self) -> Option<VerifiedIdentity<'_>> {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Disabled,
    ReadOnly,
    LocalOnly,
    Unauthenticated,
    NotAllowed,
    /// Local mode: the request carries no valid per-client token (#193).
    ClientToken,
}

/// Local mode needs more than the same-user peer: every request, page or POST, proves it holds
/// the token `oxa lead serve` minted. Compared without an early exit on the first differing byte.
pub(crate) fn client_token_ok(expected: &str, given: Option<&str>) -> bool {
    let Some(given) = given else {
        return false;
    };
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(1));
    }
    !expected.is_empty() && diff == 0
}

/// Authorize before obtaining any observation. No socket, process, mutation, or secret reads.
pub fn read_only<T>(
    config: &Config,
    method: &str,
    peer: IpAddr,
    authority: &str,
    forwarded_login: Option<&str>,
    identity: &impl IdentityProvider,
    read: impl FnOnce() -> T,
) -> Result<T, Refusal> {
    if !config.enabled {
        return Err(Refusal::Disabled);
    }
    if method != "GET" {
        return Err(Refusal::ReadOnly);
    }
    observe(config, peer, authority, forwarded_login, identity, read)
}

pub(crate) fn chat_post(
    config: &Config,
    peer: IpAddr,
    request: &crate::lead_http::Request<'_>,
    identity: &impl IdentityProvider,
) -> Result<(), Refusal> {
    if !config.enabled {
        return Err(Refusal::Disabled);
    }
    if !(config.fixed_prompts() || config.owner_chat()) || request.method != "POST" || request.path != "/chat"
    {
        return Err(Refusal::ReadOnly);
    }
    observe(
        config,
        peer,
        request.authority,
        request.forwarded_login,
        identity,
        || (),
    )
    .map(|_| ())
}

/// The owner's answer to an approval request (#1512 S6): the same owner gate as `chat_post`,
/// on its own path. Answering needs owner chat, so a fixed prompt alone never grants it.
pub(crate) fn approval_post(
    config: &Config,
    peer: IpAddr,
    request: &crate::lead_http::Request<'_>,
    identity: &impl IdentityProvider,
) -> Result<(), Refusal> {
    if !config.enabled {
        return Err(Refusal::Disabled);
    }
    if !config.owner_chat() || request.method != "POST" || request.path != "/approvals" {
        return Err(Refusal::ReadOnly);
    }
    observe(
        config,
        peer,
        request.authority,
        request.forwarded_login,
        identity,
        || (),
    )
    .map(|_| ())
}

fn observe<T>(
    config: &Config,
    peer: IpAddr,
    authority: &str,
    forwarded_login: Option<&str>,
    identity: &impl IdentityProvider,
    read: impl FnOnce() -> T,
) -> Result<T, Refusal> {
    if !peer.is_loopback() {
        return Err(Refusal::LocalOnly);
    }
    if authority.is_empty() || authority.len() > 255 {
        return Err(Refusal::NotAllowed);
    }
    match (config.mode, identity.verified()) {
        (Mode::Local, Some(VerifiedIdentity::LocalOwner))
            if forwarded_login.is_none()
                && authority.parse::<SocketAddr>().ok()
                    == Some(SocketAddr::new(config.bind, config.port)) =>
        {
            Ok(read())
        }
        (Mode::Tailscale, Some(VerifiedIdentity::TailscaleLogin(login, expected)))
            if authority.eq_ignore_ascii_case(expected)
                && forwarded_login == Some(login)
                && config.allowed.iter().any(|allowed| allowed == login) =>
        {
            Ok(read())
        }
        // A forwarded header alone never satisfies the requirement: without a verified identity
        // the request is unauthenticated, whichever mode is configured.
        (_, None) => Err(Refusal::Unauthenticated),
        _ => Err(Refusal::NotAllowed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lead_http::Request;

    /// A verified Tailscale login for the one configured owner.
    struct Owner;
    impl IdentityProvider for Owner {
        fn verified(&self) -> Option<VerifiedIdentity<'_>> {
            Some(VerifiedIdentity::TailscaleLogin(
                "owner",
                "lead.example.invalid:9448",
            ))
        }
    }

    fn config(enabled: bool, owner_chat: bool) -> Config {
        Config {
            enabled,
            fixed_prompts: false,
            owner_chat,
            mode: Mode::Tailscale,
            bind: Ipv4Addr::LOCALHOST.into(),
            port: 8743,
            allowed: vec!["owner".into()],
            proxy: None,
        }
    }

    fn request(method: &'static str, path: &'static str) -> Request<'static> {
        Request {
            method,
            path,
            authority: "lead.example.invalid:9448",
            forwarded_login: Some("owner"),
            client_token: None,
            token_in_query: false,
            denied: Default::default(),
        }
    }

    #[test]
    fn client_token_compares_whole_value() {
        assert!(client_token_ok("abc", Some("abc")));
        assert!(!client_token_ok("abc", Some("abd")));
        assert!(!client_token_ok("abc", Some("abcd")));
        assert!(!client_token_ok("abc", Some("ab")));
        assert!(!client_token_ok("abc", None));
        assert!(!client_token_ok("", Some("")));
    }

    #[test]
    fn approval_post_requires_enabled_and_owner_chat() {
        let peer: IpAddr = Ipv4Addr::LOCALHOST.into();
        let post = request("POST", "/approvals");
        assert_eq!(
            approval_post(&config(false, true), peer, &post, &Owner),
            Err(Refusal::Disabled)
        );
        assert_eq!(
            approval_post(&config(true, false), peer, &post, &Owner),
            Err(Refusal::ReadOnly)
        );
        assert_eq!(approval_post(&config(true, true), peer, &post, &Owner), Ok(()));
        assert_eq!(
            approval_post(&config(true, true), peer, &post, &Unverified),
            Err(Refusal::Unauthenticated)
        );
        let other_login = Request {
            forwarded_login: Some("guest"),
            ..request("POST", "/approvals")
        };
        assert_eq!(
            approval_post(&config(true, true), peer, &other_login, &Owner),
            Err(Refusal::NotAllowed)
        );
    }

    #[test]
    fn approval_post_refuses_get_and_other_paths() {
        let peer: IpAddr = Ipv4Addr::LOCALHOST.into();
        let config = config(true, true);
        for (method, path) in [
            ("GET", "/approvals"),
            ("PUT", "/approvals"),
            ("POST", "/chat"),
            ("POST", "/approvals?x=1"),
            ("POST", "/"),
        ] {
            assert_eq!(
                approval_post(&config, peer, &request(method, path), &Owner),
                Err(Refusal::ReadOnly),
                "{method} {path}"
            );
        }
        // A remote peer is refused by the same observation as chat, never answered.
        let remote: IpAddr = std::net::Ipv4Addr::new(192, 0, 2, 7).into();
        assert_eq!(
            approval_post(&config, remote, &request("POST", "/approvals"), &Owner),
            Err(Refusal::LocalOnly)
        );
    }
}
