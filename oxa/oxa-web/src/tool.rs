//! The `web_search` tool and the provider seam.
use crate::shape::{shape, Budgets, Found};
use oxa_proto::{
    tool_errors, Access, CancelToken, Exposure, Level, Mode, Source, Tool, ToolClass, ToolCtx, ToolOutput,
    ToolSpec, Ui, Value,
};
use oxa_tools::descriptions::web_search as text;
use std::sync::Arc;

/// The `source` of the tool (an oxa addition, like `oxa:subagent`): selected by default,
/// dropped by `--no-tools` / a `--tools` list that does not name it.
pub const SOURCE: &str = "oxa:web_search";

/// The validated arguments of one call.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    pub q: String,
    pub count: Option<u32>,
    pub freshness: Option<String>,
    pub country: Option<String>,
    pub search_lang: Option<String>,
}

/// A search provider. Errors are model-facing texts in the problem shape (fixed, W4).
pub trait SearchProvider: Send + Sync {
    /// The host the tool connects to (`Access::Net` in the ask mode).
    fn host(&self) -> String;
    /// Run one search. Must honour `cancel` (then `Err(tool_errors::ABORTED)`).
    fn search(&self, q: &Query, cancel: &CancelToken) -> Result<Vec<Found>, String>;
    /// A user notice to show once (the fallback notice), taken at most once per process.
    fn take_notice(&self) -> Option<String> {
        None
    }
}

fn freshness_ok(s: &str) -> bool {
    if matches!(s, "pd" | "pw" | "pm" | "py") {
        return true;
    }
    let date = |d: &str| {
        let b = d.as_bytes();
        b.len() == 10
            && b[4] == b'-'
            && b[7] == b'-'
            && b.iter()
                .enumerate()
                .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    };
    s.split_once("to").is_some_and(|(a, b)| date(a) && date(b))
}

fn lang_ok(s: &str) -> bool {
    let mut parts = s.split('-');
    let head = parts.next().unwrap_or("");
    (2..=3).contains(&head.len())
        && head.bytes().all(|c| c.is_ascii_alphabetic())
        && parts.all(|p| (2..=4).contains(&p.len()) && p.bytes().all(|c| c.is_ascii_alphanumeric()))
}

/// Validate the arguments (strings and integers only; `query` required, at most 600
/// characters). The error is the model-facing text.
pub fn parse_query(args: &Value) -> Result<Query, String> {
    let Value::Object(map) = args else {
        return Err(text::QUERY_MISSING.to_string());
    };
    let wrong = |name: &str| text::WRONG_TYPE.replace("{name}", name);
    let mut q = Query::default();
    for (k, v) in map {
        match k.as_str() {
            "query" => match v {
                Value::String(s) => q.q = crate::shape::normalise(s),
                _ => return Err(wrong(k)),
            },
            "count" => match v {
                Value::Null => {}
                Value::Number(n) => match n.as_u64().filter(|n| (1..=text::MAX_COUNT).contains(n)) {
                    Some(n) => q.count = Some(n as u32),
                    None => return Err(text::BAD_COUNT.to_string()),
                },
                // A small model may quote the number.
                Value::String(s) => match s
                    .trim()
                    .parse::<u64>()
                    .ok()
                    .filter(|n| (1..=text::MAX_COUNT).contains(n))
                {
                    Some(n) => q.count = Some(n as u32),
                    None => return Err(text::BAD_COUNT.to_string()),
                },
                _ => return Err(wrong(k)),
            },
            "freshness" | "country" | "search_lang" => {
                let s = match v {
                    Value::Null => continue,
                    Value::String(s) => s.trim().to_string(),
                    _ => return Err(wrong(k)),
                };
                if s.is_empty() {
                    continue;
                }
                match k.as_str() {
                    "freshness" if freshness_ok(&s) => q.freshness = Some(s),
                    "freshness" => return Err(text::BAD_FRESHNESS.to_string()),
                    "country" if s.len() == 2 && s.bytes().all(|c| c.is_ascii_alphabetic()) => {
                        q.country = Some(s.to_ascii_lowercase())
                    }
                    "country" => return Err(text::BAD_COUNTRY.to_string()),
                    _ if lang_ok(&s) => q.search_lang = Some(s.to_ascii_lowercase()),
                    _ => return Err(text::BAD_LANG.to_string()),
                }
            }
            other => return Err(text::UNKNOWN_ARG.replace("{name}", other)),
        }
    }
    if q.q.is_empty() {
        return Err(text::QUERY_MISSING.to_string());
    }
    let n = q.q.chars().count();
    if n > text::MAX_QUERY_CHARS {
        return Err(text::QUERY_TOO_LONG.replace("{n}", &n.to_string()));
    }
    Ok(q)
}

/// `web_search`. Class `Net`; it runs serially (`needs_ui`, so the one-time fallback notice
/// reaches the user) and the provider serialises its requests too.
///
/// Permission: in the workspace mode the tool runs without a prompt (it reads nothing local
/// and changes nothing, and the query text does leave the machine). In the plan mode it is
/// denied: plan mode has no network. In the ask mode it asks `Access::Net { host }` (a session
/// grant covers the host). Offline, every call is refused with the offline error.
pub struct WebSearchTool {
    spec: ToolSpec,
    provider: Arc<dyn SearchProvider>,
    budgets: Budgets,
}

impl WebSearchTool {
    pub fn new(provider: Arc<dyn SearchProvider>) -> WebSearchTool {
        WebSearchTool {
            spec: ToolSpec {
                name: text::NAME.to_string(),
                description: text::DESCRIPTION.to_string(),
                schema_json: text::SCHEMA.to_string(),
                class: ToolClass::Net,
                exposure: Exposure::Direct,
                source: Source::Extension(SOURCE.to_string()),
            },
            provider,
            budgets: Budgets::default(),
        }
    }
    pub fn with_budgets(mut self, budgets: Budgets) -> WebSearchTool {
        self.budgets = budgets;
        self
    }

    fn offline_refusal(&self) -> Option<String> {
        oxa_proto::offline().then(|| {
            oxa_proto::offline_error(&format!("{} ({})", text::NAME, self.provider.host())).to_string()
        })
    }
}

impl Tool for WebSearchTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        parse_query(args)?;
        if let Some(e) = self.offline_refusal() {
            return Err(e);
        }
        Ok(match cx.mode {
            // Item 2 (sec-prod): plan mode is no-network, so the call is denied by the policy
            // table like any other network access (`plan mode does not allow network access`).
            Mode::Ask | Mode::Plan => vec![Access::Net {
                host: self.provider.host(),
            }],
            Mode::Workspace => Vec::new(),
        })
    }
    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let q = match parse_query(args) {
            Ok(q) => q,
            Err(e) => return ToolOutput::error(e),
        };
        if let Some(e) = self.offline_refusal() {
            return ToolOutput::error(e);
        }
        if cx.cancel.is_cancelled() {
            return ToolOutput::error(tool_errors::ABORTED);
        }
        match self.provider.search(&q, cx.cancel) {
            Ok(found) => ToolOutput::ok(shape(&q.q, &found, q.count.map(|c| c as usize), &self.budgets)),
            Err(e) => ToolOutput::error(e),
        }
    }
    fn needs_ui(&self) -> bool {
        true
    }
    fn run_with_ui(
        &self,
        args: &Value,
        cx: &ToolCtx,
        live: &mut dyn FnMut(&str),
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        let out = self.run(args, cx, live);
        if let Some(notice) = self.provider.take_notice() {
            ui.notify(&notice, Level::Warning);
        }
        out
    }
}
