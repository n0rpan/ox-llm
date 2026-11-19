//! The shuffle bag and the key fallbacks (animations design 3.2, steps 2 to 5).
//!
//! A pick depends only on the session id, the pool key, the members and the number of earlier
//! refills, so the same inputs always give the same sequence. Nothing here reads a clock.

use std::collections::{HashMap, VecDeque};

/// The event key and its fallbacks, most specific first. `tool:bash` falls back to `tool`,
/// `wait:ci` to `wait` then `tool`, `ci:green_after_red` to `ci:green` then `done`. Any other
/// `a:b` falls back to `a`; a key without `:` has no fallback.
pub fn key_chain(key: &str) -> Vec<String> {
    let mut chain = vec![key.to_string()];
    let table: Option<&[&str]> = match key {
        "ci:green_after_red" => Some(&["ci:green", "done"][..]),
        "wait:ci" => Some(&["wait", "tool"][..]),
        _ => None,
    };
    match table {
        Some(parents) => chain.extend(parents.iter().map(|p| p.to_string())),
        None => {
            if let Some((parent, _)) = key.split_once(':') {
                chain.push(parent.to_string());
            }
        }
    }
    chain
}

/// The first key of [`key_chain`] whose pool has members (step 2), if any.
pub fn first_with_members(key: &str, has_members: impl Fn(&str) -> bool) -> Option<String> {
    key_chain(key).into_iter().find(|k| has_members(k.as_str()))
}

/// One shuffle bag per pool (step 3). `members` pairs each item with its weight, the number of
/// copies in the bag. The bag is refilled from `members` only when it is empty, so a caller that
/// changes a pool's members must start a new bag for that key.
///
/// Each refill plays every weighted copy. Consecutive picks differ, including at refill
/// boundaries, unless the remaining weights leave no alternative.
#[derive(Debug, Clone)]
pub struct Bag<T> {
    session: String,
    key: String,
    refills: u64,
    queue: VecDeque<T>,
    last: Option<T>,
}

impl<T: Clone + PartialEq> Bag<T> {
    /// An empty bag for one pool of one session.
    pub fn new(session: &str, key: &str) -> Self {
        Bag {
            session: session.to_string(),
            key: key.to_string(),
            refills: 0,
            queue: VecDeque::new(),
            last: None,
        }
    }

    /// The next item of the pool, or `None` when the pool has no copies at all.
    pub fn next(&mut self, members: &[(T, u32)]) -> Option<T> {
        if self.queue.is_empty() {
            self.refill(members);
        }
        let pick = self.queue.pop_front()?;
        self.last = Some(pick.clone());
        Some(pick)
    }

    fn refill(&mut self, members: &[(T, u32)]) {
        let mut counts: Vec<(&T, u64)> = Vec::new();
        for (item, weight) in members {
            if let Some((_, count)) = counts.iter_mut().find(|(known, _)| *known == item) {
                *count += u64::from(*weight);
            } else {
                counts.push((item, u64::from(*weight)));
            }
        }
        let mut rng = XorShift::new(seed(&self.session, &self.key, self.refills));
        shuffle(&mut counts, &mut rng);
        self.refills += 1;
        let mut previous = counts
            .iter()
            .position(|(item, _)| Some(*item) == self.last.as_ref());
        // Spend the largest remaining weight that differs from the last pick. This
        // keeps enough lighter copies to separate heavier ones. The shuffle breaks ties.
        while let Some((index, (item, count))) = counts
            .iter_mut()
            .enumerate()
            .filter(|(_, (_, count))| *count > 0)
            .max_by_key(|(index, (_, count))| (Some(*index) != previous, *count))
        {
            self.queue.push_back((*item).clone());
            *count -= 1;
            previous = Some(index);
        }
    }
}

/// The pick kept for one (run, key) (step 4). A pick is reused until the caller says its time
/// budget is spent; the clock stays with the caller.
#[derive(Debug)]
pub struct Sticky<T> {
    picks: HashMap<(u64, String), T>,
}

impl<T> Default for Sticky<T> {
    fn default() -> Self {
        Sticky {
            picks: HashMap::new(),
        }
    }
}

impl<T: Clone> Sticky<T> {
    /// The kept pick for `run` and `key`, or a fresh one from `pick` when none is kept or
    /// `spent` is true. The fresh pick is kept in its place.
    pub fn hold(&mut self, run: u64, key: &str, spent: bool, pick: impl FnOnce() -> T) -> T {
        let slot = (run, key.to_string());
        if let Some(kept) = self.picks.get(&slot).filter(|_| !spent) {
            return kept.clone();
        }
        let fresh = pick();
        self.picks.insert(slot, fresh.clone());
        fresh
    }
}

/// xorshift64. The state is never zero.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        XorShift(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    fn step(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn shuffle<T>(items: &mut [T], rng: &mut XorShift) {
    for i in (1..items.len()).rev() {
        let j = (rng.step() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

/// FNV-1a over the session id, the pool key and the refill count.
fn seed(session: &str, key: &str, refill: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in session
        .bytes()
        .chain([0u8])
        .chain(key.bytes())
        .chain([0u8])
        .chain(refill.to_le_bytes())
    {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
