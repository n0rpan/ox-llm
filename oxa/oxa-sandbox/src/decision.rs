//! One decision for where a command runs: in bwrap, with the user's rights, with the user's
//! rights but no network, or not at all. It replaces the checks that were spread over the
//! front end, `FullAccess` and the hook sandbox. The decision is a pure function of the run's
//! [`ExecPolicy`] (fixed at startup) and two live facts (yolo, the OS sandbox switch), so the
//! table below is the only place that says what happens.
//!
//! | role | host rights wanted? | result |
//! |---|---|---|
//! | model bash | no (OS sandbox on, no yolo) | bwrap |
//! | model bash | yes, offline | host without network, or refused without a pinned bwrap |
//! | model bash | yes, `oxa.sandbox.net` off | host without network; without a pinned bwrap an attended run with no OS sandbox requested runs on the host and warns, any other run is refused |
//! | model bash | yes | host |
//! | read-only (plan) | n/a | bwrap where the platform has one (refused without it), else host |
//! | exec hook | yolo | like model bash with host rights |
//! | exec hook | otherwise | bwrap where the platform has one (refused without it, never the host), else host |
use oxa_proto::{OxaError, NET_NOT_RESTRICTED_NOTICE};
use std::path::{Path, PathBuf};

/// What is being run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecRole {
    /// The model's bash tool in workspace/ask/auto/yolo modes.
    ModelBash,
    /// A read-only command (plan mode).
    ReadOnly,
    /// An exec hook or extension process.
    Hook,
}

/// Where a command runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecDecision {
    /// Through the wrapped sandbox (bwrap; it refuses by itself when bwrap is unusable).
    Sandbox,
    /// With the user's rights.
    Host,
    /// With the user's rights through this pinned bwrap with the network unshared.
    HostNoNet(PathBuf),
    /// Not at all; the text says why.
    Refuse(String),
}

/// The facts of one run that the decision depends on, fixed at startup.
#[derive(Debug, Clone)]
pub struct ExecPolicy {
    /// The platform has an OS sandbox (Linux).
    pub os_sandbox_platform: bool,
    /// The bwrap that host commands without network go through: found, probed and checked for
    /// root ownership once, before any model command ([`pinned_bwrap`]).
    pub pinned_bwrap: Option<PathBuf>,
    /// The OS sandbox was requested for this run (`--sandbox`, `oxa.sandbox.interactive`, an
    /// unattended run).
    pub os_sandbox: bool,
    /// The owner is at the keyboard of an interactive session.
    pub attended_tui: bool,
    /// The offline switch.
    pub offline: bool,
    /// `oxa.sandbox.net` is `off` (or the runner owns GitHub).
    pub net_off: bool,
}

impl ExecPolicy {
    /// A policy for this platform with the pinned bwrap looked up when a network-less host run
    /// may need it. Unattended with the OS sandbox on, as `FullAccess::new` always was.
    pub fn new(path_var: &str, home: &Path, offline: bool, net_off: bool) -> ExecPolicy {
        ExecPolicy {
            os_sandbox_platform: cfg!(target_os = "linux"),
            pinned_bwrap: (offline || net_off)
                .then(|| pinned_bwrap(path_var, home))
                .flatten(),
            os_sandbox: true,
            attended_tui: false,
            offline,
            net_off,
        }
    }

    /// The decision for `role`. `yolo_now` and `os_sandbox_now` are the live switches.
    pub fn decide(&self, role: ExecRole, yolo_now: bool, os_sandbox_now: bool) -> ExecDecision {
        match role {
            ExecRole::ModelBash => {
                if yolo_now || !os_sandbox_now {
                    self.host()
                } else {
                    ExecDecision::Sandbox
                }
            }
            ExecRole::ReadOnly if self.os_sandbox_platform => ExecDecision::Sandbox,
            ExecRole::Hook if self.os_sandbox_platform && !yolo_now => ExecDecision::Sandbox,
            ExecRole::ReadOnly => self.host(),
            ExecRole::Hook => self.host(),
        }
    }

    /// A command with the user's rights: the network rules still hold where they can.
    fn host(&self) -> ExecDecision {
        let no_net = |why: OxaError| match &self.pinned_bwrap {
            Some(bwrap) => ExecDecision::HostNoNet(bwrap.clone()),
            None => ExecDecision::Refuse(why.to_string()),
        };
        if self.offline {
            return no_net(oxa_proto::offline_error("bash under full access"));
        }
        if self.net_off {
            if self.pinned_bwrap.is_none() && self.attended_tui && !self.os_sandbox {
                return ExecDecision::Host;
            }
            return no_net(oxa_proto::offline_error("bash under full access"));
        }
        ExecDecision::Host
    }

    /// The start notices that follow from the decisions: a refused bash network that this run
    /// cannot enforce and runs on anyway.
    pub fn start_notices(&self) -> Vec<String> {
        let unrestricted = self.net_off
            && !self.offline
            && self.pinned_bwrap.is_none()
            && self.attended_tui
            && !self.os_sandbox;
        unrestricted
            .then(|| NET_NOT_RESTRICTED_NOTICE.to_string())
            .into_iter()
            .collect()
    }
}

