//! Owner-selected sampling bounds; no policy starts work or grants action credit.
use super::*;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityTiming {
    pub heartbeat_interval_secs: u64,
    pub silence_secs: u64,
}

impl ActivityTiming {
    pub fn validate(&self) -> Result<()> {
        if self.heartbeat_interval_secs == 0
            || self.silence_secs == 0
            || self.silence_secs.checked_mul(1000).is_none()
        {
            return Err("positive representable activity timing required".into());
        }
        Ok(())
    }

    /// Assess already-due work first. Past deadlines never create a busy wake loop.
    /// A future dependency deadline can shorten sampling; only verified evidence can resume.
    pub fn next_wake(&self, now: u64, dependency_deadline: Option<u64>) -> Result<u64> {
        self.validate()?;
        let next = now
            .checked_add(self.heartbeat_interval_secs)
            .ok_or("activity wake overflow")?;
        Ok(dependency_deadline
            .filter(|deadline| *deadline > now)
            .map_or(next, |deadline| next.min(deadline)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_wake_observes_deadlines_without_inventing_progress_or_spinning() {
        let timing = ActivityTiming {
            heartbeat_interval_secs: 15,
            silence_secs: 30,
        };
        assert_eq!(timing.next_wake(100, None).unwrap(), 115);
        assert_eq!(timing.next_wake(100, Some(108)).unwrap(), 108);
        assert_eq!(timing.next_wake(100, Some(90)).unwrap(), 115);
        assert!(timing.next_wake(u64::MAX, None).is_err());
        assert!(ActivityTiming {
            heartbeat_interval_secs: 0,
            ..timing
        }
        .validate()
        .is_err());
        assert!(ActivityTiming {
            silence_secs: 0,
            ..timing
        }
        .validate()
        .is_err());
        assert!(ActivityTiming {
            silence_secs: u64::MAX,
            ..timing
        }
        .validate()
        .is_err());
    }
}
