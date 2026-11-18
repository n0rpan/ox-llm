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
) -> bool {
    config.enabled
        && (config.fixed_prompts || config.owner_chat)
        && request.method == "POST"
        && request.path == "/chat"
        && observe(
            config,
            peer,
            request.authority,
            request.forwarded_login,
            identity,
            || (),
        )
        .is_ok()
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
