//! Opt-in landing dispatch through the canonical publisher under the owner journal lock.
use super::{append, open_log, Admissions, Board, File, Sample};
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
}

pub enum Approved {
    Notice {
        intent: NoticeIntent,
        failed_tests: Vec<String>,
    },
    Retry {
        intent: RetryIntent,
        /// Count established by the existing Reader and owner journal.
        prior_admissions: u8,
    },
    Reconcile(AdmissionAcknowledgment),
}

pub struct Authorization {
    pub number: u64,
    pub operation: Approved,
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

/// The existing Reader supplies the fresh board and authorization after the
/// journal lock is acquired. Reconciliation reads the precise acknowledged
/// entry with that Reader. With the default policy neither callback is used.
pub fn run_once(
    policy: &Policy,
    journal_path: &Path,
    repo: &Path,
    run: &RunPolicy,
    gh_binary: &Path,
    read: impl FnOnce(&Publications) -> Result<(Board, Option<Authorization>)>,
    read_acknowledged: impl FnOnce(&AdmissionAcknowledgment) -> Result<Value>,
) -> Result<()> {
    if !policy.enabled || run.offline {
        return Ok(());
    }
    let mut log = open_log(journal_path)?;
    let history = Admissions::load(&mut log)?;
    let (board, action) = read(&history.publications)?;
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
    let Some(entry) = board
        .entries
        .iter()
        .find(|entry| entry.number == number && entry.head == rejection.pr_head)
    else {
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
            && entry.action == crate::board::Action::Ready
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
    use crate::board::{Action, Entry};

    #[test]
    fn ambiguous_delivery_reservation_survives_reopening_and_refuses_redispatch() {
        let temp = crate::test_dir::PrivateDir::new("landing-publication");
        let path = temp.path().join("owner.jsonl");
        let intent = NoticeIntent {
            rejection: RejectedLanding {
                repository_id: "R_repo".into(),
                queue_id: "MQ_main".into(),
                pull_request_id: "PR_pull".into(),
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