/// The bwrap of the network-disabled host path, found and probed once, before any model
/// command: a later `PATH` lookup could pick up a `bwrap` a command placed in a writable `PATH`
/// entry and run it with the network. Host bash can write what the user can, so the pinned file
/// and every directory above it must be root-owned and not group- or world-writable;
/// otherwise it could be swapped after the pin. `None` when no such bwrap was usable.
pub fn pinned_bwrap(path_var: &str, home: &Path) -> Option<PathBuf> {
    let found = crate::Bwrap::detect(path_var, home).ok()?.bwrap;
    let real = std::fs::canonicalize(found).ok()?;
    root_owned_path(&real).then_some(real)
}

/// `true` when `path` (canonical) and every directory above it are owned by root and not
/// writable by group or others, so a command running with the user's rights cannot replace it.
fn root_owned_path(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    path.ancestors()
        .all(|p| std::fs::metadata(p).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o022 == 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ExecDecision::*;
    use ExecRole::*;

    fn policy() -> ExecPolicy {
        ExecPolicy {
            os_sandbox_platform: true,
            pinned_bwrap: None,
            os_sandbox: false,
            attended_tui: true,
            offline: false,
            net_off: false,
        }
    }

    #[test]
    fn attended_net_off_without_bwrap_runs_on_the_host_and_says_so() {
        let p = ExecPolicy {
            net_off: true,
            ..policy()
        };
        assert_eq!(p.decide(ModelBash, false, false), Host);
        assert_eq!(p.start_notices(), vec![NET_NOT_RESTRICTED_NOTICE.to_string()]);
        let unattended = ExecPolicy {
            attended_tui: false,
            ..p.clone()
        };
        assert!(matches!(unattended.decide(ModelBash, false, false), Refuse(_)));
        assert!(unattended.start_notices().is_empty());
        let requested = ExecPolicy {
            os_sandbox: true,
            ..p
        };
        assert!(matches!(requested.decide(ModelBash, true, true), Refuse(_)));
    }

    #[test]
    fn a_pinned_bwrap_keeps_the_network_off_for_host_commands() {
        let b = PathBuf::from("/usr/bin/bwrap");
        let p = ExecPolicy {
            net_off: true,
            pinned_bwrap: Some(b.clone()),
            ..policy()
        };
        assert_eq!(p.decide(ModelBash, false, false), HostNoNet(b.clone()));
        assert!(p.start_notices().is_empty());
        let offline = ExecPolicy { offline: true, ..p };
        assert_eq!(offline.decide(ModelBash, true, true), HostNoNet(b));
    }

    #[test]
    fn offline_without_bwrap_refuses_in_every_role() {
        let p = ExecPolicy {
            offline: true,
            ..policy()
        };
        assert!(matches!(p.decide(ModelBash, true, false), Refuse(_)));
        assert!(matches!(p.decide(Hook, true, false), Refuse(_)));
        // Roles that stay in the sandbox are refused by the sandbox itself.
        assert_eq!(p.decide(ReadOnly, false, true), Sandbox);
        assert_eq!(p.decide(Hook, false, true), Sandbox);
    }

    #[test]
    fn the_sandbox_is_used_when_nothing_asks_for_host_rights() {
        let p = ExecPolicy {
            os_sandbox: true,
            attended_tui: false,
            ..policy()
        };
        assert_eq!(p.decide(ModelBash, false, true), Sandbox);
        assert_eq!(p.decide(ModelBash, true, true), Host);
        assert_eq!(p.decide(ModelBash, false, false), Host);
    }

    #[test]
    fn hooks_never_fall_back_to_the_host_on_linux_unless_yolo() {
        let p = policy();
        for os_sandbox_now in [false, true] {
            assert_eq!(p.decide(Hook, false, os_sandbox_now), Sandbox);
        }
        assert_eq!(p.decide(Hook, true, true), Host);
        assert_eq!(p.decide(ReadOnly, false, false), Sandbox);
    }

    #[test]
    fn a_platform_without_an_os_sandbox_runs_everything_on_the_host() {
        let p = ExecPolicy {
            os_sandbox_platform: false,
            ..policy()
        };
        assert_eq!(p.decide(Hook, false, false), Host);
        assert_eq!(p.decide(ReadOnly, false, false), Host);
    }

    #[test]
    fn offline_holds_for_read_only_without_an_os_sandbox() {
        let p = ExecPolicy {
            os_sandbox_platform: false,
            offline: true,
            ..policy()
        };
        assert!(matches!(p.decide(ReadOnly, false, false), Refuse(_)));
        let unattended_net_off = ExecPolicy {
            os_sandbox_platform: false,
            net_off: true,
            ..policy()
        };
        assert!(matches!(
            unattended_net_off.decide(ReadOnly, false, false),
            Refuse(_)
        ));
    }
}
