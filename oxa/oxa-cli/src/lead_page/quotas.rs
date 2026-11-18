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

    fn usage_page(
        github_quotas: Option<&GitHubQuotas>,
        model_plan: Option<&oxc_gateway::status::chatgpt_usage::Usage>,
        now: u64,
    ) -> String {
        document_for(
            Page::Usage,
            &View {
                github_quotas,
                model_plan,
                ..Default::default()
            },
            now,
        )
    }

    #[test]
    fn quota_meters_keep_identity_resource_units_and_measured_zero_explicit() {
        let mut quotas = GitHubQuotas::default();
        for (identity, resource, name, limit, remaining) in [
            (GitHubIdentity::App, GitHubResource::Core, "core", 5000, 0),
            (GitHubIdentity::App, GitHubResource::Graphql, "graphql", 100, 75),
            (GitHubIdentity::Owner, GitHubResource::Core, "core", 500, 200),
        ] {
            let headers = format!("x-ratelimit-resource: {name}\nx-ratelimit-limit: {limit}\nx-ratelimit-remaining: {remaining}\nx-ratelimit-reset: 180");
            assert!(quotas.observe(identity, resource, &headers));
        }
        let model = oxc_gateway::status::chatgpt_usage::parse(&json!({
            "plan_type": "fixture-plan", "credits": {"balance": "fixture-private-field"},
            "rate_limit": {
                "primary_window": {"used_percent": 0, "reset_at": 180},
                "secondary_window": {"used_percent": 100, "reset_at": 180}
            }
        }));
        let html = usage_page(Some(&quotas), Some(&model), 100);
        let meters: Vec<_> = html
            .split("<meter ")
            .skip(1)
            .map(|part| part.split("</meter>").next().unwrap())
            .collect();
        assert_eq!(meters.len(), 5);
        for (label, max, value, description) in [
            (
                "GitHub App · REST core remaining",
                "5000",
                "0",
                "0 of 5000 remaining",
            ),
            (
                "GitHub App · GraphQL remaining",
                "100",
                "75",
                "75 of 100 remaining",
            ),
            (
                "GitHub Owner · REST core remaining",
                "500",
                "200",
                "200 of 500 remaining",
            ),
            ("Primary model window used", "100", "0", "0% used"),
            ("Secondary model window used", "100", "100", "100% used"),
        ] {
            let meter = meters
                .iter()
                .find(|meter| meter.contains(&format!("aria-label=\"{label}\"")))
                .unwrap();
            assert!(meter.contains(&format!("min=0 max={max} value={value} ")));
            assert!(meter.contains(&format!("aria-valuetext=\"{description}\"")));
        }
        assert!(html.contains("GitHub Owner · GraphQL</dt><dd>Unavailable"));
        assert!(!html.contains("fixture-private-field"));
        assert!(!html.contains("<form") && !html.contains("<button"));
    }

    #[test]
    fn missing_invalid_and_expired_quota_windows_do_not_become_zero_meters() {
        let mut quotas = GitHubQuotas::default();
        for (identity, remaining, resets_at) in
            [(GitHubIdentity::App, 0, 120), (GitHubIdentity::Owner, 450, 180)]
        {
            let headers = format!("x-ratelimit-resource: core\nx-ratelimit-limit: 5000\nx-ratelimit-remaining: {remaining}\nx-ratelimit-reset: {resets_at}");
            assert!(quotas.observe(identity, GitHubResource::Core, &headers));
        }
        let model = oxc_gateway::status::chatgpt_usage::parse(&json!({
            "rate_limit": {"primary_window": {"used_percent": 0, "reset_at": 120}}
        }));
        let boundary = usage_page(Some(&quotas), Some(&model), 120);
        assert_eq!(boundary.matches("<meter ").count(), 1);
        assert!(boundary.contains("aria-label=\"GitHub Owner · REST core remaining\""));
        assert!(boundary.contains("GitHub App · REST core</dt><dd>Unavailable"));
        assert!(boundary.contains("Primary model window</dt><dd>Unavailable"));
        assert!(boundary.contains("Secondary model window</dt><dd>Unavailable"));
        assert!(!usage_page(Some(&quotas), Some(&model), 180).contains("<meter "));
        let unknown = usage_page(None, None, 100);
        assert!(unknown.contains("Usage observations unavailable."));
        assert!(!unknown.contains("<meter "));
        let invalid = oxc_gateway::status::chatgpt_usage::parse(&json!({
            "rate_limit": {
                "primary_window": {"used_percent": 101, "reset_at": 180},
                "secondary_window": {"used_percent": -1, "reset_at": 180}
            }
        }));
        assert!(!usage_page(None, Some(&invalid), 100).contains("<meter "));
    }
}
