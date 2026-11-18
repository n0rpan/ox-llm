//! Presentation groups only; native board policy retains admission and execution authority.
use super::{escape, pages::BOARD_ROWS, View};
use oxa_work::board::{Action, Board, Entry};
use std::fmt::Write;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Blocked,
    Fix,
    Waiting,
    Ready,
}
impl Filter {
    const ALL: [(Self, &'static str, &'static str); 5] = [
        (Self::All, "all", "All"),
        (Self::Blocked, "blocked", "Blocked"),
        (Self::Fix, "fix", "Needs fix"),
        (Self::Waiting, "waiting", "Waiting"),
        (Self::Ready, "ready", "Ready"),
    ];
    fn matches(self, row: &Entry, view: &View<'_>) -> Option<bool> {
        match self {
            Self::All => Some(true),
            Self::Blocked => self.available(view).then(|| blocked(row, view)).flatten(),
            _ => Some(blocked(row, view) != Some(true) && self == group(row.action)),
        }
    }
    pub(super) fn available(self, view: &View<'_>) -> bool {
        self != Self::Blocked
            || view.situation.is_some_and(|snapshot| {
                snapshot
                    .blocked
                    .iter()
                    .map(|block| &block.subject)
                    .chain(snapshot.asks.iter().map(|ask| &ask.subject))
                    .all(|subject| matches!(subject, oxa_proto::work::Subject::PullRequest(_)))
            })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub filter: Filter,
    pub page: usize,
}
impl Query {
    pub(super) fn parse(text: &str) -> Option<Self> {
        let (mut filter, mut page) = (None, None);
        for part in text.split('&') {
            let (key, value) = part.split_once('=')?;
            match key {
                "f" if filter.is_none() => {
                    filter = Some(if value.is_empty() {
                        Filter::All
                    } else {
                        Filter::ALL.iter().find(|(_, key, _)| *key == value)?.0
                    })
                }
                "page" if page.is_none() => {
                    let parsed: usize = value.parse().ok()?;
                    if parsed == 0 || parsed.to_string() != value {
                        return None;
                    }
                    parsed.checked_mul(BOARD_ROWS)?;
                    page = Some(parsed);
                }
                _ => return None,
            }
        }
        Some(Self {
            filter: filter.unwrap_or_default(),
            page: page.unwrap_or(0),
        })
    }
    pub(super) fn url(self) -> String {
        let mut path = String::from("/board");
        if self.filter != Filter::All {
            let key = Filter::ALL
                .iter()
                .find(|(filter, _, _)| *filter == self.filter)
                .unwrap()
                .1;
            let _ = write!(path, "?f={key}");
        }
        if self.page > 0 {
            let separator = if self.filter == Filter::All { '?' } else { '&' };
            let _ = write!(path, "{separator}page={}", self.page);
        }
        path
    }
    pub(super) fn rows<'a>(self, board: &'a Board, view: &View<'_>) -> Vec<&'a Entry> {
        let mut rows: Vec<_> = board
            .entries
            .iter()
            .filter(|row| self.filter.matches(row, view) == Some(true))
            .collect();
        rows.sort_by_key(|row| {
            (
                blocked(row, view) != Some(true),
                !row.release_pinned,
                group(row.action) as u8,
            )
        });
        rows
    }
    pub(super) fn navigation(self, html: &mut String, board: &Board, view: &View<'_>) {
        html.push_str("<nav class=filters aria-label=\"Board filters\">");
        for (filter, _, label) in Filter::ALL {
            if !filter.available(view) {
                html.push_str("<span class=muted>Blocked · unavailable</span>");
                continue;
            }
            let count = board
                .entries
                .iter()
                .filter(|row| filter.matches(row, view) == Some(true))
                .count();
            let path = escape(&Self { filter, page: 0 }.url());
            let selected = if self.filter == filter {
                " aria-current=page"
            } else {
                ""
            };
            let _ = write!(html, "<a href=\"{path}\"{selected}>{label} ({count})</a>");
        }
        html.push_str("</nav>");
    }
}

fn blocked(row: &Entry, view: &View<'_>) -> Option<bool> {
    let snapshot = view.situation?;
    let subject = oxa_proto::work::Subject::PullRequest(row.number);
    Some(
        snapshot.blocked.iter().any(|block| block.subject == subject)
            || snapshot.asks.iter().any(|ask| ask.subject == subject),
    )
}

fn group(action: Action) -> Filter {
    match action {
        Action::Ready => Filter::Ready,
        Action::WaitRequiredCi | Action::WaitMergeability | Action::WaitReview | Action::WaitIssue => {
            Filter::Waiting
        }
        Action::FixFindings
        | Action::FixRequiredCi
        | Action::ResolveConflict
        | Action::AddRemoves
        | Action::BuildIssue
        | Action::PromoteDraft => Filter::Fix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::work::{Ask, AskClosed, Blocked, Checkpoint, Fact, Subject};

    #[test]
    fn native_asks_order_and_partition_rows_until_the_real_close_event() {
        let test = oxa_fake::TestEnv::new();
        let facts = oxa_work::events::Facts::initialize(&test.agent_dir().join("facts")).unwrap();
        let record = |revision, fact| {
            facts
                .append(
                    Checkpoint {
                        source: "synthetic-ask".into(),
                        revision,
                    },
                    revision + 9,
                    fact,
                )
                .unwrap();
        };
        record(
            1,
            Fact::AskOpened(Ask {
                id: "ask-1".into(),
                subject: Subject::PullRequest(1),
                question: "Choose next step".into(),
            }),
        );
        let entry = |number, action, release_pinned| Entry {
            number,
            action,
            release_pinned,
            head: "synthetic-head".into(),
            lease: None,
            attempts_today: 0,
            eligible: true,
        };
        let board = Board {
            observed_at: 1,
            entries: vec![
                entry(1, Action::Ready, false),
                entry(2, Action::Ready, true),
                entry(3, Action::FixFindings, false),
                entry(4, Action::WaitReview, false),
            ],
        };
        let numbers = |filter, snapshot: &_| {
            Query { filter, page: 0 }
                .rows(
                    &board,
                    &View {
                        situation: Some(snapshot),
                        ..View::default()
                    },
                )
                .iter()
                .map(|row| row.number)
                .collect::<Vec<_>>()
        };
        let snapshot = facts.situation().unwrap();
        assert_eq!(numbers(Filter::All, &snapshot), [1, 2, 3, 4]);
        assert_eq!(numbers(Filter::Blocked, &snapshot), [1]);
        assert_eq!(numbers(Filter::Ready, &snapshot), [2]);
        record(2, Fact::AskClosed(AskClosed { id: "ask-1".into() }));
        let snapshot = facts.situation().unwrap();
        assert!(numbers(Filter::Blocked, &snapshot).is_empty());
        assert_eq!(numbers(Filter::Ready, &snapshot), [2, 1]);
        record(
            3,
            Fact::Blocked(Blocked {
                subject: Subject::PullRequest(4),
                code: "synthetic-block".into(),
                ask_id: None,
            }),
        );
        let snapshot = facts.situation().unwrap();
        assert_eq!(numbers(Filter::Blocked, &snapshot), [4]);
        assert!(numbers(Filter::Waiting, &snapshot).is_empty());
        assert_eq!(numbers(Filter::All, &snapshot), [4, 2, 3, 1]);
        record(
            4,
            Fact::AskOpened(Ask {
                id: "ask-task".into(),
                subject: Subject::Task("unmapped-task".into()),
                question: "Choose next step".into(),
            }),
        );
        let snapshot = facts.situation().unwrap();
        assert!(numbers(Filter::Blocked, &snapshot).is_empty());
        assert_eq!(Filter::Blocked.matches(&board.entries[0], &View::default()), None);
    }
}
