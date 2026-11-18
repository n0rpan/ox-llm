//! Opt-in landing dispatch through the canonical publisher under the owner journal lock.
use super::{append, open_log, Admissions, Board, File, LandingSample, Sample};
use crate::board::{landing, queue::RejectionEvidence, Action};
use crate::{
    Admission, AdmissionAcknowledgment, AdmissionReceipt, DeliveryReceipt, LandingPublisher, NoticeIntent,
    RejectedLanding, Result, RetryIntent, RunPolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

#[derive(Default)]
pub struct Policy {
    pub enabled: bool,
    pub publisher_id: String,
}

enum Approved {
    Notice {
        intent: NoticeIntent,
        failed_tests: Vec<String>,
    },
    Retry {
        intent: RetryIntent,
        /// A conservative admission count from the existing Reader.
        prior_admissions: u8,
    },
    Reconcile(AdmissionAcknowledgment),
}

struct Authorization {
    number: u64,
    operation: Approved,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "step", content = "evidence", rename_all = "snake_case")]
pub(super) enum Publication {
    NoticeReserved(NoticeIntent),
    Delivered(DeliveryReceipt),
    RetryReserved(RetryIntent),
    Acknowledged(AdmissionAcknowledgment),
    Admitted(AdmissionReceipt),
}

impl Publication {
    fn rejection(&self) -> &RejectedLanding {
        match self {
            Self::NoticeReserved(intent) => &intent.rejection,
            Self::Delivered(receipt) => &receipt.intent.rejection,
            Self::RetryReserved(intent) => &intent.delivery.intent.rejection,
            Self::Acknowledged(receipt) => &receipt.intent.delivery.intent.rejection,
            Self::Admitted(receipt) => &receipt.acknowledgment.intent.delivery.intent.rejection,
        }
    }

    fn retry(&self) -> bool {
        matches!(
            self,
            Self::RetryReserved(_) | Self::Acknowledged(_) | Self::Admitted(_)
        )
    }

