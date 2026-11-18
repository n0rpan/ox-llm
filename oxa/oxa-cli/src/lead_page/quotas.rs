use super::View;
use crate::lead_quota::{model_windows, GitHubIdentity, GitHubResource};
use std::fmt::Write;

pub(super) fn render(html: &mut String, view: &View<'_>, now: u64) {
    html.push_str("<section><h2>Plans and quotas</h2>");
    if view.github_quotas.is_none() && view.model_plan.is_none() {
        html.push_str("<p>Usage observations unavailable.</p></section>");
        return;
    }
    html.push_str("<dl>");
    for (identity, label) in [(GitHubIdentity::App, "App"), (GitHubIdentity::Owner, "Owner")] {
        for (resource, api) in [
            (GitHubResource::Core, "REST core"),
            (GitHubResource::Graphql, "GraphQL"),
        ] {
            let _ = write!(html, "<dt>GitHub {label} · {api}</dt>");
            if let Some(window) = view
                .github_quotas
                .and_then(|quotas| quotas.window(identity, resource, now))
            {
                let _ = write!(
                    html,
                    "<dd>{} of {} remaining · resets at {} (Unix seconds)</dd>",
                    window.remaining, window.limit, window.resets_at
                );
            } else {
                html.push_str("<dd>Unavailable</dd>");
            }
        }
    }
    let plan = view
        .model_plan
        .and_then(|usage| usage.plan_type.as_deref())
        .filter(|plan| {
            !plan.is_empty()
                && plan.len() <= 40
                && plan
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        });
    let _ = write!(
        html,
        "<dt>Model plan</dt><dd>{}</dd>",
        plan.unwrap_or("Unavailable")
    );
    let windows = view
        .model_plan
        .map_or([None; 2], |usage| model_windows(usage, now));
    for (label, window) in ["Primary", "Secondary"].into_iter().zip(windows) {
        let _ = write!(html, "<dt>{label} model window</dt>");
        if let Some(window) = window {
            let _ = write!(
                html,
                "<dd>{}% used · resets at {} (Unix seconds)</dd>",
                window.used_percent, window.resets_at
            );
        } else {
            html.push_str("<dd>Unavailable</dd>");
        }
    }
    html.push_str("</dl></section>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lead_page::{document_for, Page};
    use crate::lead_quota::GitHubQuotas;
    use serde_json::json;

    #[test]
    fn usage_page_shows_live_windows_without_inventing_missing_quotas() {
        let mut quotas = GitHubQuotas::default();
        let headers = "x-ratelimit-resource: core\nx-ratelimit-limit: 5000\nx-ratelimit-remaining: 0\nx-ratelimit-reset: 120";
        assert!(quotas.observe(GitHubIdentity::App, GitHubResource::Core, headers));
        let model = oxc_gateway::status::chatgpt_usage::parse(&json!({
            "plan_type": "fixture-plan",
            "credits": {"balance": "fixture-private-field"},
            "rate_limit": {
                "primary_window": {"used_percent": 12.5, "reset_at": 120},
                "secondary_window": {"used_percent": 85, "reset_at": 180}
            }
        }));
        let html = document_for(
            Page::Usage,
            &View {
                github_quotas: Some(&quotas),
                model_plan: Some(&model),
                ..Default::default()
            },
            100,
        );
        assert!(html.contains("GitHub App · REST core</dt><dd>0 of 5000 remaining"));
        assert!(html.contains("GitHub Owner · REST core</dt><dd>Unavailable"));
        assert!(html.contains("Model plan</dt><dd>fixture-plan"));
        assert!(html.contains("Primary model window</dt><dd>12.5% used"));
        assert!(html.contains("Secondary model window</dt><dd>85% used"));
        assert!(!html.contains("fixture-private-field"));
    }
}
