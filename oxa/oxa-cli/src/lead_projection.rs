//! Pure throughput and stuck-work projection over already-sampled facts (#2346).
//!
//! No file, network or clock access and no configuration read: the caller passes the
//! decoded sample history, the board, the owner's target and the observation time.
//! The merge history comes from the sample log itself (each record's verified main
//! head), so the projection adds no GitHub call and no per-tab work. S4's observer
//! can cache the result; S6's Home and S11's Board render it without recomputing it.

use crate::lead_page::{self, View};
use oxa_work::board::{Action, Board};
use std::fmt::Write;

/// Hourly buckets the Home page shows, oldest first; the last is the current hour.
pub const HOURS: usize = 12;
/// Owner default when `oxa.lead.target.merges_per_hour` sets nothing.
pub const DEFAULT_TARGET: u32 = 10;
const HOUR: u64 = 3_600;

/// One verified main merge observed in the sample history: the merge commit oid
/// (the dedupe key) and its verified `mergedAt` in Unix seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merge {
    pub at: u64,
    pub head: String,
}

/// Merge facts decoded from the complete Reader records in the sample tail.
/// `covered_from` is the oldest record's observation time: hours before it were
/// never sampled and stay unknown, never zero. `main_samples` counts records that
/// carried a main head at all, so a sampler without `--landing-shadow` is named.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeHistory {
    /// Chronological, deduped by merge commit oid.
    pub merges: Vec<Merge>,
    pub covered_from: Option<u64>,
    /// The latest record's observation time; the window ends here, not at the wall
    /// clock, so an old sample cannot render as a quiet recent hour.
    pub observed_at: u64,
    pub main_samples: usize,
}

/// Merges per hour against the owner's target, from the sample history alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Throughput {
    /// Oldest first; the last bucket is the current, partial hour. `None` means the
    /// hour was not fully sampled: an unknown hour never renders as zero merges.
    pub buckets: [Option<u32>; HOURS],
    /// Start of the oldest bucket (UTC-hour aligned).
    pub window_start: u64,
    /// Start of the current, partial hour.
    pub hour_start: u64,
    pub target: u32,
    /// The last complete hour's count; `None` when that hour is unknown.
    pub merged_last_hour: Option<u32>,
    /// Newest verified merge known, even before the window; `None` when none was observed.
    pub last_merge_at: Option<u64>,
    /// Verified merges observed inside the window (a lower bound: two merges between
    /// two samples advance the head once).
    pub observed: u32,
    pub covered_from: Option<u64>,
    pub main_samples: usize,
}

impl Throughput {
    /// Project the history into 12 hourly buckets ending at the latest observation.
    /// A bucket is known only when its whole hour lies inside the sampled coverage.
    pub fn project(history: &MergeHistory, target: u32) -> Self {
        let hour_start = history.observed_at - history.observed_at % HOUR;
        let window_start = hour_start.saturating_sub((HOURS as u64 - 1) * HOUR);
        let mut buckets = [None; HOURS];
        for (i, bucket) in buckets.iter_mut().enumerate() {
            let start = window_start.saturating_add(i as u64 * HOUR);
            if history.covered_from.is_some_and(|from| start >= from) {
                let end = start.saturating_add(HOUR);
                *bucket = Some(
                    history
                        .merges
                        .iter()
                        .filter(|merge| merge.at >= start && merge.at < end)
                        .count() as u32,
                );
            }
        }
        Self {
            buckets,
            window_start,
            hour_start,
            target,
            merged_last_hour: buckets[HOURS - 2],
            last_merge_at: history.merges.iter().map(|merge| merge.at).max(),
            observed: history
                .merges
                .iter()
                .filter(|merge| merge.at >= window_start && merge.at < hour_start + HOUR)
                .count() as u32,
            covered_from: history.covered_from,
            main_samples: history.main_samples,
        }
    }

    /// The bar scale: the target and the fullest known hour, at least one.
    pub fn scale(&self) -> u32 {
        self.buckets
            .iter()
            .flatten()
            .copied()
            .chain(std::iter::once(self.target))
            .max()
            .unwrap_or(1)
            .max(1)
    }
}

