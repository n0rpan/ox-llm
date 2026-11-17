//! Opt-in, ephemeral facts for the trusted CLI. Never part of an error or ledger.
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    LocalCredentialRead,
    DiscoveryHttp,
    ChatHttp,
}

/// Closed local validation categories; never contains upstream data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamCause {
    ContentType,
    Transport,
    Eof,
    FrameShape,
    ToolShape,
    ToolArguments,
    TerminalUsage,
}
impl StreamCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContentType => "content_type",
            Self::Transport => "transport",
            Self::Eof => "eof",
            Self::FrameShape => "frame_shape",
            Self::ToolShape => "tool_shape",
            Self::ToolArguments => "tool_arguments",
            Self::TerminalUsage => "terminal_usage",
        }
    }
}

/// Which existing terminal-usage rejection fired; never contains provider values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalCheck {
    DoneWithoutFinish,
    DoneWithoutUsage,
    FrameAfterUsage,
    ChoicesAfterFinish,
    UsageBeforeFinish,
    UsageRecordInvalid,
}
impl TerminalCheck {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DoneWithoutFinish => "done_without_finish",
            Self::DoneWithoutUsage => "done_without_usage",
            Self::FrameAfterUsage => "frame_after_usage",
            Self::ChoicesAfterFinish => "choices_after_finish",
            Self::UsageBeforeFinish => "usage_before_finish",
            Self::UsageRecordInvalid => "usage_record_invalid",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub stage: Stage,
    pub status: Option<u16>,
    pub code: Option<&'static str>,
    pub cause: Option<StreamCause>,
    pub terminal_check: Option<TerminalCheck>,
    pub billing_source: Option<crate::opencode::PlanName>,
    /// Fixed order: Retry-After, limit, remaining, reset. Numeric values only.
    pub rate: [Option<u32>; 4],
}

/// One native CLI operation at a time; retries clear the preceding operation.
/// API callers do not install this sink. No serialization or user supplied text.
#[derive(Clone, Default)]
pub struct Diagnostics(Arc<Mutex<Option<Snapshot>>>, Arc<Mutex<Option<BillingFact>>>);
impl std::fmt::Debug for Diagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Diagnostics([private])")
    }
}
#[derive(Clone, Copy)]
pub struct BillingFact {
    pub plan: crate::opencode::PlanName,
    pub cost: f64,
}
impl BillingFact {
    pub fn record(self) -> Value {
        let display = if self.cost > 0.0 {
            format!("billed USD {:.2}", self.cost)
        } else {
            format!("plan {}", self.plan.as_str())
        };
        serde_json::json!({"provider":"opencode","scope":"last_settled_call_in_process","display":display,"cost_usd":self.cost,"plan":self.plan.as_str()})
    }
}
impl Diagnostics {
    pub fn last_billing(&self) -> Option<BillingFact> {
        self.1.lock().ok().and_then(|v| *v)
    }
    pub(crate) fn settled_billing(&self, plan: crate::opencode::PlanName, cost: f64) {
        if cost.is_finite() && cost >= 0.0 {
            if let Ok(mut slot) = self.1.lock() {
                *slot = Some(BillingFact { plan, cost });
            }
        }
    }

    pub fn take(&self) -> Option<Snapshot> {
        self.0.lock().ok()?.take()
    }
    pub(crate) fn clear(&self) {
        let _ = self.take();
    }
    pub(crate) fn local(&self) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(Snapshot {
                stage: Stage::LocalCredentialRead,
                status: None,
                code: None,
                cause: None,
                terminal_check: None,
                billing_source: None,
                rate: [None; 4],
            });
        }
    }
    pub(crate) fn http(&self, stage: Stage, status: u16, headers: &HeaderMap) {
        let names = [
            "retry-after",
            "x-ratelimit-limit",
            "x-ratelimit-remaining",
            "x-ratelimit-reset",
        ];
        let rate = names.map(|name| {
            let mut values = headers.get_all(name).iter();
            let text = values.next()?.to_str().ok()?;
            if values.next().is_some()
                || text.is_empty()
                || text.len() > 10
                || !text.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            text.parse::<u32>().ok()
        });
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(Snapshot {
                stage,
                status: Some(status),
                code: None,
                cause: None,
                terminal_check: None,
                billing_source: None,
                rate,
            });
        }
    }
    pub(crate) fn stream_cause(&self, cause: StreamCause) {
        if let Ok(mut slot) = self.0.lock() {
            if let Some(snapshot) = slot.as_mut() {
                snapshot.cause = Some(cause);
                snapshot.terminal_check = None;
            }
        }
    }
    pub(crate) fn terminal_check(&self, check: TerminalCheck) {
        if let Ok(mut slot) = self.0.lock() {
            if let Some(snapshot) = slot.as_mut() {
                snapshot.cause = Some(StreamCause::TerminalUsage);
                snapshot.terminal_check = Some(check);
            }
        }
    }
    pub(crate) fn billing_source(&self, plan: crate::opencode::PlanName) {
        if let Ok(mut slot) = self.0.lock() {
            if let Some(snapshot) = slot.as_mut() {
                snapshot.billing_source = Some(plan);
            }
        }
    }
    pub(crate) fn error_code(&self, body: &Value) {
        let known = |value: &Value| match value.as_str()? {
            "RateLimit" => Some("RateLimit"),
            "RateLimit.Exceeded" => Some("RateLimit.Exceeded"),
            "rate_limit_error" => Some("rate_limit_error"),
            "inference_failed" => Some("inference_failed"),
            "invalid_request_error" => Some("invalid_request_error"),
            "authentication_error" => Some("authentication_error"),
            _ => None,
        };
        let code = known(&body["error"]["type"]).or_else(|| known(&body["error"]["code"]));
        if let Ok(mut slot) = self.0.lock() {
            if let Some(snapshot) = slot.as_mut() {
                snapshot.code = code;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_checks_remain_finite_and_do_not_survive_snapshot_replacement() {
        let diagnostics = Diagnostics::default();
        for (check, label) in [
            (TerminalCheck::DoneWithoutFinish, "done_without_finish"),
            (TerminalCheck::DoneWithoutUsage, "done_without_usage"),
            (TerminalCheck::FrameAfterUsage, "frame_after_usage"),
            (TerminalCheck::ChoicesAfterFinish, "choices_after_finish"),
            (TerminalCheck::UsageBeforeFinish, "usage_before_finish"),
            (TerminalCheck::UsageRecordInvalid, "usage_record_invalid"),
        ] {
            assert_eq!(check.as_str(), label);
            diagnostics.http(Stage::ChatHttp, 200, &HeaderMap::new());
            diagnostics.terminal_check(check);
            let snapshot = diagnostics.take().unwrap();
            assert_eq!(snapshot.cause, Some(StreamCause::TerminalUsage));
            assert_eq!(snapshot.terminal_check, Some(check));
            assert!(diagnostics.take().is_none());
            for replace in 0..4 {
                diagnostics.http(Stage::ChatHttp, 200, &HeaderMap::new());
                diagnostics.terminal_check(check);
                match replace {
                    0 => diagnostics.local(),
                    1 => diagnostics.http(Stage::DiscoveryHttp, 503, &HeaderMap::new()),
                    2 => diagnostics.stream_cause(StreamCause::Transport),
                    _ => diagnostics.clear(),
                }
                assert!(diagnostics.take().is_none_or(|s| s.terminal_check.is_none()));
            }
        }
    }
}