    fn follows(&self, previous: Option<&Self>) -> bool {
        match (previous, self) {
            (None, Self::NoticeReserved(_) | Self::RetryReserved(_)) => true,
            (Some(Self::NoticeReserved(intent)), Self::Delivered(receipt)) => intent == &receipt.intent,
            (Some(Self::RetryReserved(intent)), Self::Acknowledged(receipt)) => intent == &receipt.intent,
            (Some(Self::RetryReserved(intent)), Self::Admitted(receipt)) => {
                intent == &receipt.acknowledgment.intent
            }
            (Some(Self::Acknowledged(ack)), Self::Admitted(receipt)) => ack == &receipt.acknowledgment,
            _ => false,
        }
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct PublicationKey {
    repository: String,
    pull_request: String,
    head: String,
    removal: Option<String>,
}

#[derive(Default)]
pub struct Publications {
    latest: BTreeMap<PublicationKey, Publication>,
}

impl Publications {
    fn key(rejection: &RejectedLanding, retry: bool) -> PublicationKey {
        PublicationKey {
            repository: rejection.repository_id.clone(),
            pull_request: rejection.pull_request_id.clone(),
            head: rejection.pr_head.clone(),
            removal: (!retry).then(|| rejection.removal_id.clone()),
        }
    }

    pub(super) fn observe(&mut self, record: Publication) -> Result<()> {
        let rejection = record.rejection();
        let key = Self::key(rejection, record.retry());
        if !record.follows(self.latest.get(&key)) {
            return Err("invalid landing publication journal transition".into());
        }
        if let Publication::RetryReserved(intent) = &record {
            if !matches!(
                self.latest.get(&Self::key(rejection, false)),
                Some(Publication::Delivered(receipt)) if *receipt == intent.delivery
            ) {
                return Err("landing retry requires a journaled delivery receipt".into());
            }
        }
        self.latest.insert(key, record);
        Ok(())
    }

    pub fn delivered(&self, rejection: &RejectedLanding) -> Option<&DeliveryReceipt> {
        match self.prior(rejection, false) {
            Some(Publication::Delivered(receipt)) if &receipt.intent.rejection == rejection => Some(receipt),
            _ => None,
        }
    }

    pub fn pending_admission(&self, rejection: &RejectedLanding) -> Option<&AdmissionAcknowledgment> {
        match self.prior(rejection, true) {
            Some(Publication::Acknowledged(receipt))
                if &receipt.intent.delivery.intent.rejection == rejection =>
            {
                Some(receipt)
            }
            _ => None,
        }
    }

    pub fn attempted_retry(&self, rejection: &RejectedLanding) -> bool {
        self.prior(rejection, true).is_some()
    }

    pub fn admitted(&self, rejection: &RejectedLanding) -> Option<&AdmissionReceipt> {
        match self.prior(rejection, true) {
            Some(Publication::Admitted(receipt)) => Some(receipt),
            _ => None,
        }
    }

    fn prior(&self, rejection: &RejectedLanding, retry: bool) -> Option<&Publication> {
        self.latest.get(&Self::key(rejection, retry))
    }
}

fn record(log: &mut File, board: &Board, publication: &Publication) -> Result<()> {
    append(
        log,
        &Sample {
            schema_version: 1,
            observed_at: board.observed_at,
            board,
            would_take: Vec::new(),
            reviews: &BTreeMap::new(),
            queue_rejections: &[],
            matched_queue_admissions: &BTreeMap::new(),
            landing: &BTreeMap::new(),
            landing_main: None,
            landing_publication: Some(publication),
        },
    )
}

/// Apply one fresh Reader snapshot under the existing owner-journal lock.
pub fn run_once(
    policy: &Policy,
    journal_path: &Path,
    repo: &Path,
    run: &RunPolicy,
    gh_binary: &Path,
    read: impl FnOnce() -> Result<LandingSample>,
    read_acknowledged: impl FnOnce(&AdmissionAcknowledgment) -> Result<Value>,
) -> Result<()> {
    if !policy.enabled || run.offline {
        return Ok(());
    }
    let mut log = open_log(journal_path)?;
    if !crate::board::queue::is_node_id(&policy.publisher_id) {
        return Err("landing policy requires the intended publisher identity".into());
    }
    let mut history = Admissions::load(&mut log)?;
    let sample = read()?;
    let action = authorize(policy, &mut history, &sample);
    let board = sample.board.clone();
    let mut sample = Some(sample);
    super::run_with_landing(
        &mut log,
        true,
        || sample.take().ok_or_else(|| "snapshot already written".into()),
        |_| {},
    )?;
    advance(&mut log, &history.publications, &board, action, |file, action| {
        let publisher = LandingPublisher::new(repo, run, gh_binary, file);
        match action {
            Approved::Notice { intent, failed_tests } => {
                let names: Vec<_> = failed_tests.iter().map(String::as_str).collect();
                Ok(Publication::Delivered(publisher.notice(&intent, &names)?))
            }
            Approved::Retry { intent, .. } => match publisher.enqueue(&intent)? {
                Admission::Pending(ack) => Ok(Publication::Acknowledged(ack)),
                Admission::Confirmed(receipt) => Ok(Publication::Admitted(receipt)),
            },
            Approved::Reconcile(ack) => match ack.reconcile(&read_acknowledged(&ack)?)? {
                Admission::Pending(ack) => Ok(Publication::Acknowledged(ack)),
                Admission::Confirmed(receipt) => Ok(Publication::Admitted(receipt)),
            },
        }
    })
}

fn authorize(policy: &Policy, history: &mut Admissions, sample: &LandingSample) -> Option<Authorization> {
    let main = sample.landing_main.as_deref().unwrap_or_default();
    let matched = history.matches(
        &sample.board,
        sample.landing_main.as_deref(),
        sample
            .queue_rejections
            .iter()
            .map(RejectionEvidence::admission_identity),
    );
    sample
        .queue_rejections
        .iter()
        .find_map(|rejection| {
            let number = rejection.admission_identity().number;
            let decision = sample.landing.get(&number)?;
            if decision.queue.is_some() {
                return None;
            }
            let admission = matched.get(&number)?;
            let entry = sample.board.entries.iter().find(|entry| entry.number == number)?;
            let target = rejection.notification_target()?;
            let (binding, prior_admissions) = rejection.publication_binding()?;
            let intent = NoticeIntent {
                rejection: binding,
                recipient_id: target.author_id.clone(),
                recipient_login: target.login.clone(),
                publisher_id: policy.publisher_id.clone(),
            };
            let tests = rejection.reported_failure_names();
            let known = history
                .publications
                .delivered(&intent.rejection)
                .filter(|receipt| receipt.intent == intent);
            let consumed = history.publications.attempted_retry(&intent.rejection);
            let prior_admissions = prior_admissions.filter(|n| (1..3).contains(n));
            let retry = if !consumed && prior_admissions.is_some() {
                landing::Retry::Unused {
                    last_admission_at: admission.last_admission_at(),
                }
            } else {
                landing::Retry::Unknown
            };
            let plan = landing::Policy { enabled: true }.plan(
                entry,
                decision.latest_reviews,
                landing::Queue::Rejected(landing::Rejection {
                    head: &intent.rejection.pr_head,
                    main: admission.baseline_main(),
                    merge_group_head: &intent.rejection.rejected_group,
                    removal_event: &intent.rejection.removal_id,
                    cause: if tests.is_empty() {
                        landing::RejectionCause::Unknown
                    } else {
                        landing::RejectionCause::FailedTests(&tests)
                    },
                    notified_event: known.map(|receipt| receipt.intent.rejection.removal_id.as_str()),
                    retry,
                }),
                main,
                sample.board.observed_at,
            );
            if let (landing::Next::Retry, Some(delivery), Some(prior_admissions)) =
                (plan.next, known, prior_admissions)
            {
                return Some(Authorization {
                    number,
                    operation: Approved::Retry {
                        intent: RetryIntent {
                            delivery: delivery.clone(),
                            observed_main: main.to_owned(),
                            observed_at: sample.board.observed_at,
                            last_admission_at: admission.last_admission_at(),
                        },
                        prior_admissions,
                    },
                });
            }
            if plan.notify_rejection.is_some()
                && !tests.is_empty()
                && history.publications.prior(&intent.rejection, false).is_none()
            {
                return Some(Authorization {
                    number,
                    operation: Approved::Notice {
                        intent,
                        failed_tests: tests.into_iter().map(str::to_owned).collect(),
                    },
                });
            }
            None
        })
        .or_else(|| reconcile(&history.publications, sample))
}

fn reconcile(history: &Publications, sample: &LandingSample) -> Option<Authorization> {
    history.latest.values().find_map(|record| {
        let Publication::Acknowledged(ack) = record else {
            return None;
        };
        let rejection = &ack.intent.delivery.intent.rejection;
        let number = sample
            .board
            .entries
            .iter()
            .find(|entry| {
                entry.number == rejection.pull_request_number
                    && entry.head == rejection.pr_head
                    && sample
                        .landing
                        .get(&entry.number)
                        .and_then(|decision| decision.queue.as_ref())
                        .is_some_and(|queue| queue.enqueued_at == ack.enqueued_at)
            })?
            .number;
        Some(Authorization {
            number,
            operation: Approved::Reconcile(ack.clone()),
        })
    })
}

fn advance(
    log: &mut File,
    history: &Publications,
    board: &Board,
    action: Option<Authorization>,
    publish: impl FnOnce(&File, Approved) -> Result<Publication>,
) -> Result<()> {
    let Some(Authorization {
        number,
        operation: action,
    }) = action
    else {
        return Ok(());
    };
    let (rejection, retry) = match &action {
        Approved::Notice { intent, .. } => (&intent.rejection, false),
        Approved::Retry { intent, .. } => (&intent.delivery.intent.rejection, true),
        Approved::Reconcile(ack) => (&ack.intent.delivery.intent.rejection, true),
    };
    let Some(entry) = board.entries.iter().find(|entry| {
        entry.number == number
            && entry.number == rejection.pull_request_number
            && entry.head == rejection.pr_head
    }) else {
        return Ok(());
    };
    let previous = history.prior(rejection, retry);
    let reservation = match &action {
        Approved::Notice { intent, .. } if previous.is_none() => Publication::NoticeReserved(intent.clone()),
        Approved::Retry {
            intent,
            prior_admissions,
        } if previous.is_none()
            && (1..3).contains(prior_admissions)
            && entry.action == Action::Ready
            && entry.eligible
            && board.observed_at == intent.observed_at
            && matches!(
                history.prior(rejection, false),
                Some(Publication::Delivered(receipt)) if *receipt == intent.delivery
            ) =>
        {
            Publication::RetryReserved(intent.clone())
        }
        Approved::Reconcile(ack) if matches!(previous, Some(Publication::Acknowledged(previous)) if ack == previous) =>
        {
            let receipt = publish(log, action)?;
            return match receipt {
                Publication::Acknowledged(_) => Ok(()),
                Publication::Admitted(_) if receipt.follows(previous) => record(log, board, &receipt),
                _ => Err("invalid landing reconciliation result".into()),
            };
        }
        _ => return Ok(()),
    };
    record(log, board, &reservation)?;
    let receipt = publish(log, action)?;
    if !receipt.follows(Some(&reservation)) {
        return Err("invalid landing publication result".into());
    }
    record(log, board, &receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::Entry;

    #[test]
    fn default_off_does_not_open_the_journal_or_call_the_reader() {
        let temp = crate::test_dir::PrivateDir::new("landing-off");
        let path = temp.path().join("owner.jsonl");
        let run = RunPolicy {
            git_binary: temp.path().join("git"),
            host_path: Default::default(),
            terminate_binary: None,
            offline: false,
            runner_owns_github: false,
            storage: temp.path().to_owned(),
        };
        run_once(
            &Policy::default(),
            &path,
            temp.path(),
            &run,
            &temp.path().join("gh"),
            || panic!("default off cannot read a board"),
            |_| panic!("default off cannot reconcile"),
        )
        .unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn ambiguous_delivery_reservation_survives_reopening_and_refuses_redispatch() {
        let temp = crate::test_dir::PrivateDir::new("landing-publication");
        let path = temp.path().join("owner.jsonl");
        let intent = NoticeIntent {
            rejection: RejectedLanding {
                repository_id: "R_repo".into(),
                queue_id: "MQ_main".into(),
                pull_request_id: "PR_pull".into(),
                pull_request_number: 42,
                pr_head: "a".repeat(40),
                rejected_group: "b".repeat(40),
                removal_id: "E_removed".into(),
                removed_at: 2,
            },
            recipient_id: "U_author".into(),
            recipient_login: "synthetic-author".into(),
            publisher_id: "U_publisher".into(),
        };
        let mut board = Board {
            observed_at: 3,
            entries: vec![Entry {
                number: 42,
                head: "c".repeat(40),
                release_pinned: false,
                action: Action::Ready,
                lease: None,
                attempts_today: 0,
                eligible: true,
            }],
        };
        let authorization = |intent: NoticeIntent| {
            Some(Authorization {
                number: 42,
                operation: Approved::Notice {
                    intent,
                    failed_tests: vec!["synthetic test".into()],
                },
            })
        };
        {
            let mut file = open_log(&path).unwrap();
            let history = Admissions::load(&mut file).unwrap();
            advance(
                &mut file,
                &history.publications,
                &board,
                authorization(intent.clone()),
                |_, _| panic!("wrong head must not dispatch"),
            )
            .unwrap();
            board.entries[0].head = intent.rejection.pr_head.clone();
            assert!(advance(
                &mut file,
                &history.publications,
                &board,
                authorization(intent.clone()),
                |_, _| Err("transport disconnected after dispatch".into())
            )
            .is_err());
        }
        let mut file = open_log(&path).unwrap();
        let history = Admissions::load(&mut file).unwrap();
        assert!(history.publications.delivered(&intent.rejection).is_none());
        let mut moved_recipient = intent;
        moved_recipient.recipient_id = "U_replacement".into();
        advance(
            &mut file,
            &history.publications,
            &board,
            authorization(moved_recipient),
            |_, _| panic!("ambiguous delivery must not dispatch again"),
        )
        .unwrap();
    }
}
