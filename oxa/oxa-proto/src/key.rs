use crate::{codes, OxaError};
use std::fmt;

/// A key identifier in pi's notation, normalised: lowercase, modifiers in the order
/// `ctrl+shift+alt+super`, then the key (`ctrl+shift+up`, `alt+m`, `enter`, `shift+tab`, `+`).
///
/// This is the vocabulary shared by `keybindings.json` (`oxa-config`), the terminal input
/// parser (`oxa-tui`) and extension shortcuts. Equality on the normalised string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyId(String);

const MODS: [&str; 4] = ["ctrl", "shift", "alt", "super"];

impl KeyId {
    /// Parse and normalise. Accepts modifiers in any order and any case; `meta`/`option` are
    /// aliases of `alt`, `cmd`/`win` of `super`, `control` of `ctrl`, `return` of `enter`,
    /// `esc` of `escape`. The key `+` is written `+` or `plus` (`ctrl++` = ctrl and plus).
    pub fn parse(s: &str) -> Result<KeyId, OxaError> {
        let bad = || OxaError::new(codes::INVALID, format!("invalid key id: {s:?}"));
        let lower = s.trim().to_lowercase();
        if lower.is_empty() {
            return Err(bad());
        }
        let (mods_part, key) = if lower == "+" {
            ("", "+".to_string())
        } else if let Some(rest) = lower.strip_suffix("++") {
            (rest, "+".to_string())
        } else {
            match lower.rsplit_once('+') {
                Some((m, k)) => (m, k.to_string()),
                None => ("", lower.clone()),
            }
        };
        let mods_part = mods_part.to_string();
        let mut have = [false; 4];
        for m in mods_part.split('+').filter(|m| !m.is_empty()) {
            let idx = match m {
                "ctrl" | "control" => 0,
                "shift" => 1,
                "alt" | "meta" | "option" => 2,
                "super" | "cmd" | "win" => 3,
                _ => return Err(bad()),
            };
            have[idx] = true;
        }
        let key = match key.as_str() {
            "" => return Err(bad()),
            "plus" => "+".to_string(),
            "return" => "enter".to_string(),
            "esc" => "escape".to_string(),
            k if MODS.contains(&k) => return Err(bad()),
            k => k.to_string(),
        };
        let mut out = String::new();
        for (i, m) in MODS.iter().enumerate() {
            if have[i] {
                out.push_str(m);
                out.push('+');
            }
        }
        out.push_str(&key);
        Ok(KeyId(out))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    /// The key without modifiers (`up` of `ctrl+shift+up`).
    pub fn key(&self) -> &str {
        if self.0.ends_with('+') {
            "+"
        } else {
            self.0.rsplit('+').next().unwrap_or(&self.0)
        }
    }
    pub fn has_mod(&self, m: &str) -> bool {
        let mods = &self.0[..self.0.len() - self.key().len()];
        mods.split('+').any(|x| x == m)
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
