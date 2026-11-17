//! Explicit user-only proxy trust. No listener, daemon discovery or deployment changes.
use super::{Config, IdentityProvider, Mode, Refusal, VerifiedIdentity};
use serde_json::Value;
use std::io::Read;
use std::net::{Ipv6Addr, SocketAddr, TcpStream};

pub(super) struct Trust {
    uid: u32,
    authority: String,
}

impl Trust {
    pub(super) fn parse(value: Option<&Value>) -> Result<Option<Self>, &'static str> {
        let Some(value) = value else { return Ok(None) };
        let object = value.as_object().ok_or("invalid lead proxy settings")?;
        if object
            .keys()
            .any(|key| !["uid", "authority"].contains(&key.as_str()))
        {
            return Err("unknown lead proxy setting");
        }
        let uid = object
            .get("uid")
            .and_then(Value::as_u64)
            .and_then(|uid| u32::try_from(uid).ok())
            .filter(|uid| *uid != u32::MAX)
            .ok_or("invalid lead proxy principal")?;
        let authority = object
            .get("authority")
            .and_then(Value::as_str)
            .filter(|text| valid_authority(text))
            .ok_or("invalid lead proxy authority")?;
        Ok(Some(Self {
            uid,
            authority: authority.to_owned(),
        }))
    }
}

fn valid_authority(text: &str) -> bool {
    if text.is_empty() || text.len() > 255 || !text.is_ascii() {
        return false;
    }
    if let Ok(address) = text.parse::<SocketAddr>() {
        return address.port() != 0;
    }
    if let Some(ip) = text.strip_prefix('[').and_then(|ip| ip.strip_suffix(']')) {
        return ip.parse::<Ipv6Addr>().is_ok();
    }
    let host = if let Some((host, port)) = text.rsplit_once(':') {
        if port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || !port.parse::<u16>().is_ok_and(|port| port > 0)
        {
            return false;
        }
        host
    } else {
        text
    };
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// Call before binding. A configured proxy principal may assert identity for every process
/// with that UID; the user must dedicate that principal to trusted proxy workloads.
pub fn pre_bind(config: &Config) -> Result<(), &'static str> {
    if !config.enabled {
        return Err("lead serve is disabled");
    }
    if config.mode == Mode::Local {
        return Ok(());
    }
    let trust = config.proxy.as_ref().ok_or("trusted proxy is not configured")?;
    if !cfg!(target_os = "linux") {
        return Err("trusted proxy identity requires Linux");
    }
    // proc TCP ownership uses the kernel fallback UID for unmapped principals.
    // That value cannot identify one trusted proxy account in a user namespace.
    let mut fallback = String::new();
    std::fs::File::open("/proc/sys/kernel/overflowuid")
        .and_then(|file| file.take(33).read_to_string(&mut fallback))
        .map_err(|_| "proxy principal mapping unavailable")?;
    let reserved = fallback
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|_| fallback.len() <= 32)
        .ok_or("proxy principal mapping unavailable")?;
    if trust.uid == reserved {
        return Err("kernel fallback proxy principal is refused");
    }
    Ok(())
}

/// Proof borrows this request's configuration, stream and login lifetime. Its authority
/// comes exclusively from user configuration, never Host or another forwarded header.
pub struct Proxy<'a> {
    login: &'a str,
    authority: &'a str,
}

/// Use the actual accepted stream, after rejecting duplicate/ambiguous identity headers.
/// An error must refuse the request; it must never fall back to trusting its raw header.
pub fn verify<'a>(
    config: &'a Config,
    stream: &'a TcpStream,
    forwarded_login: Option<&'a str>,
) -> Result<Proxy<'a>, Refusal> {
    if !config.enabled {
        return Err(Refusal::Disabled);
    }
    if config.mode != Mode::Tailscale {
        return Err(Refusal::NotAllowed);
    }
    pre_bind(config).map_err(|_| Refusal::Unauthenticated)?;
    let trust = config.proxy.as_ref().ok_or(Refusal::Unauthenticated)?;
    let login = forwarded_login
        .filter(|login| !login.is_empty() && login.len() <= 128)
        .filter(|login| !login.chars().any(|ch| ch.is_whitespace() || ch.is_control()))
        .ok_or(Refusal::Unauthenticated)?;
    let uid = crate::lead_peer::owner_uid(stream).map_err(|_| Refusal::Unauthenticated)?;
    if uid != trust.uid {
        return Err(Refusal::Unauthenticated);
    }
    Ok(Proxy {
        login,
        authority: &trust.authority,
    })
}

impl IdentityProvider for Proxy<'_> {
    fn verified(&self) -> Option<VerifiedIdentity<'_>> {
        Some(VerifiedIdentity::TailscaleLogin(self.login, self.authority))
    }
}
