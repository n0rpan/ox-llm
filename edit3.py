import sys
def edit(p, pairs):
    s=open(p).read()
    for pair in pairs:
        a,b=pair
        assert a in s,(p,a[:70])
        s=s.replace(a,b,1)
    open(p,'w').write(s)

edit('oxa-core/src/queue.rs',[
("impl WrapUpReason {\n","""impl WrapUpReason {
    /// What `oxa_result.limit` names: the limit or signal that ended the run.
    pub fn limit(self) -> &'static str {
        match self {
            Self::TimeLimit => "time",
            Self::Signal => "signal",
            Self::TokenLimit => "tokens",
        }
    }

"""),
("    pub image_budget_warned: std::sync::atomic::AtomicBool,\n","""    pub image_budget_warned: std::sync::atomic::AtomicBool,
    /// The run's `--max-time` window and which time notes were already shown.
    pub(crate) time_limit: Mutex<Option<TimeLimit>>,
"""),
("/// The wake a front end registered for state it cannot see through its own channels","""/// The run's time window (`--max-time`) and the notes already pushed into the conversation
/// (bit 0: 70%, bit 1: 90%).
#[derive(Debug, Clone, Copy)]
pub(crate) struct TimeLimit {
    start: std::time::Instant,
    deadline: std::time::Instant,
    noted: u8,
}

/// A time note due at the top of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimeNote {
    pub percent: u8,
    pub minutes_left: u64,
}

/// The wake a front end registered for state it cannot see through its own channels"""),
("    pub(crate) fn remaining_tokens(&self)","""    /// Record the run's time window (`None` = no `--max-time`), for the time notes.
    pub fn set_time_limit(&self, window: Option<(std::time::Instant, std::time::Instant)>) {
        *lock(&self.shared.gate.time_limit) = window.map(|(start, deadline)| TimeLimit {
            start,
            deadline,
            noted: 0,
        });
    }

    /// The next time note that is due (70% then 90% of the window), marked as shown. A late
    /// first look, already past 90%, gives the later note only.
    pub(crate) fn take_time_note(&self) -> Option<TimeNote> {
        self.time_note_at(std::time::Instant::now())
    }

    fn time_note_at(&self, now: std::time::Instant) -> Option<TimeNote> {
        let mut guard = lock(&self.shared.gate.time_limit);
        let limit = guard.as_mut()?;
        let total = limit.deadline.saturating_duration_since(limit.start);
        if total.is_zero() {
            return None;
        }
        let used = now.saturating_duration_since(limit.start).as_secs_f64() / total.as_secs_f64();
        let (percent, bits) = if used >= 0.9 && limit.noted & 2 == 0 {
            (90, 3)
        } else if used >= 0.7 && limit.noted & 1 == 0 {
            (70, 1)
        } else {
            return None;
        };
        limit.noted |= bits;
        Some(TimeNote {
            percent,
            minutes_left: limit.deadline.saturating_duration_since(now).as_secs().div_ceil(60),
        })
    }

    pub(crate) fn remaining_tokens(&self)"""),
("mod tests {\n","""mod tests {
    #[test]
    fn time_notes_come_at_70_and_90_percent_once_each() {
        let h = handle(QueueMode::All, QueueMode::All);
        let start = std::time::Instant::now();
        let at = |secs: u64| start + std::time::Duration::from_secs(secs);
        assert_eq!(h.time_note_at(at(500)), None, "no window yet");
        h.set_time_limit(Some((start, at(1000))));
        assert_eq!(h.time_note_at(at(500)), None);
        let first = h.time_note_at(at(700)).unwrap();
        assert_eq!((first.percent, first.minutes_left), (70, 5));
        assert_eq!(h.time_note_at(at(750)), None, "the 70% note is shown once");
        let second = h.time_note_at(at(950)).unwrap();
        assert_eq!(second.percent, 90);
        assert_eq!(h.time_note_at(at(990)), None);
        // A late first look covers both notes with the later one.
        h.set_time_limit(Some((start, at(1000))));
        assert_eq!(h.time_note_at(at(960)).unwrap().percent, 90);
        assert_eq!(h.time_note_at(at(970)), None);
    }

"""),
])
edit('oxa-core/src/prompt.rs',[("Use this one short final turn to commit only changes made during this run if appropriate, or summarize the current state and remaining work. Leave pre-existing changes alone. Do not start new work.","Use this one final turn to commit what works from the changes made during this run if appropriate, and state what is unfinished. Leave pre-existing changes alone. Do not start new work.")])
edit('oxa-core/src/agent_loop.rs',[
("""        if !wrapping_up {
            let reason = match st.shared.gate.wrap_up""","""        if !wrapping_up {
            // Time awareness: one short note at 70% and one at 90% of `--max-time`. It is a user
            // message at a turn boundary, so the frozen prefix does not change.
            let note = (crate::Handle {
                shared: st.shared.clone(),
            })
            .take_time_note();
            if let Some(note) = note {
                let tail = if note.percent >= 90 {
                    "; commit what works now"
                } else {
                    ""
                };
                let text = format!(
                    "[time: {}% of this run's time limit used, about {} min left{tail}]",
                    note.percent, note.minutes_left
                );
                push_user(st, text, sink, ui, run_msgs)?;
            }
            let reason = match st.shared.gate.wrap_up"""),
("""            if let Some(reason) = reason {
                wrapping_up = true;
                push_user(st, crate::prompt::WRAP_UP.to_string(), sink, ui, run_msgs)?;""","""            if let Some(reason) = reason {
                wrapping_up = true;
                crate::progress::notify(st, crate::Progress::WrapUp { limit: reason.limit() });
                push_user(st, crate::prompt::WRAP_UP.to_string(), sink, ui, run_msgs)?;"""),
("            max_tokens: wrapping_up.then_some(1024),","            max_tokens: wrapping_up.then(|| wrap_up_tokens(st)),"),
])
s=open('oxa-core/src/agent_loop.rs').read()
s+="""
/// Output cap of the wrap-up turn: the owner's `oxa.run.wrapUpTokens`, else the model's own
/// output limit, at most 4096 (room to commit and report).
fn wrap_up_tokens(st: &SessionState) -> u32 {
    let own = st.settings.user_u32("oxa.run.wrapUpTokens", 0);
    if own > 0 {
        return own;
    }
    match st.info.max_output {
        0 => 4096,
        limit => limit.min(4096),
    }
}
"""
open('oxa-core/src/agent_loop.rs','w').write(s)
edit('oxa-core/src/progress.rs',[("    /// An operation returned, including an empty queue","""    /// A limit or SIGTERM turned this run into its wrap-up: `time`, `tokens` or `signal`.
    WrapUp {
        limit: &'static str,
    },
    /// An operation returned, including an empty queue""")])
