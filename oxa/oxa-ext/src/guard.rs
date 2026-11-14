//! E2: the W-guard. Owner: task E2. Signatures below are frozen; bodies are yours.
use oxa_proto::{codes, GuardClass, HookPoint, HookReply, OxaError, WireMsg};

/// What the guard decided about one reply.
#[derive(Debug, Clone, PartialEq)]
pub enum GuardVerdict {
    /// The reply is allowed as is.
    Pass(HookReply),
    /// The reply is not one this point accepts (an observer returned something, or the reply
    /// kind is not in the point's table on `HookReply`). Treated as `Continue`; `reason` goes
    /// to the log as a warning.
    Ignored { reason: String },
    /// A prefix-checked point returned a list that does not extend the current wire and the
    /// extension did not declare `rebase: true`. Treated as `Continue`; the error
    /// (`code = "guard_refused"`) is reported as an `ExtensionError`.
    Refused(OxaError),
    /// A declared rebase: allowed, and the caller must log it as a cache-miss cause
    /// (`miss_cause::EXTENSION`) and rebase with `rebase_why::EXTENSION`.
    DeclaredRebase(Vec<WireMsg>),
}

/// Short name of a reply kind, for messages.
fn kind_name(reply: &HookReply) -> &'static str {
    match reply {
        HookReply::Continue => "continue",
        HookReply::Cancel(_) => "cancel",
        HookReply::Block(_) => "block",
        HookReply::ReplaceInput(_) => "replace_input",
        HookReply::Handled => "handled",
        HookReply::ReplaceArgs(_) => "replace_args",
        HookReply::ReplaceResult(_) => "replace_result",
        HookReply::Append(_) => "append",
        HookReply::Rebase(_) | HookReply::DeclaredRebase { .. } => "rebase",
        HookReply::Summary { .. } => "summary",
        HookReply::Trust(_) | HookReply::TrustRemembered(_) => "trust",
    }
}

/// The W-guard (plan sec. 2). Pure.
///
/// * `GuardClass::Observer`: `Continue` passes; anything else is `Ignored`.
/// * `GuardClass::AppendSafe`: a reply listed for `point` in the table on
///   [`oxa_proto::HookReply`] passes; other kinds are `Ignored`. `Rebase` is always `Ignored`
///   here.
/// * `GuardClass::PrefixChecked`: `Continue` passes. `Rebase(list)` passes as
///   `Pass(Rebase(list))` when `oxa_proto::wire_is_prefix(current_wire, &list)` (a pure append,
///   including the identical list); otherwise `DeclaredRebase(list)` if `declared_rebase`,
///   else `Refused`. Other kinds are `Ignored`.
pub fn check(
    point: HookPoint,
    current_wire: &[WireMsg],
    reply: HookReply,
    declared_rebase: bool,
) -> GuardVerdict {
    let ignored = |reply: &HookReply| GuardVerdict::Ignored {
        reason: format!(
            "hook point \"{}\" does not accept a \"{}\" reply; ignored",
            point.as_str(),
            kind_name(reply)
        ),
    };
    match point.guard() {
        GuardClass::Observer => match reply {
            HookReply::Continue => GuardVerdict::Pass(HookReply::Continue),
            other => ignored(&other),
        },
        GuardClass::AppendSafe => {
            if reply == HookReply::Continue || accepts(point, &reply) {
                GuardVerdict::Pass(reply)
            } else {
                ignored(&reply)
            }
        }
        GuardClass::PrefixChecked => match reply {
            HookReply::Continue => GuardVerdict::Pass(HookReply::Continue),
            HookReply::Rebase(list) => {
                if oxa_proto::wire_is_prefix(current_wire, &list) {
                    GuardVerdict::Pass(HookReply::Rebase(list))
                } else if declared_rebase {
                    GuardVerdict::DeclaredRebase(list)
                } else {
                    GuardVerdict::Refused(OxaError {
                        code: codes::GUARD_REFUSED,
                        message: format!(
                            "\"{}\" returned a message list that does not extend the current request; \
                             the request is sent unchanged",
                            point.as_str()
                        ),
                        hint: Some(
                            "return the current messages unchanged and append to them, or declare \"rebase\": true \
                             in the extension to allow a logged rebase"
                                .to_string(),
                        ),
                    })
                }
            }
            other => ignored(&other),
        },
    }
}

/// `true` when `reply`'s kind is accepted at `point` (the table on `HookReply`).
pub fn accepts(point: HookPoint, reply: &HookReply) -> bool {
    use HookPoint as P;
    use HookReply as R;
    matches!(
        (point, reply),
        (P::ProjectTrust, R::Trust(_) | R::TrustRemembered(_))
            | (P::Input, R::ReplaceInput(_) | R::Handled)
            | (P::UserBash, R::ReplaceResult(_) | R::Block(_))
            | (P::BeforeAgentStart, R::Append(_))
            | (P::ToolCall, R::Block(_) | R::ReplaceArgs(_))
            | (P::ToolResult, R::ReplaceResult(_))
            | (
                P::SessionBeforeCompact | P::SessionBeforeTree,
                R::Cancel(_) | R::Summary { .. }
            )
            | (P::SessionBeforeSwitch | P::SessionBeforeFork, R::Cancel(_))
            | (P::AgentBeforeSettle, R::Append(_))
            | (P::CacheWarmingDecision, R::Cancel(_))
            | (
                P::Context | P::ContextWithSystem | P::BeforeProviderRequest,
                R::Rebase(_)
            )
    )
}