/// One distinct stuck-work reason. The counts come from facts the sample already
/// carries; `None` on a reason means its source was not observed, never zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Open PRs whose next action is waiting for review.
    NoReview,
    /// Open PRs whose required CI failed at the observed head.
    RedCi,
    /// PRs whose body carries a `Restricts:` line without the owner's
    /// `owner-accepted` label, as the Reader recorded from the same sample.
    HeldRestricts,
    /// Locally accepted work not published as a PR yet (runner situation).
    NoPr,
}

impl Reason {
    pub const ALL: [Self; 4] = [Self::NoReview, Self::RedCi, Self::HeldRestricts, Self::NoPr];

    pub fn label(self) -> &'static str {
        match self {
            Self::NoReview => "No review",
            Self::RedCi => "Red CI",
            Self::HeldRestricts => "Held Restricts:",
            Self::NoPr => "No PR yet",
        }
    }

    /// Where the count links. Review and CI map to the Board's existing filter
    /// groups; a held PR can sit in any group and unpublished work is not a PR,
    /// so those link to the whole Board and the Workers page.
    pub fn link(self) -> &'static str {
        match self {
            Self::NoReview => "/board?f=waiting",
            Self::RedCi => "/board?f=fix",
            Self::HeldRestricts => "/board",
            Self::NoPr => "/workers",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::NoReview => "waiting for review",
            Self::RedCi => "required checks failed",
            Self::HeldRestricts => "needs owner-accepted",
            Self::NoPr => "accepted, unpublished",
        }
    }
}

/// The distinct stuck counts of one observation. Each reason degrades on its own:
/// an absent source stays `None` and the page says why below the group.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stuck {
    pub no_review: Option<Vec<u64>>,
    pub red_ci: Option<Vec<u64>>,
    pub held_restricts: Option<Vec<u64>>,
    /// Task ids of unpublished local acceptances; `None` without a runner situation.
    pub no_pr: Option<Vec<String>>,
}

impl Stuck {
    /// Derive the counts from the board's own actions, the recorded `Restricts:`
    /// holds and the situation's unpublished acceptances. No policy is recomputed.
    pub fn project(
        board: Option<&Board>,
        restricts_held: Option<&[u64]>,
        unpublished: Option<&[String]>,
    ) -> Self {
        let with_action = |action: Action| {
            board.map(|board| {
                board
                    .entries
                    .iter()
                    .filter(|entry| entry.action == action)
                    .map(|entry| entry.number)
                    .collect()
            })
        };
        Self {
            no_review: with_action(Action::WaitReview),
            red_ci: with_action(Action::FixRequiredCi),
            held_restricts: restricts_held.map(|held| held.to_vec()),
            no_pr: unpublished.map(|tasks| tasks.to_vec()),
        }
    }

    pub fn count(&self, reason: Reason) -> Option<usize> {
        match reason {
            Reason::NoReview => self.no_review.as_ref().map(Vec::len),
            Reason::RedCi => self.red_ci.as_ref().map(Vec::len),
            Reason::HeldRestricts => self.held_restricts.as_ref().map(Vec::len),
            Reason::NoPr => self.no_pr.as_ref().map(Vec::len),
        }
    }
}

/// Both projections of one observation, built once by the caller and rendered by
/// any page. The Home wiring is one call; S6's Home keeps the same model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Work {
    pub throughput: Throughput,
    pub stuck: Stuck,
}

