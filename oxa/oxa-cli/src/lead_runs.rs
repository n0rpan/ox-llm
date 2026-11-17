//! Signal projection over complete, read-only GitHub Actions queue observations.

use serde_json::Value;
use std::collections::BTreeSet;

const MAX_RUNS: u64 = 1_000;
const PAGE_SIZE: usize = 100;
const STALL_SECONDS: u64 = 15 * 60;
const INCOMPLETE: &str = "incomplete queued workflow runs";

/// Return run IDs from complete REST pages sampled with `status=queued`.
/// A recently updated run is quiet again, including an old run queued for a retry.
pub fn stalled(pages: &[Value], observed_at: u64) -> Result<Vec<u64>, &'static str> {
    let total = pages
        .first()
        .and_then(|page| page["total_count"].as_u64())
        .filter(|total| *total <= MAX_RUNS)
        .ok_or(INCOMPLETE)?;
    if pages.len() != (total as usize).div_ceil(PAGE_SIZE).max(1) {
        return Err(INCOMPLETE);
    }

    let mut seen = BTreeSet::new();
    let mut stalled = Vec::new();
    for (index, page) in pages.iter().enumerate() {
        let rows = page["workflow_runs"].as_array().ok_or(INCOMPLETE)?;
        let expected = (total as usize - index * PAGE_SIZE).min(PAGE_SIZE);
        if page["total_count"].as_u64() != Some(total) || rows.len() != expected {
            return Err(INCOMPLETE);
        }
        for row in rows {
            let id = row["id"].as_u64().filter(|id| *id > 0).ok_or(INCOMPLETE)?;
            if row["status"].as_str() != Some("queued") || !seen.insert(id) {
                return Err(INCOMPLETE);
            }
            let updated = row["updated_at"]
                .as_str()
                .and_then(oxa_session::parse_utc_seconds)
                .ok_or(INCOMPLETE)?;
            if observed_at
                .checked_sub(updated)
                .is_some_and(|age| age > STALL_SECONDS)
            {
                stalled.push(id);
            }
        }
    }
    Ok(stalled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(id: u64, updated: &str) -> Value {
        json!({
            "id": id,
            "status": "queued",
            "created_at": "1970-01-01T00:00:00Z",
            "updated_at": updated
        })
    }

    #[test]
    fn signals_only_after_fifteen_minutes_since_the_queued_record_changed() {
        let pages = [json!({"total_count":3, "workflow_runs":[
            run(10, "1970-01-02T00:00:00Z"),
            run(11, "1970-01-02T00:00:01Z"),
            run(12, "1970-01-02T01:00:00Z")
        ]})];
        assert!(stalled(&pages, 86_400 + 900).unwrap().is_empty());
        let ids = stalled(&pages, 86_400 + 901).unwrap();
        assert_eq!(ids, [10]);
        let page = crate::lead_page::content(
            &crate::lead_page::View {
                stalled_runs: &ids,
                ..Default::default()
            },
            86_400 + 3600,
        );
        assert!(page.contains("Workflow runs queued for more than 15 minutes (1)"));
        assert!(page.contains("Run 10"));
        assert!(!page.contains("Run 11"));
    }

    #[test]
    fn requires_complete_consistent_pages_and_unique_queued_runs() {
        let pages: Vec<_> = (0..10)
            .map(|page| {
                let rows: Vec<_> = (1..=100)
                    .map(|i| run(page * 100 + i, "1970-01-01T00:00:00Z"))
                    .collect();
                json!({"total_count":1_000, "workflow_runs":rows})
            })
            .collect();
        assert_eq!(stalled(&pages, 901).unwrap(), (1..=1_000).collect::<Vec<_>>());
        assert!(stalled(&pages[..9], 901).is_err());

        for (field, value) in [
            ("/total_count", json!(1_001)),
            ("/workflow_runs/0/id", json!(1)),
            ("/workflow_runs/0/status", json!("in_progress")),
            ("/workflow_runs/0/updated_at", json!("invalid")),
        ] {
            let mut changed = pages.clone();
            *changed[9].pointer_mut(field).unwrap() = value;
            assert!(stalled(&changed, 901).is_err(), "{field}");
        }
    }
}
