//! Passive subscription allowance headers, independent of transport and credentials.
use std::time::{Duration, SystemTime};

/// Missing, incomplete, malformed, or ambiguous windows are absent, not zero.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Allowance {
    pub primary: Option<Window>,
    pub secondary: Option<Window>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub used_percent: f64,
    pub window_minutes: u64,
    pub resets_at: SystemTime,
}

/// Parse `x-<family>-(primary|secondary)-<field>` headers with an injected clock.
/// Names are ASCII case-insensitive. Each window requires all three fields from
/// the same family; duplicate fields or multiple complete families are ambiguous
/// and ignored. Percentages must be finite decimals in 0..=100, minutes positive
/// integers, and reset delays nonnegative integer seconds. No network or clock
/// reads occur here; callers supply response headers and their receipt time.
pub fn parse<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>, now: SystemTime) -> Allowance {
    // Keep families separate: unrelated providers' partial metadata must never
    // be combined into an apparently complete window.
    let mut families = std::collections::BTreeMap::<(String, usize), Fields<'a>>::new();
    for (name, value) in headers {
        let name = name.to_ascii_lowercase();
        let Some(name) = name.strip_prefix("x-") else {
            continue;
        };
        for (field, suffix) in ["-used-percent", "-window-minutes", "-reset-after-seconds"]
            .into_iter()
            .enumerate()
        {
            let Some(stem) = name.strip_suffix(suffix) else {
                continue;
            };
            for (kind, suffix) in ["-primary", "-secondary"].into_iter().enumerate() {
                let Some(family) = stem.strip_suffix(suffix).filter(|s| !s.is_empty()) else {
                    continue;
                };
                let fields = families.entry((family.to_owned(), kind)).or_default();
                if fields.values[field].replace(value).is_some() {
                    fields.duplicate = true;
                }
            }
        }
    }
    let mut windows = [Vec::new(), Vec::new()];
    for ((_, kind), fields) in families {
        if let Some(window) = fields.window(now) {
            windows[kind].push(window);
        }
    }
    let [primary, secondary] = windows.map(
        |mut windows| {
            if windows.len() == 1 {
                windows.pop()
            } else {
                None
            }
        },
    );
    Allowance { primary, secondary }
}

#[derive(Default)]
struct Fields<'a> {
    values: [Option<&'a str>; 3],
    duplicate: bool,
}

