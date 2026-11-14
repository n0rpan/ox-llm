//! Item 2 (sec-prod): `web_search` in each permission mode. Plan mode is no-network: the call is
//! denied with the plan-mode network message the other network accesses get. Workspace keeps
//! running without a prompt (unchanged); ask prompts for the host. No server is started: only
//! the declared access and the policy decision are checked.
mod common;

use common::*;
use oxa_perm::{deny_reasons, ModePolicy};
use oxa_proto::{Access, CancelToken, Decision, Mode, Policy, Tool, ToolCtx};
use oxa_web::{Brave, SearchProvider, WebSearchTool};
use serde_json::json;
use std::sync::Arc;

fn tool() -> WebSearchTool {
    let provider: Arc<dyn SearchProvider> = Arc::new(Brave::new(key(), opts("http://127.0.0.1:9")));
    WebSearchTool::new(provider)
}

fn access_in(mode: Mode) -> (Vec<Access>, std::path::PathBuf) {
    let cwd = std::env::temp_dir();
    let (sb, cancel) = (NoSandbox, CancelToken::new());
    let cx: ToolCtx = ctx(&cwd, &sb, mode, &cancel);
    let accesses = tool().access(&json!({"query": "q"}), &cx).expect("declared");
    (accesses, cwd)
}

#[test]
fn plan_mode_declares_a_network_access_that_the_policy_denies() {
    let (accesses, cwd) = access_in(Mode::Plan);
    assert_eq!(accesses.len(), 1, "plan: one network access is declared");
    assert!(matches!(accesses[0], Access::Net { .. }), "{:?}", accesses[0]);
    let policy = ModePolicy::new();
    assert_eq!(
        policy.decide(Mode::Plan, &accesses[0], &cwd),
        Decision::Deny(deny_reasons::PLAN_NET)
    );
    assert_eq!(deny_reasons::PLAN_NET, "plan mode does not allow network access");
    // The model sees the same tool error text as for any other network call in plan mode.
    let text = format!(
        "{}{}",
        oxa_proto::tool_errors::DENIED_PREFIX,
        deny_reasons::PLAN_NET
    );
    assert_eq!(text, "Permission denied: plan mode does not allow network access");
}

#[test]
fn workspace_mode_is_unchanged_no_prompt() {
    let (accesses, _) = access_in(Mode::Workspace);
    assert!(
        accesses.is_empty(),
        "workspace: no access is declared, so no prompt"
    );
}

#[test]
fn ask_mode_prompts_for_the_host() {
    let (accesses, cwd) = access_in(Mode::Ask);
    assert_eq!(accesses.len(), 1);
    assert_eq!(
        ModePolicy::new().decide(Mode::Ask, &accesses[0], &cwd),
        Decision::Ask
    );
}
