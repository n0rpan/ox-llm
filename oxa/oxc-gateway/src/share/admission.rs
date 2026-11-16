//! Durable guest request/token admission using the key store's lock and private I/O.
//! Unknown outcomes retain their full reservation, including across process exit.
use super::keys::{Error, KeyInfo, Store};
use serde_json::{json, Map, Value};
use std::fmt;

const MAX_HOLDS: usize = 128;

/// Retain until trusted terminal usage. Dropping this guard never refunds work.
#[must_use = "settle trusted usage; dropping retains the conservative charge"]
pub struct Admission<'a> {
    store: &'a Store,
    key: KeyInfo,
    day: u64,
    nonce: String,
    reserved: u64,
}
impl fmt::Debug for Admission<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuestAdmission")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}
impl Admission<'_> {
    pub fn key(&self) -> &KeyInfo {
        &self.key
    }

    /// A request is always charged once; only unused reserved tokens are returned.
    /// This consumes the settlement authority, preventing duplicate refunds.
    pub fn settle(self, tokens: u64) -> Result<(), Error> {
        if tokens > self.reserved {
            return Err(Error::Invalid);
        }
        let _lock = self.store.try_lock()?;
        let mut state = State::read(self.store, self.day)?;
        let hold = state.holds.remove(&self.nonce).ok_or(Error::Corrupt)?;
        if hold != json!([self.key.id, self.reserved]) {
            return Err(Error::Corrupt);
        }
        let row = state.usage.get_mut(&self.key.id).ok_or(Error::Corrupt)?;
        let (requests, charged) = counts(row)?;
        let charged = charged
            .checked_sub(self.reserved - tokens)
            .ok_or(Error::Corrupt)?;
        *row = json!([requests, charged]);
        state.write(self.store, self.day)
    }
}

impl Store {
    /// `day` is the trusted UTC day number, never a guest request field. Reserve
    /// the complete possible input+output window, before any model work starts.
    /// Busy storage is an explicit refusal; the listener never waits on flock.
    pub fn admit(&self, bearer: &str, day: u64, tokens: u64) -> Result<Option<Admission<'_>>, Error> {
        if tokens == 0 {
            return Err(Error::Invalid);
        }
        let _lock = self.try_lock()?;
        let Some(key) = self.find_key(bearer)? else {
            return Ok(None);
        };
        let mut state = State::read(self, day)?;
        let (requests, charged) = state
            .usage
            .get(&key.id)
            .map(counts)
            .transpose()?
            .unwrap_or((0, 0));
        let requests = requests
            .checked_add(1)
            .filter(|n| *n <= key.limits.daily_requests)
            .ok_or(Error::RequestLimit)?;
        let charged = charged
            .checked_add(tokens)
            .filter(|n| *n <= key.limits.daily_tokens)
            .ok_or(Error::TokenLimit)?;
        if state.holds.len() >= MAX_HOLDS {
            return Err(Error::Full);
        }
        let mut random = [0; 16];
        getrandom::fill(&mut random).map_err(|_| Error::Unavailable)?;
        let nonce: String = random.iter().map(|b| format!("{b:02x}")).collect();
        if state.holds.contains_key(&nonce) {
            return Err(Error::Unavailable);
        }
        state.holds.insert(nonce.clone(), json!([key.id, tokens]));
        state.usage.insert(key.id.clone(), json!([requests, charged]));
        state.write(self, day)?;
        Ok(Some(Admission {
            store: self,
            key,
            day,
            nonce,
            reserved: tokens,
        }))
    }

    /// Bounded per-key usage for the owner status surface, not provider balances.
    pub fn guest_usage(&self, day: u64, key_id: &str) -> Result<(u64, u64), Error> {
        let _lock = self.try_lock()?;
        State::read(self, day)?
            .usage
            .get(key_id)
            .map(counts)
            .transpose()
            .map(|v| v.unwrap_or((0, 0)))
    }
}

struct State {
    usage: Map<String, Value>,
    holds: Map<String, Value>,
}
impl State {
    fn read(store: &Store, day: u64) -> Result<Self, Error> {
        let Some(bytes) = store.read_bytes(&filename(day))? else {
            return Ok(Self {
                usage: Map::new(),
                holds: Map::new(),
            });
        };
        let v: Value = serde_json::from_slice(&bytes).map_err(|_| Error::Corrupt)?;
        if v.as_object().ok_or(Error::Corrupt)?.len() != 3 || v["version"] != 1 {
            return Err(Error::Corrupt);
        }
        let usage = v["usage"].as_object().ok_or(Error::Corrupt)?.clone();
        let holds = v["holds"].as_object().ok_or(Error::Corrupt)?.clone();
        if usage.len() > 64 || holds.len() > MAX_HOLDS {
            return Err(Error::Corrupt);
        }
        for (id, row) in &usage {
            if !identifier(id) {
                return Err(Error::Corrupt);
            }
            counts(row)?;
        }
        let mut held: Map<String, Value> = Map::new();
        for (nonce, row) in &holds {
            if !identifier(nonce) || row.as_array().ok_or(Error::Corrupt)?.len() != 2 {
                return Err(Error::Corrupt);
            }
            let id = row[0].as_str().filter(|v| identifier(v)).ok_or(Error::Corrupt)?;
            let tokens = row[1].as_u64().filter(|n| *n > 0).ok_or(Error::Corrupt)?;
            let (requests, total) = held.get(id).map(counts).transpose()?.unwrap_or((0, 0));
            held.insert(
                id.into(),
                json!([
                    requests.checked_add(1).ok_or(Error::Corrupt)?,
                    total.checked_add(tokens).ok_or(Error::Corrupt)?
                ]),
            );
        }
        for (id, row) in held {
            let (requests, tokens) = counts(&row)?;
            let (charged_requests, charged_tokens) = counts(usage.get(&id).ok_or(Error::Corrupt)?)?;
            if requests > charged_requests || tokens > charged_tokens {
                return Err(Error::Corrupt);
            }
        }
        Ok(Self { usage, holds })
    }
    fn write(self, store: &Store, day: u64) -> Result<(), Error> {
        let bytes = serde_json::to_vec(&json!({"version":1,"usage":self.usage,"holds":self.holds}))
            .map_err(|_| Error::Corrupt)?;
        store.write_bytes(&filename(day), &bytes)
    }
}
fn filename(day: u64) -> String {
    format!("share-usage-{day}.json")
}
fn identifier(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn counts(row: &Value) -> Result<(u64, u64), Error> {
    if row.as_array().ok_or(Error::Corrupt)?.len() != 2 {
        return Err(Error::Corrupt);
    }
    Ok((
        row[0].as_u64().ok_or(Error::Corrupt)?,
        row[1].as_u64().ok_or(Error::Corrupt)?,
    ))
}
