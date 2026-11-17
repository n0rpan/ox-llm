//! Default-off lead access scaffold. No listener or production identity adapter is installed.
use oxa_config::{Dirs, Settings};
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;

pub const SETTING: &str = "oxa.lead.serve";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Local,
    Tailscale,
}

/// Identity settings are deliberately not Debug/Display output.
pub struct Config {
    enabled: bool,
    mode: Mode,
    bind: IpAddr,
    port: u16,
    allowed: Vec<String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: Mode::Local,
            bind: Ipv4Addr::LOCALHOST.into(),
            port: 8743,
            allowed: vec![],
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
        if values
            .keys()
            .any(|key| !["enabled", "identity", "bind", "port", "allowed_identities"].contains(&key.as_str()))
        {
            return Err("unknown lead serve setting");
        }
        let mut config = Self::default();
        if let Some(value) = values.get("enabled") {
            config.enabled = value.as_bool().ok_or("invalid lead serve enable flag")?;
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
}

/// Results from a trusted request-bound transport adapter, never from an HTTP header alone.
pub enum VerifiedIdentity<'a> {
    LocalOwner,
    TailscaleLogin(&'a str),
}
/// Future local adapters must verify OS ownership; proxy adapters must authenticate proxy
/// provenance and bind the login to this request. Loopback is necessary, not sufficient.
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
    if !peer.is_loopback() {
        return Err(Refusal::LocalOnly);
    }
    match (config.mode, identity.verified()) {
        (Mode::Local, Some(VerifiedIdentity::LocalOwner)) if forwarded_login.is_none() => Ok(read()),
        (Mode::Tailscale, Some(VerifiedIdentity::TailscaleLogin(login)))
            if forwarded_login == Some(login) && config.allowed.iter().any(|allowed| allowed == login) =>
        {
            Ok(read())
        }
        // A forwarded header alone never satisfies the requirement: without a verified identity
        // the request is unauthenticated, whichever mode is configured.
        (_, None) => Err(Refusal::Unauthenticated),
        _ => Err(Refusal::NotAllowed),
    }
}
