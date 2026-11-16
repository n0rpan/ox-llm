use super::{Completed, Error};
use crate::{accounting::Usage, anthropic_client, anthropic_stream};
use std::sync::Arc;

pub(super) fn anthropic(completed: anthropic_stream::Completed) -> Completed {
    let output_items: Arc<[_]> = completed.native_blocks.into();
    let replay_items = if output_items
        .iter()
        .any(|item| matches!(item["type"].as_str(), Some("thinking" | "redacted_thinking")))
    {
        Arc::clone(&output_items)
    } else {
        Arc::from([])
    };
    Completed {
        assistant: completed.assistant,
        usage: completed.usage.map(|usage| Usage {
            prompt: Some(
                usage.input_tokens + usage.cache_read_input_tokens + usage.cache_creation_input_tokens,
            ),
            output: Some(usage.output_tokens),
            cache_read: Some(usage.cache_read_input_tokens),
            cache_write: Some(usage.cache_creation_input_tokens),
            finish: Some(completed.finish_reason.into()),
            ..Usage::default()
        }),
        output_items,
        replay_items,
        finish_reason: completed.finish_reason,
        history: None,
    }
}
pub(super) fn cancelled(error: &Error) -> bool {
    match error {
        Error::Upstream(anthropic_client::Error::Cancelled) => true,
        #[cfg(feature = "upstream-openai")]
        Error::OpenAi(crate::openai::client::Error::Cancelled) => true,
        _ => false,
    }
}
pub(super) fn clean_refusal(error: &Error) -> bool {
    match error {
        Error::Upstream(anthropic_client::Error::Http(400 | 401 | 403 | 404 | 413 | 429)) => true,
        #[cfg(feature = "upstream-openai")]
        Error::OpenAi(crate::openai::client::Error::Http {
            status: 400 | 401 | 403 | 404 | 413 | 429,
            ..
        }) => true,
        _ => false,
    }
}
#[cfg(feature = "upstream-openai")]
pub(super) fn openai_refusal(error: &crate::openai::client::Error) -> crate::policy::Refusal {
    use crate::openai::client::{Error, Refusal};
    let (status, code, param, message) = match error {
        Error::Cancelled => (499, "request_cancelled", "", "Request cancelled."),
        Error::Request(param) => (400, "invalid_value", *param, "Invalid Responses request."),
        Error::Credentials | Error::Refresh => (
            503,
            "cloud_credentials_unavailable",
            "model",
            "Subscription credentials are unavailable.",
        ),
        Error::Setup => (
            503,
            "cloud_transport_unavailable",
            "",
            "Cloud transport is unavailable.",
        ),
        Error::Endpoint => (
            503,
            "cloud_upstream_unavailable",
            "model",
            "Invalid Responses endpoint.",
        ),
        Error::Http { status, refusal } => {
            let code = match refusal {
                Some(Refusal::Api(crate::rate_limits::Refusal::OutOfCredit)) => "cloud_out_of_credit",
                Some(Refusal::Subscription(crate::rate_limits::chatgpt::Refusal::UsageLimitReached {
                    ..
                })) => "subscription_usage_limit_reached",
                Some(Refusal::Subscription(crate::rate_limits::chatgpt::Refusal::UsageNotIncluded)) => {
                    "subscription_usage_not_included"
                }
                _ => "cloud_upstream_error",
            };
            (
                if (400..600).contains(status) { *status } else { 502 },
                code,
                "",
                "The Responses upstream refused the request.",
            )
        }
        Error::Protocol => (
            502,
            "cloud_upstream_stream_broken",
            "",
            "Invalid Responses stream.",
        ),
        Error::TooLarge => (
            502,
            "cloud_upstream_too_large",
            "",
            "Responses stream exceeds its size limit.",
        ),
        Error::Transport => (
            502,
            "cloud_upstream_unavailable",
            "",
            "The Responses upstream request failed.",
        ),
    };
    crate::policy::Refusal {
        status,
        code,
        param,
        message,
    }
}
