//! Recovery policy for contradictory required check observations.

/// A single check and its exact, independently verified Actions job attempt.
pub struct Observation<'a> {
    pub required: bool,
    pub status: &'a str,
    pub conclusion: Option<&'a str>,
    pub completed_at: Option<&'a str>,
    pub job_id: u64,
    pub run_attempt: u64,
}

/// The caller scopes durable history to the repository, exact head, workflow
/// run and logical job. A missing or unreadable journal is unknown.
#[derive(Clone, Copy)]
pub enum History {
    Unknown,
    Unused,
    Reserved,
    Requested { before_attempt: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Quiet,
    WaitForReceipt,
    RerunJob(u64),
    Alarm,
}

pub fn decide(_enabled: bool, _check: &Observation<'_>, _now: u64, _history: History) -> Decision {
    Decision::Quiet
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check() -> Observation<'static> {
        Observation {
            required: true,
            status: "in_progress",
            conclusion: Some("success"),
            completed_at: Some("1970-01-02T00:00:00Z"),
            job_id: 23,
            run_attempt: 2,
        }
    }

    #[test]
    fn only_a_required_in_progress_check_with_completion_evidence_is_eligible() {
        let check = check();
        assert_eq!(decide(false, &check, 87_001, History::Unused), Decision::Quiet);
        for other in [
            Observation {
                required: false,
                ..check
            },
            Observation {
                status: "completed",
                ..check
            },
            Observation {
                status: "queued",
                ..check
            },
            Observation {
                conclusion: None,
                ..check
            },
            Observation {
                conclusion: Some(""),
                ..check
            },
            Observation {
                completed_at: None,
                ..check
            },
            Observation {
                completed_at: Some("bad date"),
                ..check
            },
            Observation {
                completed_at: Some("1970-01-03T00:00:00Z"),
                ..check
            },
            Observation { job_id: 0, ..check },
            Observation {
                run_attempt: 0,
                ..check
            },
        ] {
            assert_eq!(decide(true, &other, 87_001, History::Unused), Decision::Quiet);
        }
        assert_eq!(decide(true, &check, 87_000, History::Unused), Decision::Quiet);
        assert_eq!(
            decide(true, &check, 87_001, History::Unused),
            Decision::RerunJob(23)
        );
    }

    #[test]
    fn stale_receipts_and_ambiguous_writes_cannot_authorize_another_rerun() {
        let check = check();
        for history in [
            History::Unknown,
            History::Reserved,
            History::Requested { before_attempt: 2 },
        ] {
            assert_eq!(decide(true, &check, 87_001, history), Decision::WaitForReceipt);
        }
        assert_eq!(
            decide(true, &check, 87_001, History::Requested { before_attempt: 1 }),
            Decision::Alarm
        );
        assert_eq!(
            decide(true, &check, 87_000, History::Requested { before_attempt: 1 }),
            Decision::Quiet
        );
    }
}
