use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProblemNote {
    pub title: String,
    pub notes: String,
    pub keywords: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReportRecord {
    pub disposition: String,
    pub draft: PathBuf,
    pub url: Option<String>,
    pub detail: String,
}
pub(crate) const INSTRUCTIONS: &str = "\n\nIf you encountered a problem with oxa, oxc or the engine, follow the report-issue skill's privacy rules. Do not post it yourself. End your final reply with <oxa-work-reports>[{\"title\":\"component and symptom\",\"notes\":\"your own brief reproduction, expected and actual behavior\",\"keywords\":[\"component symptom\",\"command error\"]}]</oxa-work-reports>. Supply two or three distinct symptom keyword queries. Never include prompts, transcripts, secrets, personal data or file contents. Omit the block when there is no problem to report. The runner only files notes when its user explicitly requests --report-problems.";
pub(crate) fn parse(output: &str) -> Vec<ProblemNote> {
    let Some((_, rest)) = output.rsplit_once("<oxa-work-reports>") else {
        return vec![];
    };
    let Some((json, _)) = rest.split_once("</oxa-work-reports>") else {
        return vec![];
    };
    if json.len() > 32_000 {
        return vec![];
    }
    let Ok(notes) = serde_json::from_str::<Vec<ProblemNote>>(json) else {
        return vec![];
    };
    notes
        .into_iter()
        .take(5)
        .filter(|n| {
            !n.title.trim().is_empty()
                && n.title.chars().count() <= 200
                && !n.notes.trim().is_empty()
                && n.notes.chars().count() <= 2000
                && (2..=3).contains(&n.keywords.len())
                && n.keywords
                    .iter()
                    .all(|k| !k.trim().is_empty() && k.chars().count() <= 100)
                && n.keywords
                    .iter()
                    .map(|q| q.trim().to_lowercase())
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    == n.keywords.len()
        })
        .collect()
}
