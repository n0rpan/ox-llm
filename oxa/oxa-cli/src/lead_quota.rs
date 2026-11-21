//! Per-identity quota observations and opt-in Lead admission policy.

use oxa_work::shadow::AppQuota;
pub use oxa_work::shadow::GitHubWindow;
use oxc_gateway::status::chatgpt_usage::{Usage, Window as ProviderWindow};

#[derive(Clone, Copy)]
pub enum GitHubIdentity {
    App,
    Owner,
}

#[derive(Clone, Copy)]
pub enum GitHubResource {
    Core,
    Graphql,
}

#[derive(Default)]
pub struct GitHubQuotas {
    windows: [[Option<GitHubWindow>; 2]; 2],
}

impl GitHubQuotas {
    pub(crate) fn app_sample(&self, now: u64) -> Option<AppQuota> {
        let core = self.window(GitHubIdentity::App, GitHubResource::Core, now);
        let graphql = self.window(GitHubIdentity::App, GitHubResource::Graphql, now);
        (core.is_some() || graphql.is_some()).then_some(AppQuota { core, graphql })
    }

    pub(crate) fn from_app_sample(sample: AppQuota, observed_at: u64, now: u64) -> Option<Self> {
        if now.checked_sub(observed_at)? > 2 * oxa_work::shadow::INTERVAL.as_secs() {
            return None;
        }
        let live = |window: &GitHubWindow| {
            window.limit > 0 && window.remaining <= window.limit && now < window.resets_at
        };
        let app = [sample.core.filter(live), sample.graphql.filter(live)];
        app.iter().any(Option::is_some).then_some(Self {
            windows: [app, [None; 2]],
        })
    }

    /// Record response headers against the identity that authorized the request.
    /// A response never changes another identity or API resource's observation.
    pub fn observe(&mut self, identity: GitHubIdentity, resource: GitHubResource, headers: &str) -> bool {
        let slot = &mut self.windows[identity as usize][resource as usize];
        *slot = read_github_window(resource, headers);
        slot.is_some()
    }

    /// Fill core and graphql from one `rate_limit` response (a read GitHub does not
    /// charge), each from its own entry: a resource the response lacks stays unobserved.
    pub fn observe_rate_limit(&mut self, identity: GitHubIdentity, response: &serde_json::Value) {
        for (resource, key) in [
            (GitHubResource::Core, "core"),
            (GitHubResource::Graphql, "graphql"),
        ] {
            let entry = &response["resources"][key];
            let window = (|| {
                let limit = entry["limit"].as_u64()?;
                let remaining = entry["remaining"].as_u64()?;
                let resets_at = entry["reset"].as_u64()?;
                (limit > 0 && remaining <= limit && resets_at > 0).then_some(GitHubWindow {
                    limit,
                    remaining,
                    resets_at,
                })
            })();
            self.windows[identity as usize][resource as usize] = window;
        }
    }

    pub fn window(
        &self,
        identity: GitHubIdentity,
        resource: GitHubResource,
        now: u64,
    ) -> Option<GitHubWindow> {
        self.windows[identity as usize][resource as usize].filter(|window| now < window.resets_at)
    }

    pub fn admit(
        &self,
        identity: GitHubIdentity,
        resource: GitHubResource,
        now: u64,
        cost: u64,
        reserve: Option<u64>,
    ) -> Admission {
        let Some(reserve) = reserve else {
            return Admission::Proceed;
        };
        let Some(window) = self
            .window(identity, resource, now)
            .filter(|_| reserve > 0 && cost > 0)
        else {
            return Admission::AwaitObservation;
        };
        if window
            .remaining
            .checked_sub(cost)
            .is_some_and(|after| after >= reserve)
        {
            Admission::Proceed
        } else {
            Admission::WaitUntil(window.resets_at)
        }
    }
}

