//! Read-only prepared-input normalization through the existing board Reader.
use super::*;
use crate::work_refill::Source;
use oxa_work::board::{Action, Entry, ExistingWork, Issue, Snapshot};
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
    host: Option<&'a str>,
    existing: BTreeMap<u64, ExistingWork>,
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
            host: None,
            existing: BTreeMap::new(),
        }
    }
    pub fn with_host(mut self, host: &'a str) -> Self {
        self.host = Some(host);
        self
    }
}
impl GitHub for Input<'_> {
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        self.existing.clear();
        let pull_requests = self.reader.open_pull_requests()?;
        let mut issue_fields = String::new();
        for packet in self.packets.iter().filter(|p| p.action == Action::BuildIssue) {
            issue_fields.push_str(&format!(
                " i{}:issue(number:{}){{number state updatedAt}}",
                packet.number, packet.number
            ));
            if self.host.is_some() {
                let branch = serde_json::to_string(&format!("refs/heads/{}", packet.branch))
                    .map_err(|_| "invalid branch query")?;
                issue_fields.push_str(&format!(" b{}:ref(qualifiedName:{branch}){{name target{{... on Commit{{committer{{user{{login}}}}}}}}}}", packet.number));
            }
        }
        let (mut metadata, mut rows, mut cursor, mut cursors) =
            (Value::Null, vec![], String::new(), BTreeSet::new());
        // Retained normalized facts and all metadata pages share one bounded budget.
        let mut bytes = serde_json::to_vec(&pull_requests)
            .map_err(|_| "cannot bound board facts")?
            .len();
        loop {
            let fields = format!(
                "pullRequests(first:100,states:OPEN{cursor}){{totalCount nodes{{number headRefOid headRefName author{{login}} closingIssuesReferences(first:100){{totalCount nodes{{number}}}}}} pageInfo{{hasNextPage endCursor}}}}{issue_fields}"
            );
            let (page, read) = self.reader.repository_read(&fields)?;
            super::charge(&mut bytes, read)?;
            let connection = &page["pullRequests"];
            if connection["totalCount"].as_u64() != Some(pull_requests.len() as u64) {
                return Err("board membership changed; retry later".into());
            }
            let batch = connection["nodes"].as_array().ok_or("missing metadata page")?;
            if batch.len() > 100 {
                return Err("oversized metadata page".into());
            }
            rows.extend(batch.iter().cloned());
            if rows.len() > pull_requests.len() {
                return Err("oversized metadata snapshot".into());
            }
            if metadata.is_null() {
                metadata = page.clone();
            }
            if !connection["pageInfo"]["hasNextPage"]
                .as_bool()
                .ok_or("missing metadata pagination")?
            {
                break;
            }
            let next = connection["pageInfo"]["endCursor"]
                .as_str()
                .filter(|c| !c.is_empty())
                .ok_or("missing metadata cursor")?;
            if batch.is_empty() || !cursors.insert(next.to_owned()) {
                return Err("invalid metadata pagination progress".into());
            }
            cursor = format!(",after:{next:?}");
            issue_fields.clear();
        }
        metadata["pullRequests"]["nodes"] = rows.into();
        let rows = nodes(&metadata["pullRequests"])?;
        let heads: BTreeMap<_, _> = rows
            .iter()
            .map(|p| (p["number"].as_u64(), p["headRefOid"].as_str()))
            .collect();
        if heads.len() != rows.len()
            || heads.len() != pull_requests.len()
            || pull_requests
                .iter()
                .any(|p| heads.get(&Some(p.number)) != Some(&Some(p.head.as_str())))
        {
            return Err("board membership or head changed; retry later".into());
        }
        let mut closing = BTreeSet::new();
        for pr in rows {
            if self.host.is_some() {
                for packet in self.packets.iter().filter(|p| p.action == Action::BuildIssue) {
                    if text(pr, "headRefName")? == packet.branch
                        || nodes(&pr["closingIssuesReferences"])?
                            .iter()
                            .any(|issue| issue["number"].as_u64() == Some(packet.number))
                    {
                        self.existing.insert(
                            packet.number,
                            ExistingWork::PullRequest {
                                number: pr["number"].as_u64().ok_or("invalid existing PR")?,
                                owner: text(&pr["author"], "login")?.into(),
                            },
                        );
                    }
                }
            }
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
            if self.host.is_some() && !self.existing.contains_key(&packet.number) {
                let branch = metadata
                    .get(format!("b{}", packet.number))
                    .ok_or("incomplete branch observation")?;
                if !branch.is_null() {
                    if text(branch, "name")? != packet.branch {
                        return Err("observed branch changed identity".into());
                    }
                    self.existing.insert(
                        packet.number,
                        ExistingWork::Branch {
                            name: packet.branch.clone(),
                            owner: text(&branch["target"]["committer"]["user"], "login")?.into(),
                        },
                    );
                }
            }
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
    fn host_id(&self) -> Result<Option<&str>, String> {
        self.host
            .map(Some)
            .ok_or_else(|| "native refill requires an authenticated host".into())
    }
    fn existing_work(&mut self, entry: &Entry) -> Result<Option<ExistingWork>, String> {
        self.host_id()?;
        let valid = self.revalidate(entry)?;
        if let Some(existing) = self.existing.remove(&entry.number) {
            return Ok(Some(existing));
        }
        if !valid {
            return Err("prepared source changed before hosted admission".into());
        }
        Ok(None)
    }
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