/// The Home throughput and stuck-work panel. Server-rendered from the typed model;
/// every unknown says why, and no absent source renders as a healthy zero.
pub fn render(html: &mut String, view: &View<'_>, now: u64) {
    html.push_str("<section id=work class=\"card work\"><h2>Throughput</h2>");
    let Some(work) = view.work else {
        // A board without a projection is a caller that did not build one; the
        // board's own sections still render. Only a page with no board at all
        // shows the missing state, with the allow-listed cause.
        if view.board.is_some() {
            return;
        }
        let (cause, next) = lead_page::board_gap(view);
        lead_page::say_why(html, "Throughput", cause, next, None, now);
        html.push_str("<h2>Stuck work</h2>");
        lead_page::say_why(html, "Stuck work", cause, next, None, now);
        html.push_str("</section>");
        return;
    };
    let throughput = &work.throughput;
    // Headline: the last complete hour against the owner's configured target.
    html.push_str("<p class=work-head>");
    match throughput.merged_last_hour {
        Some(count) => {
            let _ = write!(
                html,
                "<span class=num>{count}</span> merged last full hour · target {}/h ",
                throughput.target
            );
            if count >= throughput.target {
                html.push_str("<span class=\"chip ok\">at target</span>");
            } else {
                html.push_str("<span class=\"chip warn\">below target</span>");
            }
        }
        None => {
            let _ = write!(
                html,
                "<span class=num>–</span> merged last full hour · target {}/h",
                throughput.target
            );
        }
    }
    html.push_str("</p>");
    // Twelve hourly bars, oldest first; the last is the current partial hour.
    // SVG attributes carry the values (the CSP allows no inline style), and the
    // text alternative names every hour so the bars are not colour-only.
    let scale = throughput.scale();
    let mut alternative = String::from("Merges per hour, last 12 hours: ");
    let mut bars = String::new();
    for (i, bucket) in throughput.buckets.iter().enumerate() {
        let start = throughput.window_start.saturating_add(i as u64 * HOUR);
        let hour = (start % 86_400) / HOUR;
        let x = i * 10 + 1;
        let (height, text) = match bucket {
            Some(count) => {
                let height = (u64::from(*count) * 40 / u64::from(scale)).min(40);
                (height, format!("{count} merges"))
            }
            None => (40, String::from("not sampled")),
        };
        let class = if bucket.is_none() { " class=unknown" } else { "" };
        let _ = write!(
            bars,
            "<rect{class} x={x} y={} width=8 height={height}><title>{hour:02}:00 UTC: {text}</title></rect>",
            42 - height
        );
        let _ = write!(alternative, "{hour:02}:00 {text}; ");
    }
    let target_y = 42 - (u64::from(throughput.target) * 40 / u64::from(scale)).min(40);
    let _ = write!(
        html,
        "<div class=bars><svg viewBox=\"0 0 120 44\" role=img aria-label=\"{}\"><line class=target x1=0 y1={target_y} x2=120 y2={target_y}></line>{bars}</svg></div>",
        lead_page::escape(alternative.trim_end_matches("; "))
    );
    let last_merge = match throughput.last_merge_at {
        Some(at) => format!("last merge {}", lead_page::age(at, now)),
        None => String::from("no merge observed"),
    };
    let _ = write!(
        html,
        "<p class=muted>{} observed main merges in 12 h (last bar: the hour in progress) · {last_merge} · two merges between samples count once</p>",
        throughput.observed
    );
    if throughput.main_samples == 0 {
        html.push_str("<p class=\"muted say-why\"><b>No merge history in this sample.</b> These records carry no main head; sample with <code>--landing-shadow --lead-alarms</code> to fill the bars.</p>");
    } else if let Some(from) = throughput.covered_from {
        if from > throughput.window_start {
            let _ = write!(
                html,
                "<p class=\"muted say-why\"><b>Partly sampled:</b> hours before {} were not retained in the log tail and stay unknown.</p>",
                lead_page::human_time(from, now)
            );
        }
    }
    html.push_str("<h2>Stuck work</h2><div class=\"tiles work-tiles\">");
    let mut missing: Vec<&str> = Vec::new();
    for reason in Reason::ALL {
        let count = work.stuck.count(reason);
        let value = count.map_or_else(|| "–".to_string(), |n| n.to_string());
        let _ = write!(
            html,
            "<a class=tile href=\"{}\"><span class=k>{}</span><span class=\"v num\">{value}</span><span class=d>{}</span></a>",
            reason.link(),
            reason.label(),
            reason.hint()
        );
        if count.is_none() {
            missing.push(reason.label());
        }
    }
    html.push_str("</div>");
    if !missing.is_empty() {
        let cause = match view.board_cause {
            Some(cause) => format!("{}.", lead_page::sentence(cause.text())),
            None => String::from("The latest sample did not carry this source."),
        };
        let _ = write!(
            html,
            "<p class=\"muted say-why\"><b>Unobserved counts:</b> {}. {cause}</p>",
            lead_page::escape(&missing.join(", "))
        );
    }
    html.push_str("</section>");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(merges: &[(u64, &str)], covered_from: Option<u64>, observed_at: u64) -> MergeHistory {
        MergeHistory {
            merges: merges
                .iter()
                .map(|(at, head)| Merge {
                    at: *at,
                    head: (*head).into(),
                })
                .collect(),
            covered_from,
            observed_at,
            main_samples: 1,
        }
    }

    #[test]
    fn buckets_align_to_utc_hours_and_count_each_merge_once() {
        // 12:20 observation; merges at 10:05, 10:55, 11:00 and 12:10.
        let base = 12 * HOUR + 20 * 60;
        let h = history(
            &[
                (10 * HOUR + 5 * 60, "a"),
                (10 * HOUR + 55 * 60, "b"),
                (11 * HOUR, "c"),
                (12 * HOUR + 10 * 60, "d"),
            ],
            Some(9 * HOUR),
            base,
        );
        let t = Throughput::project(&h, 10);
        assert_eq!(t.hour_start, 12 * HOUR);
        assert_eq!(t.window_start, HOUR); // 12 buckets: 01:00 .. 12:00
        assert_eq!(t.buckets[9], Some(2)); // 10:00 hour
        assert_eq!(t.buckets[10], Some(1)); // 11:00 hour, the last full hour
        assert_eq!(t.buckets[11], Some(1)); // current partial hour
        assert_eq!(t.merged_last_hour, Some(1));
        assert_eq!(t.last_merge_at, Some(12 * HOUR + 10 * 60));
        assert_eq!(t.observed, 4);
    }

    #[test]
    fn unsampled_hours_stay_unknown_and_never_render_as_zero() {
        // Only the current hour is covered: the eleven older buckets are unknown.
        let h = history(&[], Some(12 * HOUR), 12 * HOUR + 60);
        let t = Throughput::project(&h, 10);
        assert!(t.buckets[..11].iter().all(|b| b.is_none()));
        assert_eq!(t.buckets[11], Some(0));
        assert_eq!(t.merged_last_hour, None);
        // A known zero is distinct from an unknown hour.
        let h = history(&[], Some(0), 12 * HOUR + 60);
        let t = Throughput::project(&h, 10);
        assert_eq!(t.buckets[10], Some(0));
        assert_eq!(t.merged_last_hour, Some(0));
    }

    #[test]
    fn the_window_ends_at_the_observation_not_the_wall_clock() {
        let h = history(&[(11 * HOUR, "a")], Some(0), 12 * HOUR);
        let t = Throughput::project(&h, 10);
        assert_eq!(t.hour_start, 12 * HOUR);
        assert_eq!(t.buckets[10], Some(1));
        // A merge after the observation is not projected into a later hour.
        let h = history(&[(13 * HOUR, "a")], Some(0), 12 * HOUR);
        let t = Throughput::project(&h, 10);
        assert_eq!(t.observed, 0);
        assert_eq!(t.last_merge_at, Some(13 * HOUR));
    }

    #[test]
    fn the_scale_covers_the_target_and_the_fullest_hour() {
        let h = history(&[], Some(0), 12 * HOUR);
        assert_eq!(Throughput::project(&h, 10).scale(), 10);
        let merges: Vec<(u64, String)> = (0..15).map(|i| (11 * HOUR + i, format!("head-{i}"))).collect();
        let h = MergeHistory {
            merges: merges
                .iter()
                .map(|(at, head)| Merge {
                    at: *at,
                    head: head.clone(),
                })
                .collect(),
            covered_from: Some(0),
            observed_at: 12 * HOUR,
            main_samples: 1,
        };
        assert_eq!(Throughput::project(&h, 10).scale(), 15);
    }

    #[test]
    fn stuck_reasons_are_distinct_and_degrade_per_source() {
        let entry = |number, action| oxa_work::board::Entry {
            number,
            head: "a".repeat(40),
            release_pinned: false,
            action,
            lease: None,
            attempts_today: 0,
            eligible: true,
            evidence_incomplete: None,
        };
        let board = Board {
            observed_at: 100,
            entries: vec![
                entry(1, Action::WaitReview),
                entry(2, Action::WaitReview),
                entry(3, Action::FixRequiredCi),
                entry(4, Action::Ready),
            ],
        };
        let stuck = Stuck::project(
            Some(&board),
            Some(&[4, 9]),
            Some(&["task-a".into(), "task-b".into()]),
        );
        assert_eq!(stuck.no_review.as_deref(), Some(&[1, 2][..]));
        assert_eq!(stuck.red_ci.as_deref(), Some(&[3][..]));
        assert_eq!(stuck.held_restricts.as_deref(), Some(&[4, 9][..]));
        assert_eq!(
            stuck.no_pr.as_deref(),
            Some(&["task-a".into(), "task-b".into()][..])
        );
        assert_eq!(stuck.count(Reason::NoReview), Some(2));
        assert_eq!(stuck.count(Reason::RedCi), Some(1));
        // Each absent source stays unknown on its own.
        let stuck = Stuck::project(Some(&board), None, None);
        assert_eq!(stuck.count(Reason::NoReview), Some(2));
        assert_eq!(stuck.count(Reason::HeldRestricts), None);
        assert_eq!(stuck.count(Reason::NoPr), None);
        let stuck = Stuck::project(None, None, None);
        assert!(Reason::ALL.iter().all(|r| stuck.count(*r).is_none()));
        // An observed empty source is a real zero, not an unknown.
        let stuck = Stuck::project(Some(&Board::default()), Some(&[]), Some(&[]));
        assert!(Reason::ALL.iter().all(|r| stuck.count(*r) == Some(0)));
    }

    #[test]
    fn the_panel_names_unknowns_and_links_the_counts() {
        let board = Board {
            observed_at: 12 * HOUR,
            entries: vec![oxa_work::board::Entry {
                number: 7,
                head: "a".repeat(40),
                release_pinned: false,
                action: Action::WaitReview,
                lease: None,
                attempts_today: 0,
                eligible: true,
                evidence_incomplete: None,
            }],
        };
        let work = Work {
            throughput: Throughput::project(&history(&[], Some(12 * HOUR), 12 * HOUR), 10),
            stuck: Stuck::project(Some(&board), None, None),
        };
        let view = View {
            board: Some(&board),
            work: Some(&work),
            ..View::default()
        };
        let mut html = String::new();
        render(&mut html, &view, 12 * HOUR + 60);
        assert!(html.contains("id=work"));
        assert_eq!(html.matches("<rect").count(), 12);
        assert_eq!(html.matches("class=unknown").count(), 11);
        assert!(html.contains("target 10/h"));
        assert!(html.contains("href=\"/board?f=waiting\""));
        assert!(html.contains("href=\"/board?f=fix\""));
        assert!(html.contains("Unobserved counts:</b> Held Restricts:, No PR yet"));
        // Absent sources render as "–", while an observed empty source is a real zero.
        assert_eq!(html.matches("<span class=\"v num\">–</span>").count(), 2);
        assert!(html.contains("<span class=\"v num\">0</span>")); // red CI: observed, none
                                                                  // Without any observation the panel says why instead of inventing bars.
        let mut html = String::new();
        let view = View {
            board_cause: Some(crate::lead_input::BoardCause::Missing),
            ..View::default()
        };
        render(&mut html, &view, 100);
        assert!(html.contains("Throughput"));
        assert!(!html.contains("<rect"));
        assert!(html.contains("protected Board observation"));
    }
}