fn read_github_window(resource: GitHubResource, headers: &str) -> Option<GitHubWindow> {
    let names = [
        "x-ratelimit-resource",
        "x-ratelimit-limit",
        "x-ratelimit-remaining",
        "x-ratelimit-reset",
    ];
    let mut values = [None; 4];
    for (name, value) in headers.lines().filter_map(|line| line.split_once(':')) {
        if let Some(index) = names.iter().position(|key| name.eq_ignore_ascii_case(key)) {
            if values[index].replace(value.trim()).is_some() {
                return None;
            }
        }
    }
    let [observed_resource, limit, remaining, resets_at] = values;
    let expected_resource = match resource {
        GitHubResource::Core => "core",
        GitHubResource::Graphql => "graphql",
    };
    if observed_resource? != expected_resource {
        return None;
    }
    let limit = limit?.parse::<u64>().ok()?;
    let remaining = remaining?.parse::<u64>().ok()?;
    let resets_at = resets_at?.parse::<u64>().ok()?;
    (limit > 0 && remaining <= limit && resets_at > 0).then_some(GitHubWindow {
        limit,
        remaining,
        resets_at,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Proceed,
    AwaitObservation,
    WaitUntil(u64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelWindow {
    pub used_percent: f64,
    pub resets_at: u64,
}

/// Preserve both live provider windows in their original order.
pub fn model_windows(usage: &Usage, now: u64) -> [Option<ModelWindow>; 2] {
    let live = |window: &ProviderWindow| {
        let used_percent = window.used_percent?;
        let resets_at = window.reset_at?;
        ((0.0..=100.0).contains(&used_percent) && now < resets_at).then_some(ModelWindow {
            used_percent,
            resets_at,
        })
    };
    [
        usage.primary.as_ref().and_then(live),
        usage.secondary.as_ref().and_then(live),
    ]
}

pub fn admit_model(usage: &Usage, now: u64, stop_at_percent: Option<u8>) -> Admission {
    let Some(stop_at_percent) = stop_at_percent else {
        return Admission::Proceed;
    };
    if !(1..100).contains(&stop_at_percent) {
        return Admission::AwaitObservation;
    }
    let windows = model_windows(usage, now);
    if let Some(until) = windows
        .iter()
        .flatten()
        .filter(|window| window.used_percent >= f64::from(stop_at_percent))
        .map(|window| window.resets_at)
        .max()
    {
        Admission::WaitUntil(until)
    } else if windows.iter().any(Option::is_some)
        && [usage.primary.is_some(), usage.secondary.is_some()]
            .into_iter()
            .zip(windows)
            .all(|(reported, live)| !reported || live.is_some())
    {
        Admission::Proceed
    } else {
        Admission::AwaitObservation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_identity_and_resource_keeps_its_own_reset_and_reserve() {
        let mut quotas = GitHubQuotas::default();
        use GitHubIdentity::{App, Owner};
        use GitHubResource::{Core, Graphql};
        for (identity, resource, label, remaining, reset) in [
            (App, Core, "core", 5, 120),
            (Owner, Core, "core", 50, 150),
            (App, Graphql, "graphql", 40, 130),
        ] {
            assert!(quotas
                .observe(identity, resource, &format!(
                    "X-RateLimit-Resource: {label}\r\nx-ratelimit-limit: 100\r\nx-ratelimit-remaining: {remaining}\r\nx-ratelimit-reset: {reset}\r\n"
                )));
        }
        assert_eq!(
            quotas.admit(App, Core, 100, 2, Some(4)),
            Admission::WaitUntil(120)
        );
        assert_eq!(quotas.admit(App, Core, 100, 2, Some(3)), Admission::Proceed);
        assert_eq!(quotas.admit(Owner, Core, 100, 2, Some(4)), Admission::Proceed);
        assert_eq!(quotas.admit(App, Graphql, 100, 2, Some(4)), Admission::Proceed);
        assert_eq!(
            quotas.admit(Owner, Graphql, 100, 2, Some(4)),
            Admission::AwaitObservation
        );
        assert_eq!(
            quotas.admit(App, Core, 120, 2, Some(4)),
            Admission::AwaitObservation
        );
        assert_eq!(quotas.admit(Owner, Graphql, 100, 2, None), Admission::Proceed);
        assert!(!quotas.observe(App, Core, "x-ratelimit-resource: graphql"));
        assert_eq!(
            quotas.admit(App, Core, 100, 2, Some(4)),
            Admission::AwaitObservation
        );
        assert_eq!(quotas.admit(Owner, Core, 100, 2, Some(4)), Admission::Proceed);
    }

    #[test]
    fn real_provider_shape_keeps_each_window_and_uses_the_last_blocking_reset() {
        let usage = oxc_gateway::status::chatgpt_usage::parse(&json!({
            "plan_type": "synthetic",
            "rate_limit": {
                "primary_window": {"used_percent": 91.5, "reset_at": 120},
                "secondary_window": {"used_percent": 95.2, "reset_at": 130}
            }
        }));
        assert_eq!(
            model_windows(&usage, 100).map(|window| window.unwrap().resets_at),
            [120, 130]
        );
        assert_eq!(admit_model(&usage, 100, Some(90)), Admission::WaitUntil(130));
        assert_eq!(admit_model(&usage, 100, Some(98)), Admission::Proceed);
        assert_eq!(admit_model(&usage, 130, Some(90)), Admission::AwaitObservation);
        assert_eq!(admit_model(&usage, 130, None), Admission::Proceed);
        assert_eq!(admit_model(&usage, 120, Some(98)), Admission::AwaitObservation);
    }
}
