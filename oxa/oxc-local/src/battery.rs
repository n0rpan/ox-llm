//! N9 (design and fixture contract): an opt-in battery decode team.
//!
//! The policy is inert unless the user-only per-model setting `battery_decode_team` declares
//! a smaller CPU team. It runs once per child launch (cold load or model switch), reads one
//! read-only power source and the process CPU mask, and can only rewrite the `--oxe-cpus`
//! word that the installed arguments already selected. It never writes sysfs or a governor,
//! never polls in the background, never starts a thread and never changes a running team
//! (the supervisor's [`Config`](crate::Config) is fixed for the child's lifetime).
//!
//! Nothing here is a measurement: no energy, speed or efficiency number is produced or
//! claimed, and the owner power row that may later pick a default is a separate,
//! pre-registered gate (v0.2.3). See `oxa/docs/design/battery-decode-team.md`.

use std::path::Path;

/// Linux's read-only power source tree. The caller may inject another root (fixtures, or a
/// platform with the same layout); nothing here writes to it.
pub const SYSFS_POWER_SUPPLY: &str = "/sys/class/power_supply";

/// The process CPU mask, read-only. Absent on platforms without it.
pub const PROC_STATUS: &str = "/proc/self/status";

/// What the power source reports. Only [`Power::Discharging`] is a battery signal;
/// `Ac`, `Charging` and `Unknown` all keep the installed team.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    /// On mains (a mains/USB source is online, or the battery is full/not charging).
    Ac,
    /// A system battery is charging: the load is still supplied.
    Charging,
    /// A system battery is discharging.
    Discharging,
    /// Missing, unreadable, malformed or contradictory data. Never treated as battery.
    Unknown,
}

/// Why a launch keeps the installed decode team.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    /// No `battery_decode_team` setting: the policy is off (default).
    NotOptedIn,
    /// The user's own arguments choose CPUs; an explicit choice always wins.
    Explicit,
    /// The power source is not a discharging system battery.
    Power(Power),
    /// The declared team is not a strict, usable subset of the installed team.
    Unusable,
    /// The CPU mask is unknown or leaves no usable CPU.
    Masked,
}

/// The launch decision: keep the installed CPU list exactly, or use a smaller team.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Installed(Keep),
    /// A non-empty, strict subset of the installed team, in the declared order.
    Reduced(Vec<usize>),
}

/// The pure policy. Every input is injected, so no test needs a battery, a mask or a child.
///
/// * `power` — the read-only power source state.
/// * `installed` — the decode team the installed arguments selected (already qualified).
/// * `declared` — the opt-in team; `None` means the setting is absent (policy off).
/// * `explicit` — the user supplied their own CPU list; it always wins.
/// * `allowed` — CPUs this process may run on; `None` = the mask could not be read.
///
/// Unknown or unreadable inputs keep the installed team; the policy never guesses.
pub fn decide(
    power: Power,
    installed: &[usize],
    declared: Option<&[usize]>,
    explicit: bool,
    allowed: Option<&[usize]>,
) -> Decision {
    let Some(declared) = declared else {
        return Decision::Installed(Keep::NotOptedIn);
    };
    if explicit {
        return Decision::Installed(Keep::Explicit);
    }
    if power != Power::Discharging {
        return Decision::Installed(Keep::Power(power));
    }
    // A strict subset of the installed team: already supported, and smaller.
    if declared.is_empty()
        || declared.len() >= installed.len()
        || declared.iter().any(|cpu| !installed.contains(cpu))
    {
        return Decision::Installed(Keep::Unusable);
    }
    let Some(allowed) = allowed else {
        return Decision::Installed(Keep::Masked);
    };
    let usable: Vec<usize> = declared
        .iter()
        .copied()
        .filter(|cpu| allowed.contains(cpu))
        .collect();
    if usable.is_empty() || usable.len() >= installed.len() {
        return Decision::Installed(Keep::Masked);
    }
    Decision::Reduced(usable)
}

/// Parse a CPU list in the engine's documented `--oxe-cpus` grammar ("0-7", "0,1,4,5",
/// "0-3,8-11"): comma-separated decimal indices or inclusive ranges, no duplicates, order
/// preserved. Kept in step with `engine/crates/sched/src/lib.rs` `parse_cpu_list`.
pub fn parse_cpus(list: &str) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for part in list.split(',').map(str::trim).filter(|part| !part.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (usize, usize) = (a.parse().ok()?, b.parse().ok()?);
                if b < a {
                    return None;
                }
                out.extend(a..=b);
            }
            None => out.push(part.parse().ok()?),
        }
    }
    if out.is_empty() {
        return None;
    }
    let mut seen = out.clone();
    seen.sort_unstable();
    seen.dedup();
    if seen.len() != out.len() {
        return None;
    }
    Some(out)
}

/// The canonical form of [`parse_cpus`]'s own grammar, in the given order.
pub fn format_cpus(cpus: &[usize]) -> String {
    cpus.iter().map(usize::to_string).collect::<Vec<_>>().join(",")
}

