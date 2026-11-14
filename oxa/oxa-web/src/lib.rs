//! # oxa-web: the `web_search` tool (wave H lane `search`, O-169 / O-170)
//!
//! A provider abstraction ([`SearchProvider`]) with one provider, Brave Search ([`Brave`]):
//! its LLM Context endpoint first, its web search endpoint as the fallback for plans without
//! LLM Context. The tool ([`WebSearchTool`]) is offered by a hook set ([`WebSearchHooks`]) that
//! decides once per prefix epoch (W3) whether the tool is declared: only with a key and never
//! while offline.
//!
//! ## Contracts
//! * **The key is a secret** ([`ApiKey`]): `Debug` / `Display` print a redaction; the key never
//!   reaches logs, notices, events, session files, exports, error texts or the model wire (W4).
//!   It is read from `BRAVE_SEARCH_API_KEY` (wins) or `<agent dir>/brave-search.key` (raw key,
//!   trailing newline trimmed; refused when group/world-readable, a symlink or empty). `oxa`'s
//!   `main` removes the variable from the process environment, so no child process inherits
//!   it ([`remove_key_from_process_env`]).
//! * **Network surface**: `src/brave.rs` is the only connect site of this crate (reviewed by
//!   `ci/check_net_surface.py`). Every request first passes `oxa_proto::check_connect`.
//! * **Determinism (W4)**: the result text is a pure function of the provider's answer and the
//!   budgets ([`shape`]); every error text is fixed (`oxa_tools::descriptions::web_search`).
//! * **Tests (O-170)**: mock servers only. With `OXA_TEST_ISOLATION=1` the provider refuses the
//!   real Brave host and a key file under the real home directory.
mod brave;
mod hooks;
mod key;
mod shape;
mod tool;

pub use brave::{Brave, BraveOpts, BraveShared, Endpoint, DEFAULT_BASE_URL, FALLBACK_NOTICE};
pub use hooks::{disabled_by_env, enabled, WebConfig, WebSearchHooks, STATUS_COMMAND};
pub use key::{load_key, real_home, remove_key_from_process_env, ApiKey, KeyInputs, KeyOrigin, KeyStatus};
pub use shape::{shape, strip_html, Budgets, Found};
pub use tool::{parse_query, Query, SearchProvider, WebSearchTool, SOURCE};

/// The key's environment variable (wins over the file).
pub const ENV_KEY: &str = "BRAVE_SEARCH_API_KEY";
/// The key file's name inside the agent directory.
pub const KEY_FILE: &str = "brave-search.key";
/// `OXA_WEB_SEARCH=0` (also `false`, `no`, `off`) switches the tool off for this process.
pub const ENV_SWITCH: &str = "OXA_WEB_SEARCH";
/// Hidden: the provider's base URL, honoured only when it is a loopback URL (tests).
pub const ENV_BASE_URL: &str = "OXA_BRAVE_BASE_URL";
/// Set by `oxa_fake::TestEnv::envs`: the provider refuses the real host and a key file under
/// the real home directory.
pub const ENV_TEST_ISOLATION: &str = "OXA_TEST_ISOLATION";
/// `--no-web-search`.
pub const FLAG_DISABLE: &str = "--no-web-search";

/// Settings (user < trusted project, read once at startup).
pub const SETTING_ENABLED: &str = "oxa.webSearch.enabled";
/// `brave` (default) or `none`.
pub const SETTING_PROVIDER: &str = "oxa.webSearch.provider";
/// LLM Context `maximum_number_of_tokens` (1024..32768).
pub const SETTING_MAX_TOKENS: &str = "oxa.webSearch.maxTokens";
/// The default of [`SETTING_MAX_TOKENS`]: about 8 KB of text, so the shaped result fits the
/// default tool-output cap (16384 bytes) without truncation in the common case.
pub const DEFAULT_MAX_TOKENS: u32 = 2048;
pub const MIN_MAX_TOKENS: u32 = 1024;
pub const MAX_MAX_TOKENS: u32 = 32768;
