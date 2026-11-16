//! Owner-only live activity. No polling, persistence, request text or peer data.
use super::{keys::KeyInfo, listener::Failure};
use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
};

pub(super) const MAX_CONNECTIONS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyActivity {
    pub id: String,
    pub name: String,
    pub active_requests: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// Accepted connection tasks, including pending backend cleanup.
    pub connections: usize,
    /// Only keys that passed authentication and the optional peer check.
    pub keys: Vec<KeyActivity>,
}
#[derive(Default)]
struct State {
    connections: usize,
    keys: BTreeMap<String, (String, usize)>,
}
/// Clone before `serve`; the owner can inspect live activity while it runs.
/// This handle is never exposed through a guest HTTP route.
#[derive(Clone)]
pub struct Status(Arc<Mutex<State>>);
impl fmt::Debug for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GuestStatus([private])")
    }
}
impl Status {
    pub(super) fn new() -> Self {
        Self(Arc::new(Mutex::new(State::default())))
    }
    pub fn snapshot(&self) -> Result<Snapshot, Failure> {
        let state = self.0.lock().map_err(|_| Failure::Busy)?;
        Ok(Snapshot {
            connections: state.connections,
            keys: state
                .keys
                .iter()
                .map(|(id, (name, count))| KeyActivity {
                    id: id.clone(),
                    name: name.clone(),
                    active_requests: *count,
                })
                .collect(),
        })
    }
    pub(super) fn open(&self) -> Result<Activity, Failure> {
        let mut state = self.0.lock().map_err(|_| Failure::Busy)?;
        if state.connections >= MAX_CONNECTIONS {
            return Err(Failure::Busy);
        }
        state.connections += 1;
        Ok(Activity {
            status: self.clone(),
            key: None,
        })
    }
}

/// One guard owns both counters until the connection and backend cleanup end.
pub(super) struct Activity {
    status: Status,
    key: Option<String>,
}
impl Activity {
    pub(super) fn authorize(&mut self, key: &KeyInfo) -> Result<(), Failure> {
        if self.key.is_some() {
            return Err(Failure::Failed);
        }
        let mut state = self.status.0.lock().map_err(|_| Failure::Busy)?;
        if state.keys.len() >= MAX_CONNECTIONS && !state.keys.contains_key(&key.id) {
            return Err(Failure::Busy);
        }
        let row = state
            .keys
            .entry(key.id.clone())
            .or_insert_with(|| (key.name.clone(), 0));
        row.1 += 1;
        self.key = Some(key.id.clone());
        Ok(())
    }
}
impl Drop for Activity {
    fn drop(&mut self) {
        if let Ok(mut state) = self.status.0.lock() {
            state.connections -= 1;
            if let Some(id) = &self.key {
                if let Some(row) = state.keys.get_mut(id) {
                    row.1 -= 1;
                    if row.1 == 0 {
                        state.keys.remove(id);
                    }
                }
            }
        }
    }
}
