//! Read-only prepared-input normalization through the existing board Reader.
use super::*;
use crate::work_refill::Source;
use oxa_work::board::{Action, Entry, Issue, Snapshot};
pub(super) fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .ok_or_else(|| "missing board input field".into())
}

pub struct Prepared {
    pub number: u64,
    pub revision: String,
    pub action: Action,
    pub branch: String,
    pub task: String,
}
pub struct Input<'a> {
    reader: Reader<'a>,
    packets: &'a [Prepared],
    intake: bool,
}
impl<'a> Input<'a> {
    /// Caller authorizes the source, trusted executable and App token before acquisition.
    pub fn new(
        gh: &'a Path,
        path: &'a std::ffi::OsStr,
        repo: &'a str,
        token: &'a str,
        packets: &'a [Prepared],
        intake: bool,
    ) -> Self {
        Self {
            reader: Reader::new(gh, path, repo, token),
            packets,
            intake,
        }
    }
}
impl GitHub for Input<'_> {
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        let pull_requests = self.reader.open_pull_requests()?;
        let mut fields = "pullRequests(first:100,states:OPEN){totalCount nodes{number headRefOid headRefName closingIssuesReferences(first:100){totalCount nodes{number}}}}".to_owned();
        for packet in self.packets.iter().filter(|p| p.action == Action::BuildIssue) {
            fields.push_str(&format!(
                " i{}:issue(number:{}){{number state updatedAt}}",
                packet.number, packet.number
            ));
        }
        let (metadata, _) = self.reader.repository_read(&fields)?;
        let rows = nodes(&metadata["pullRequests"])?;
        let heads: BTreeMap<_, _> = rows
            .iter()
            .map(|p| (p["number"].as_u64(), p["headRefOid"].as_str()))
            .collect();
        if heads.len() != pull_requests.len()
            || pull_requests
                .iter()
                .any(|p| heads.get(&Some(p.number)) != Some(&Some(p.head.as_str())))
        {
            return Err("board membership or head changed; retry later".into());
        }
        let mut closing = BTreeSet::new();
        for pr in rows {
            for issue in nodes(&pr["closingIssuesReferences"])? {
                closing.insert(issue["number"].as_u64().ok_or("invalid closing issue")?);
            }
            if let Some(packet) = self
                .packets
                .iter()
                .find(|p| p.number == pr["number"].as_u64().unwrap_or(0))
            {
                if text(pr, "headRefName")? != packet.branch {
                    return Err("prepared PR branch changed".into());
                }
            }
        }
        let mut issues = vec![];
        for packet in self.packets.iter().filter(|p| p.action == Action::BuildIssue) {
            let issue = &metadata[format!("i{}", packet.number)];
            if issue["number"].as_u64() != Some(packet.number) {
                return Err("prepared issue is missing".into());
            }
            issues.push(Issue {
                number: packet.number,
                revision: text(issue, "updatedAt")?.into(),
                release_pinned: false,
                approved: self.intake && pull_requests.len() < 40 && text(issue, "state")? == "OPEN",
                has_open_pull_request: closing.contains(&packet.number),
            });
        }
        Ok(Snapshot {
            pull_requests,
            issues,
        })
    }
}
impl Source for Input<'_> {
    fn revalidate(&mut self, entry: &Entry) -> Result<bool, String> {
        let board = Board::read(
            self,
            &Policy {
                enabled: true,
                ..Policy::default()
            },
            &BTreeMap::new(),
            0,
        )?;
        Ok(board.entries.iter().any(|fresh| {
            fresh.number == entry.number && fresh.head == entry.head && fresh.action == entry.action
        }))
    }
}
