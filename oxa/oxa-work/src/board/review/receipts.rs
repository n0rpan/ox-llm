use super::{git, Request, RequestReceipt, Reviewer};
use serde::{Deserialize, Serialize};

const PREFIX: &str = "<!-- board-review-request:v1 ";
const SUFFIX: &str = " -->";

/// A bounded GitHub marker for cross-run request deduplication.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubRequestReceipt {
    reviewer: Reviewer,
    sha: String,
    patch_id: String,
}

impl GitHubRequestReceipt {
    /// GitHub supplies the actor login. Only the runner's configured account
    /// may seed requests; a matching marker elsewhere is ordinary PR text.
    pub fn from_comment(trusted_login: &str, actor: &str, body: &str) -> Option<Self> {
        if trusted_login.is_empty() || !trusted_login.eq_ignore_ascii_case(actor) {
            return None;
        }
        let mut markers = body
            .lines()
            .filter_map(|line| line.strip_prefix(PREFIX)?.strip_suffix(SUFFIX));
        let raw = markers.next().filter(|raw| raw.len() <= 256)?;
        if markers.next().is_some() {
            return None;
        }
        let receipt: Self = serde_json::from_str(raw).ok()?;
        (receipt.reviewer != Reviewer::Owner
            && git::object_id(&receipt.sha)
            && git::object_id(&receipt.patch_id))
        .then_some(receipt)
    }

    pub fn receipt(&self) -> RequestReceipt<'_> {
        RequestReceipt {
            reviewer: self.reviewer,
            patch_id: &self.patch_id,
        }
    }
}

impl Request {
    /// The native dispatcher controls the review command and appends this marker
    /// as its own line. The marker deliberately contains no PR text.
    pub fn github_marker(&self) -> crate::Result<String> {
        if !git::object_id(self.head()) || !self.key.valid() {
            return Err("invalid GitHub review request identity".into());
        }
        let receipt = GitHubRequestReceipt {
            reviewer: self.reviewer(),
            sha: self.head().to_owned(),
            patch_id: self.patch_id().to_owned(),
        };
        let json = serde_json::to_string(&receipt).map_err(|_| "cannot encode review request")?;
        Ok(format!("{PREFIX}{json}{SUFFIX}"))
    }
}
