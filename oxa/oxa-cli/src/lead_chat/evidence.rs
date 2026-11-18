//! Bounded projection and reply references for one owner chat turn.
use super::quick_reply;
use crate::lead_page::Prompt;
use oxa_proto::{work::Snapshot, OxaError};
use oxa_work::board::{Action, Board};
use serde_json::{json, Value};

/// Stable reply-stage code of a citation the bounded input never offered (or
/// offered once and the reply repeats). Fixed text only: no reply or input bytes.
pub(super) const INVALID: &str = "lead_evidence_invalid";

const MAX_BOARD_FACTS: usize = 8;
const MAX_RUNNER_FACTS: usize = 5;

#[derive(Clone)]
pub(crate) struct FactChip {
    pub label: String,
    pub href: String,
    pub observed_at: u64,
    subject: Option<Subject>,
}

#[derive(Clone)]
enum Subject {
    Issue(u64),
    PullRequest(u64),
    Task(String),
}

/// Retain only renderable facts from the turn's original input.
pub(super) struct EvidencePacket {
    facts: Vec<FactChip>,
    board_truncated: bool,
    runners_truncated: bool,
}

impl EvidencePacket {
    pub(super) fn new(text: &str, board: Option<&Board>, situation: Option<&Snapshot>) -> Self {
        let mut facts = Vec::new();
        for prompt in [Prompt::Status, Prompt::Workers, Prompt::Attention] {
            if let Some(reply) = quick_reply(prompt, board, situation) {
                facts.push(FactChip {
                    label: reply.text,
                    href: reply.href.into(),
                    observed_at: reply.observed_at,
                    subject: None,
                });
            }
        }
        if let Some(board) = board {
            // Explicit item mentions should still work when the board has many rows.
            let mentioned: Vec<u64> = text
                .match_indices('#')
                .filter_map(|(at, _)| {
                    let tail = &text[at + 1..];
                    let end = tail.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(tail.len());
                    tail[..end].parse().ok()
                })
                .take(MAX_BOARD_FACTS)
                .collect();
            let entries = board.entries.iter();
            let named = entries.clone().filter(|entry| mentioned.contains(&entry.number));
            let other = entries.filter(|entry| !mentioned.contains(&entry.number));
            for entry in named.chain(other).take(MAX_BOARD_FACTS) {
                let (kind, subject) = match entry.action {
                    Action::BuildIssue | Action::WaitIssue => ("Issue", Subject::Issue(entry.number)),
                    _ => ("PR", Subject::PullRequest(entry.number)),
                };
                let action = json!(entry.action);
                let action = action.as_str().unwrap_or("action unavailable");
                facts.push(FactChip {
                    label: format!("{kind} #{} · {}", entry.number, action.replace('_', " ")),
                    href: format!("/items/{}", entry.number),
                    observed_at: board.observed_at,
                    subject: Some(subject),
                });
            }
        }
        if let Some(situation) = situation {
            for run in situation.running.iter().take(MAX_RUNNER_FACTS) {
                facts.push(FactChip {
                    label: format!("Runner observed task {} at {:?}", run.task, run.step),
                    href: "/workers".into(),
                    observed_at: situation.at_ms / 1000,
                    subject: Some(Subject::Task(run.task.clone())),
                });
            }
        }
        Self {
            facts,
            board_truncated: board.is_some_and(|board| board.entries.len() > MAX_BOARD_FACTS),
            runners_truncated: situation.is_some_and(|snapshot| snapshot.running.len() > MAX_RUNNER_FACTS),
        }
    }

    pub(super) fn context(&self) -> Value {
        let observations: Vec<_> = self
            .facts
            .iter()
            .enumerate()
            .map(|(index, fact)| {
                json!({"ref": index + 1, "fact": fact.label, "observed_at": fact.observed_at})
            })
            .collect();
        json!({
            "observations": observations,
            "board_rows_truncated": self.board_truncated,
            "running_rows_truncated": self.runners_truncated,
        })
    }

    pub(super) fn contains_task(&self, task: &str) -> bool {
        self.facts
            .iter()
            .any(|fact| matches!(&fact.subject, Some(Subject::Task(observed)) if observed == task))
    }

    pub(super) fn contains_item(&self, item: u64) -> bool {
        self.facts.iter().any(|fact| {
            matches!(&fact.subject, Some(Subject::Issue(number) | Subject::PullRequest(number)) if *number == item)
        })
    }

    pub(super) fn contains_pr(&self, pr: u64) -> bool {
        self.facts
            .iter()
            .any(|fact| matches!(&fact.subject, Some(Subject::PullRequest(number)) if *number == pr))
    }

    pub(super) fn resolve(&self, references: &[usize]) -> Result<Vec<FactChip>, OxaError> {
        let invalid = || OxaError::new(INVALID, "Lead reply supplied invalid evidence references");
        // The reply may cite every observation this bounded input actually
        // offered, each at most once: the offered set is itself bounded
        // (3 prompt facts + 8 board rows + 5 runner rows), so its size is the
        // only cap. A longer list is oversized before any entry is examined.
        if references.len() > self.facts.len() {
            return Err(invalid());
        }
        let mut seen = vec![false; self.facts.len()];
        references
            .iter()
            .map(|reference| {
                let index = reference.checked_sub(1).ok_or_else(invalid)?;
                let fact = self.facts.get(index).ok_or_else(invalid)?;
                if std::mem::replace(&mut seen[index], true) {
                    return Err(invalid());
                }
                Ok(fact.clone())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_work::board::Entry;

    fn board(rows: u64) -> Board {
        Board {
            observed_at: 1_700_000_000,
            entries: (1..=rows)
                .map(|number| Entry {
                    number,
                    head: "a".repeat(40),
                    release_pinned: false,
                    action: Action::WaitReview,
                    lease: None,
                    attempts_today: 0,
                    eligible: false,
                })
                .collect(),
        }
    }

    #[test]
    fn citations_cover_every_offered_observation_once() {
        let board = board(8);
        let packet = EvidencePacket::new("Summarise the board", Some(&board), None);
        // One status observation plus eight board rows are offered, so nine
        // unique citations are all inside the bounded input.
        let facts = packet
            .resolve(&[1, 2, 3, 4, 5, 6, 7, 8, 9])
            .expect("nine offered observations");
        assert_eq!(facts.len(), 9);
        assert!(facts.iter().all(|fact| !fact.label.is_empty()));
        // Nothing offered: the empty citation list is the only accepted one.
        let empty = EvidencePacket::new("Summarise the board", None, None);
        assert!(empty.resolve(&[]).expect("no citations").is_empty());
    }

    #[test]
    fn citations_outside_the_bounded_input_are_refused_with_the_stable_code() {
        let board = board(8);
        let packet = EvidencePacket::new("Summarise the board", Some(&board), None);
        for references in [
            vec![0],                             // zero is not a 1-based reference
            vec![1, 1],                          // the same observation twice
            vec![10],                            // beyond the nine offered
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10], // more citations than observations
        ] {
            let error = match packet.resolve(&references) {
                Ok(_) => panic!("{references:?} must be refused"),
                Err(error) => error,
            };
            assert_eq!(error.code, INVALID, "{references:?}");
            assert_eq!(error.message, "Lead reply supplied invalid evidence references");
        }
    }
}