edit('oxa-cli/src/status.rs',[("            Progress::GoalControlCommitted { .. } => return,","            Progress::GoalControlCommitted { .. } | Progress::WrapUp { .. } => return,")])
edit('oxa-cli/src/result_facts.rs',[
("    prefix: Option<(String, usize)>,\n}","    prefix: Option<(String, usize)>,\n    /// What ended the run early: `time`, `tokens` or `signal`.\n    limit: Option<&'static str>,\n}"),
("            prefix: None,\n        }","            prefix: None,\n            limit: None,\n        }"),
("            Progress::RequestStart { kind, .. } => self.request_start(kind),","""            Progress::WrapUp { limit } => {
                let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if !state.finished && state.limit.is_none() {
                    state.limit = Some(limit);
                }
            }
            Progress::RequestStart { kind, .. } => self.request_start(kind),"""),
("        record[\"usage_scope\"] = json!(\"process\");","""        if let Some(limit) = state.as_ref().and_then(|state| state.limit) {
            record["limit"] = json!(limit);
        }
        record["usage_scope"] = json!("process");"""),
("    #[test]\n    fn web_searches_are_counted_in_the_result() {","""    #[test]
    fn the_result_names_the_limit_that_ended_the_run() {
        let session = oxa_session::SessionRef {
            path: std::path::PathBuf::from("mem:s"),
            id: "s".to_string(),
        };
        let plain = Arc::new(ResultFacts::default());
        plain.session(Some("s"));
        assert!(ResultFacts::record(RunOutcome::Done, Some(&plain)).get("limit").is_none());
        let facts = Arc::new(ResultFacts::default());
        facts.session(Some("s"));
        facts.notify(&session, Progress::WrapUp { limit: "time" });
        facts.notify(&session, Progress::WrapUp { limit: "signal" });
        let record = ResultFacts::record(RunOutcome::Interrupted, Some(&facts));
        assert_eq!(record["limit"], "time", "the first limit stays");
    }

    #[test]
    fn web_searches_are_counted_in_the_result() {"""),
])
