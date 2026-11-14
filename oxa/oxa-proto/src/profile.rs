/// `oxa.profile`: which formats and defaults oxa uses where pi specifies one and our engine and
/// models do better with another (OXA-LOCAL-FIRST sec. 6). A profile never adds or removes a
/// capability: `local` changes how a capability is declared or defaulted, nothing else.
///
/// Read once per prefix epoch (W3) and stored with the epoch: a resumed session keeps the
/// profile its stored prefix was built with; a change takes effect at the next new epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Profile {
    /// pi 1.5.2's texts and behaviour (the PARITY reference).
    Pi,
    /// Tuned to the local engine's tool-call format and cache.
    #[default]
    Local,
}

impl Profile {
    /// The default when neither `OXA_PROFILE` nor a settings layer names one. The owner decides
    /// the default; this constant is the only place it is spelled.
    pub const DEFAULT: Profile = Profile::Local;
    pub const ALL: [Profile; 2] = [Profile::Pi, Profile::Local];
    /// The settings key.
    pub const SETTING: &'static str = "oxa.profile";
    /// The environment override (wins over every settings layer).
    pub const ENV: &'static str = "OXA_PROFILE";

    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Pi => "pi",
            Profile::Local => "local",
        }
    }
    /// Exact, lowercase names only (`pi`, `local`).
    pub fn parse(s: &str) -> Option<Profile> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }
    /// The diagnostic text for a value that is neither name.
    pub fn invalid_text(value: &str) -> String {
        format!("unknown profile {value:?}: expected \"pi\" or \"local\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_reject_others() {
        for p in Profile::ALL {
            assert_eq!(Profile::parse(p.as_str()), Some(p));
        }
        assert_eq!(Profile::parse("Local"), None);
        assert_eq!(Profile::parse(""), None);
        assert_eq!(Profile::default(), Profile::DEFAULT);
        assert!(Profile::invalid_text("x").contains("\"pi\" or \"local\""));
    }
}
