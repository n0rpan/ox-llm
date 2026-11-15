//! A8: the `ls` tool. Owner: task A8. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/tools/ls.ts
use crate::descriptions::{self, errors, limits};
use crate::grep::{cap_output, opt_count, opt_str};
use crate::resolve_path;
use oxa_proto::{Access, Tool, ToolCtx, ToolOutput, ToolSpec, Value};

/// `ls {path?, limit?}`: entries sorted case-insensitively as pi does, directories with a trailing `/`, dotfiles included; limit default 500, below 1 is 1; pi's limit notice and empty-directory text.
#[derive(Debug, Clone)]
pub struct LsTool {
    spec: ToolSpec,
}

impl LsTool {
    pub fn new() -> LsTool {
        LsTool {
            spec: crate::spec_of(&descriptions::builtin_specs::LS_DEFAULT),
        }
    }
}

impl Default for LsTool {
    fn default() -> Self {
        Self::new()
    }
}

/// A parsed `ls` call.
struct Query {
    /// The path as the model wrote it (default `.`).
    path: String,
    limit: usize,
}

fn parse(args: &Value) -> Result<Query, String> {
    let path = opt_str(args, "path")?.unwrap_or_else(|| ".".to_string());
    let limit = opt_count(args, "limit", limits::LS_LIMIT, limits::MIN_LIMIT)?;
    Ok(Query { path, limit })
}

impl Tool for LsTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// `[Access::Read(dir)]`.
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        let q = parse(args)?;
        Ok(vec![Access::Read(resolve_path(cx.cwd, &q.path))])
    }
    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let q = match parse(args) {
            Ok(q) => q,
            Err(e) => return ToolOutput::error(e),
        };
        let dir = resolve_path(cx.cwd, &q.path);
        let meta = match std::fs::metadata(&dir) {
            Ok(m) => m,
            Err(_) => return ToolOutput::error(errors::path_not_found(&q.path)),
        };
        if !meta.is_dir() {
            return ToolOutput::error(errors::ls_not_a_directory(&q.path));
        }
        let reader = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => return ToolOutput::error(errors::ls_cannot_read(&e.to_string())),
        };

        // Sorted case-insensitively; the raw name breaks ties so the order is total.
        let mut names: Vec<String> = reader
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| a.cmp(b)));

        let mut out: Vec<String> = Vec::new();
        let mut limit_reached = false;
        for name in &names {
            if out.len() >= q.limit {
                limit_reached = true;
                break;
            }
            // Entries that cannot be stat'ed are skipped, as in pi.
            let Ok(entry_meta) = std::fs::metadata(dir.join(name)) else {
                continue;
            };
            out.push(if entry_meta.is_dir() {
                format!("{name}/")
            } else {
                name.clone()
            });
        }
        if out.is_empty() {
            return ToolOutput::ok(errors::LS_EMPTY);
        }

        let cap = crate::read::output_cap(&cx.tune);
        let (body, byte_cut) = cap_output(&out.join("\n"), crate::read::body_cap(&cx.tune));
        let mut notices = Vec::new();
        if limit_reached {
            notices.push(errors::ls_limit_notice(q.limit));
        }
        if byte_cut {
            notices.push(errors::byte_limit_notice(cap));
            if crate::read::budgeted(&cx.tune) {
                notices.push(descriptions::diet::BYTE_CUT_HINT.to_string());
            }
        }
        ToolOutput::ok(format!("{body}{}", errors::notices_suffix(&notices)))
    }
}
