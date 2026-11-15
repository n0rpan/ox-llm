//! Task 60, part B: the adversarial escape suite for the oxa bwrap sandbox.
//!
//! Every test states its threat, attempt and expected refusal. The suite is a Cargo test
//! target (`cargo test -p oxa-sandbox --test escape`), so the free GitHub CI runs it on every
//! PR. Tests that need a host service skip with a notice when it is not present; a missing
//! `bwrap` is a failure under `OXA_REQUIRE_BWRAP=1` (CI sets it).
//!
//! The fixture directory is `oxa/oxa-sandbox/tests/escape/`; `tests/escape/FINDINGS.md` at the
//! repository root indexes this suite and the fixed bugs it found.

mod credentials;
mod env;
mod filenames;
mod fs_paths;
mod git;
mod harness;
mod ipc;
mod namespace;
mod offline;