impl Fields<'_> {
    fn window(self, now: SystemTime) -> Option<Window> {
        if self.duplicate {
            return None;
        }
        let [percent, minutes, reset] = self.values;
        let percent = percent?;
        let mut parts = percent.split('.');
        if !digits(parts.next()?) || parts.next().is_some_and(|part| !digits(part)) || parts.next().is_some()
        {
            return None;
        }
        let used_percent: f64 = percent.parse().ok()?;
        if !used_percent.is_finite() || !(0.0..=100.0).contains(&used_percent) {
            return None;
        }
        let window_minutes = unsigned(minutes?)?;
        if window_minutes == 0 {
            return None;
        }
        let resets_at = now.checked_add(Duration::from_secs(unsigned(reset?)?))?;
        Some(Window {
            used_percent,
            window_minutes,
            resets_at,
        })
    }
}

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn unsigned(value: &str) -> Option<u64> {
    if digits(value) {
        value.parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000)
    }

    fn window(used_percent: f64, window_minutes: u64, reset: u64) -> Window {
        Window {
            used_percent,
            window_minutes,
            resets_at: now() + Duration::from_secs(reset),
        }
    }

    #[test]
    fn chatgpt_limits_header_tables() {
        let cases: &[(&[(&str, &str)], Allowance)] = &[
            (&[], Allowance::default()),
            (
                &[
                    ("X-Codex-Primary-Used-Percent", "12.5"),
                    ("x-codex-primary-window-minutes", "300"),
                    ("x-codex-primary-reset-after-seconds", "45"),
                    ("x-codex-secondary-used-percent", "100"),
                    ("x-codex-secondary-window-minutes", "10080"),
                    ("x-codex-secondary-reset-after-seconds", "0"),
                    ("x-ratelimit-remaining-requests", "4"),
                    ("retry-after", "9"),
                ],
                Allowance {
                    primary: Some(window(12.5, 300, 45)),
                    secondary: Some(window(100.0, 10080, 0)),
                },
            ),
            (
                &[
                    ("x-chatgpt-secondary-reset-after-seconds", "60"),
                    ("x-chatgpt-secondary-window-minutes", "60"),
                    ("x-chatgpt-secondary-used-percent", "0"),
                ],
                Allowance {
                    primary: None,
                    secondary: Some(window(0.0, 60, 60)),
                },
            ),
            (
                &[
                    ("x-codex-primary-used-percent", "25"),
                    ("x-other-primary-window-minutes", "300"),
                    ("x-codex-primary-reset-after-seconds", "10"),
                    ("codex-secondary-used-percent", "25"),
                    ("codex-secondary-window-minutes", "300"),
                    ("codex-secondary-reset-after-seconds", "10"),
                ],
                Allowance::default(),
            ),
        ];
        for (headers, expected) in cases {
            assert_eq!(parse(headers.iter().copied(), now()), *expected, "{headers:?}");
        }
    }

    #[test]
    fn chatgpt_limits_malformed_and_missing_fields_are_ignored() {
        let fields = [
            "x-codex-primary-used-percent",
            "x-codex-primary-window-minutes",
            "x-codex-primary-reset-after-seconds",
        ];
        let bad_values: [&[&str]; 3] = [
            &[
                "", "NaN", "inf", "-1", "100.1", "+1", "1e1", " 1", "1 ", "1.2.3", "é",
            ],
            &["", "0", "-1", "1.5", "+1", "1e1", " 1", "18446744073709551616"],
            &[
                "",
                "-1",
                "1.5",
                "+1",
                "1e1",
                "1s",
                " 1",
                "18446744073709551616",
                "18446744073709551615",
            ],
        ];
        for index in 0..3 {
            for bad in bad_values[index].iter().copied().map(Some).chain([None]) {
                let mut headers = vec![
                    (fields[0], "50"),
                    (fields[1], "300"),
                    (fields[2], "60"),
                    ("x-codex-secondary-used-percent", "25"),
                    ("x-codex-secondary-window-minutes", "10080"),
                    ("x-codex-secondary-reset-after-seconds", "120"),
                ];
                if let Some(bad) = bad {
                    headers[index].1 = bad;
                } else {
                    headers.remove(index);
                }
                assert_eq!(
                    parse(headers, now()),
                    Allowance {
                        primary: None,
                        secondary: Some(window(25.0, 10080, 120))
                    },
                    "field {index}, value {bad:?}"
                );
            }
        }
    }

    #[test]
    fn chatgpt_limits_duplicate_fields_and_families_are_ambiguous() {
        let headers = [
            ("x-codex-primary-used-percent", "50"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-after-seconds", "60"),
        ];
        for (name, value) in headers {
            let duplicate = headers.into_iter().chain([(name, value)]);
            assert_eq!(parse(duplicate, now()), Allowance::default());
        }
        let other = [
            ("x-other-primary-used-percent", "10"),
            ("x-other-primary-window-minutes", "60"),
            ("x-other-primary-reset-after-seconds", "10"),
        ];
        assert_eq!(
            parse(headers.into_iter().chain(other), now()),
            Allowance::default()
        );
    }

    #[test]
    fn chatgpt_limits_clock_is_injected_and_overflow_is_ignored() {
        let headers = [
            ("x-codex-primary-used-percent", "50"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-after-seconds", "60"),
        ];
        let later = now() + Duration::from_secs(123);
        assert_eq!(
            parse(headers, later).primary.unwrap().resets_at,
            later + Duration::from_secs(60)
        );
        // Find the platform's last representable whole second without assuming
        // the width of SystemTime, then ensure the relative addition is checked.
        let mut seconds = 0u64;
        for bit in (0..64).rev() {
            let candidate = seconds | (1u64 << bit);
            if SystemTime::UNIX_EPOCH
                .checked_add(Duration::from_secs(candidate))
                .is_some()
            {
                seconds = candidate;
            }
        }
        let last = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        assert_eq!(parse(headers, last), Allowance::default());
    }
}
