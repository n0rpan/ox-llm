//! Owner key commands. No provider, listener or credential discovery is involved.
use super::keys::{Limits, Store};
use serde_json::{json, Value};
use std::{fmt, io::Write, path::Path};

#[derive(Debug)]
pub enum Error {
    Usage,
    Storage(super::keys::Error),
    Output,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => f.write_str("usage: oxa share key add NAME --daily-requests N --daily-tokens N | key list | key revoke ID"),
            Self::Storage(error) => error.fmt(f),
            Self::Output => f.write_str("Cannot write guest key output; use key list to inspect or revoke the created id."),
        }
    }
}
impl std::error::Error for Error {}
impl From<super::keys::Error> for Error {
    fn from(error: super::keys::Error) -> Self {
        Self::Storage(error)
    }
}

/// Only `add` prints a bearer, once on stdout after durable creation. Arguments,
/// errors, listing and revocation never contain a bearer or stored digest.
pub fn run(words: &[String], directory: &Path, out: &mut dyn Write) -> Result<(), Error> {
    let output = match words.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["key", "add", name, options @ ..] => {
            let mut requests = None;
            let mut tokens = None;
            if options.len() != 4 {
                return Err(Error::Usage);
            }
            for pair in options.as_chunks::<2>().0 {
                let slot = match pair[0] {
                    "--daily-requests" => &mut requests,
                    "--daily-tokens" => &mut tokens,
                    _ => return Err(Error::Usage),
                };
                let value = pair[1]
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or(Error::Usage)?;
                if slot.replace(value).is_some() {
                    return Err(Error::Usage);
                }
            }
            let limits = Limits {
                daily_requests: requests.ok_or(Error::Usage)?,
                daily_tokens: tokens.ok_or(Error::Usage)?,
            };
            let key = Store::initialize(directory)?.create(name, limits)?;
            let mut value = metadata(key.info);
            value["api_key"] = json!(key.bearer.expose());
            value
        }
        ["key", "list"] => Value::Array(
            Store::open(directory)?
                .list()?
                .into_iter()
                .map(metadata)
                .collect(),
        ),
        ["key", "revoke", id] => {
            let revoked = Store::open(directory)?.revoke(id)?;
            json!({"id":id,"revoked":revoked})
        }
        _ => return Err(Error::Usage),
    };
    serde_json::to_writer(&mut *out, &output).map_err(|_| Error::Output)?;
    writeln!(out).map_err(|_| Error::Output)
}

fn metadata(key: super::keys::KeyInfo) -> Value {
    json!({"id":key.id,"name":key.name,"created_at":key.created_at,"revoked":key.revoked,
        "daily_requests":key.limits.daily_requests,"daily_tokens":key.limits.daily_tokens})
}