/// Read the power source state without writing anything.
///
/// A missing tree, an unreadable entry, a malformed value or a non-`System` battery all
/// yield [`Power::Unknown`]: the policy then keeps the installed team.
pub fn read_power(root: &Path) -> Power {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Power::Unknown,
    };
    let mut battery = false;
    let mut discharging = false;
    let mut charging = false;
    let mut settled = false;
    let mut mains_online = false;
    for entry in entries {
        let Ok(entry) = entry else {
            return Power::Unknown;
        };
        let device = entry.path();
        let Ok(kind) = read_value(&device.join("type")) else {
            return Power::Unknown;
        };
        if kind == "Battery" {
            // A peripheral battery (mouse, headset) does not power the machine; an
            // unclassifiable scope is never assumed to be the system battery.
            match std::fs::read_to_string(device.join("scope")) {
                Ok(scope) => match scope.trim() {
                    "Device" => continue,
                    "System" => {}
                    _ => return Power::Unknown,
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Power::Unknown,
            }
            battery = true;
            let Ok(status) = read_value(&device.join("status")) else {
                return Power::Unknown;
            };
            match status.as_str() {
                "Discharging" => discharging = true,
                "Charging" => charging = true,
                "Full" | "Not charging" => settled = true,
                // A driver value this policy does not know: never guess a battery load.
                _ => return Power::Unknown,
            }
        } else {
            let Ok(online) = read_value(&device.join("online")) else {
                // A source without an `online` file is not a mains source (e.g. a fan).
                continue;
            };
            match online.as_str() {
                "1" => mains_online = true,
                "0" => {}
                _ => return Power::Unknown,
            }
        }
    }
    if discharging {
        Power::Discharging
    } else if charging {
        Power::Charging
    } else if (battery && settled) || (!battery && mains_online) {
        Power::Ac
    } else {
        Power::Unknown
    }
}

fn read_value(path: &Path) -> Result<String, ()> {
    let value = std::fs::read_to_string(path).map_err(|_| ())?;
    let value = value.trim();
    if value.is_empty() {
        return Err(());
    }
    Ok(value.to_owned())
}

/// The CPUs this process may run on, read from `/proc/self/status` (`None` = unreadable).
pub fn allowed_mask() -> Option<Vec<usize>> {
    read_allowed_mask(Path::new(PROC_STATUS))
}

/// [`allowed_mask`] against an injected status file (fixtures), same format.
pub fn read_allowed_mask(status: &Path) -> Option<Vec<usize>> {
    let text = std::fs::read_to_string(status).ok()?;
    let list = text
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))?
        .trim();
    parse_cpus(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAM: &[usize] = &[0, 1, 2, 3, 4, 5, 6, 7];
    const SMALL: &[usize] = &[0, 1, 2, 3];
    const ALL: &[usize] = &[0, 1, 2, 3, 4, 5, 6, 7];

    #[test]
    fn off_explicit_power_and_unusable_inputs_keep_the_installed_team() {
        assert_eq!(
            decide(Power::Discharging, TEAM, None, false, Some(ALL)),
            Decision::Installed(Keep::NotOptedIn)
        );
        assert_eq!(
            decide(Power::Discharging, TEAM, Some(SMALL), true, Some(ALL)),
            Decision::Installed(Keep::Explicit)
        );
        for power in [Power::Ac, Power::Charging, Power::Unknown] {
            assert_eq!(
                decide(power, TEAM, Some(SMALL), false, Some(ALL)),
                Decision::Installed(Keep::Power(power))
            );
        }
        for declared in [
            &[][..],
            &[0, 1, 2, 3, 4, 5, 6, 7][..],
            &[0, 1, 2, 3, 4][..],
            &[7, 8][..],
        ] {
            assert_eq!(
                decide(Power::Discharging, TEAM, Some(declared), false, Some(ALL)),
                Decision::Installed(Keep::Unusable)
            );
        }
    }

    #[test]
    fn mask_restricts_the_declared_team_and_unknown_mask_keeps_it() {
        assert_eq!(
            decide(Power::Discharging, TEAM, Some(SMALL), false, None),
            Decision::Installed(Keep::Masked)
        );
        assert_eq!(
            decide(Power::Discharging, TEAM, Some(SMALL), false, Some(&[0, 1][..])),
            Decision::Reduced(vec![0, 1])
        );
        assert_eq!(
            decide(Power::Discharging, TEAM, Some(SMALL), false, Some(&[5, 6][..])),
            Decision::Installed(Keep::Masked)
        );
        // The mask keeps the declared order; it never reorders worker 0.
        assert_eq!(
            decide(Power::Discharging, TEAM, Some(&[6, 2][..]), false, Some(ALL)),
            Decision::Reduced(vec![6, 2])
        );
    }

    #[test]
    fn cpu_lists_round_trip_through_the_documented_grammar() {
        assert_eq!(parse_cpus("0-7"), Some(TEAM.to_vec()));
        assert_eq!(parse_cpus("0,1,4,5"), Some(vec![0, 1, 4, 5]));
        assert_eq!(parse_cpus("0-3,8-11"), Some(vec![0, 1, 2, 3, 8, 9, 10, 11]));
        assert_eq!(format_cpus(SMALL), "0,1,2,3");
        assert_eq!(parse_cpus(&format_cpus(SMALL)), Some(SMALL.to_vec()));
        for bad in ["", " ", "0-", "-1", "3-1", "0,0", "0-1,1", "a", "0,,x"] {
            assert_eq!(parse_cpus(bad), None, "{bad:?}");
        }
    }
}
