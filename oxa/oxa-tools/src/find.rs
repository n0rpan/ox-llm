//! A8: the `find` tool. Owner: task A8. Signatures below are frozen; bodies are yours.
// ported from pi packages/coding-agent/src/core/tools/find.ts
use crate::descriptions::{self, errors, limits};
use crate::grep::{build_glob, cap_output, opt_count, opt_str, rel_path, req_str, strip_dot_slash, walk};
use crate::resolve_path;
use oxa_proto::{Access, Tool, ToolCtx, ToolOutput, ToolSpec, Value};

/// `find {pattern, path?, limit?}`: glob relative to `path`, .gitignore honoured, results sorted, relative to the search root; limit default 1000, below 1 is 1; pi's limit notice.
#[derive(Debug, Clone)]
pub struct FindTool {
    spec: ToolSpec,
}

impl FindTool {
    pub fn new() -> FindTool {
        FindTool {
            spec: crate::spec_of(&descriptions::FIND),
        }
    }
}

impl Default for FindTool {
    fn default() -> Self {
        Self::new()
    }
}

/// A parsed `find` call.
struct Query {
    /// The path as the model wrote it (default `.`).
    path: String,
    matcher: globset::GlobMatcher,
    /// The pattern has a `/`: it is matched against the path relative to the search root.
    has_slash: bool,
    /// The pattern is absolute: it is matched against the absolute path.
    absolute: bool,
    limit: usize,
}

fn parse(args: &Value) -> Result<Query, String> {
    let pattern = req_str(args, "pattern")?;
    let path = opt_str(args, "path")?.unwrap_or_else(|| ".".to_string());
    let limit = opt_count(args, "limit", limits::FIND_LIMIT, limits::MIN_LIMIT)?;
    let pat = strip_dot_slash(&pattern);
    if pat.is_empty() {
        return Err("Invalid arguments: pattern must not be empty".to_string());
    }
    // Smart case, as fd: an all-lowercase pattern matches case-insensitively.
    let ignore_case = pat == pat.to_lowercase();
    let has_slash = pat.contains('/');
    Ok(Query {
        path,
        matcher: build_glob(pat, ignore_case)?,
        has_slash,
        absolute: has_slash && pat.starts_with('/'),
        limit,
    })
}

impl Tool for FindTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// `[Access::Read(search root)]`.
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        let q = parse(args)?;
        Ok(vec![Access::Read(resolve_path(cx.cwd, &q.path))])
    }
    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let q = match parse(args) {
            Ok(q) => q,
            Err(e) => return ToolOutput::error(e),
        };
        let root = resolve_path(cx.cwd, &q.path);
        if std::fs::metadata(&root).is_err() {
            return ToolOutput::error(errors::path_not_found(&q.path));
        }

        let mut hits: Vec<String> = Vec::new();
        for entry in walk(&root) {
            if cx.cancel.is_cancelled() {
                return ToolOutput::error(errors::ABORTED);
            }
            let Ok(entry) = entry else { continue };
            // The search root itself is never a result.
            if entry.depth() == 0 {
                continue;
            }
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            let rel = rel_path(&root, entry.path());
            let candidate = if q.absolute {
                entry.path().to_string_lossy().into_owned()
            } else if q.has_slash {
                rel.clone()
            } else {
                entry.file_name().to_string_lossy().into_owned()
            };
            if !q.matcher.is_match(&candidate) {
                continue;
            }
            // Directories are listed with a trailing `/`, as fd does.
            if file_type.is_dir() {
                hits.push(format!("{rel}/"));
            } else {
                hits.push(rel);
            }
        }

        if hits.is_empty() {
            return ToolOutput::ok(errors::FIND_NO_FILES);
        }
        hits.sort();
        let limit_reached = hits.len() > q.limit;
        hits.truncate(q.limit);

        let cap = crate::read::output_cap(&cx.tune);
        let (body, byte_cut) = cap_output(&hits.join("\n"), crate::read::body_cap(&cx.tune));
        let mut notices = Vec::new();
        if limit_reached {
            notices.push(errors::find_limit_notice(q.limit));
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
