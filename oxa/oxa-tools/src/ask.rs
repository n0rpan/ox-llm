//! Owner questions. Offered by the CLI only in lead sessions, never worker prefixes.
use oxa_proto::{Access, Exposure, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value};
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

pub struct AskTool {
    spec: ToolSpec,
    asks_file: PathBuf,
}

impl AskTool {
    /// `asks_file` is the runner-owned journal, never a model-supplied path.
    pub fn new(asks_file: PathBuf) -> Self {
        Self {
            asks_file,
            spec: ToolSpec {
                name: "ask".into(),
                description: "Ask the owner a question with 2–4 options and a recommendation. Use only for decisions needing owner input, not routine work. Without an owner, records the question and proceeds on the recommendation. This is not permission to bypass tool policy.".into(),
                schema_json: json!({
                    "type": "object", "additionalProperties": false,
                    "properties": {
                        "question": {"type":"string", "minLength":1},
                        "options": {"type":"array", "minItems":2, "maxItems":4, "items": {
                            "type":"object", "additionalProperties":false,
                            "properties": {"label":{"type":"string", "minLength":1}, "detail":{"type":"string", "minLength":1}},
                            "required":["label", "detail"]
                        }},
                        "recommended": {"type":"string", "description":"Exact label of the recommended option"}
                    },
                    "required":["question", "options", "recommended"]
                }).to_string(),
                class: ToolClass::Read,
                exposure: Exposure::Direct,
                source: Source::Builtin,
            },
        }
    }

    fn answer(&self, args: &Value, cx: &ToolCtx, ui: Option<&mut dyn Ui>) -> ToolOutput {
        let q = match Question::parse(args) {
            Ok(q) => q,
            Err(e) => return ToolOutput::error(e),
        };
        if cx.cancel.is_cancelled() {
            return ToolOutput::error(oxa_proto::tool_errors::ABORTED);
        }
        if let Some(ui) = ui.filter(|ui| ui.interactive()) {
            let options: Vec<String> = q
                .options
                .iter()
                .map(|(label, detail)| {
                    let mark = if *label == q.recommended {
                        " (recommended)"
                    } else {
                        ""
                    };
                    format!("{label}{mark}: {detail}")
                })
                .collect();
            return match ui.select(q.question, &options, None) {
                Some(i) if i < q.options.len() => {
                    ToolOutput::ok(format!("Owner selected: {}", q.options[i].0))
                }
                _ => ToolOutput::ok(
                    "No answer; the owner dismissed the question. Do not treat this as approval.",
                ),
            };
        }
        let record = json!({
            "question": q.question, "options": args["options"], "recommended": q.recommended,
            "cwd": cx.cwd,
            "session_id": cx.env.iter().find(|(k, _)| k == "OXA_SESSION_ID").map(|(_, v)| v),
        });
        let text = format!("no owner; proceed on the recommended option: {}", q.recommended);
        match self.record(&record) {
            Ok(()) => ToolOutput::ok(text),
            // An unwritable journal must not strand unattended work or claim it was saved.
            Err(e) => ToolOutput::ok(format!(
                "{text}\nWarning: could not record the question in the asks file: {e}"
            )),
        }
    }

    fn record(&self, record: &Value) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.asks_file)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.nlink() != 1 || meta.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::other("asks file must be a private regular file"));
        }
        let mut line = record.to_string();
        line.push('\n');
        file.write_all(line.as_bytes())
    }
}

struct Question<'a> {
    question: &'a str,
    options: Vec<(&'a str, &'a str)>,
    recommended: &'a str,
}

impl<'a> Question<'a> {
    fn parse(args: &'a Value) -> Result<Self, String> {
        let text = |v: &'a Value| {
            v.as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or("ask requires non-empty strings".to_string())
        };
        let question = text(&args["question"])?;
        let recommended = text(&args["recommended"])?;
        let options = args["options"]
            .as_array()
            .filter(|a| (2..=4).contains(&a.len()))
            .ok_or("ask requires 2–4 options")?;
        let mut parsed = Vec::new();
        for o in options {
            let label = text(&o["label"])?;
            let detail = text(&o["detail"])?;
            if parsed.iter().any(|(l, _)| *l == label) {
                return Err("ask option labels must be unique".into());
            }
            parsed.push((label, detail));
        }
        if !parsed.iter().any(|(l, _)| *l == recommended) {
            return Err("ask recommended must be an option label".into());
        }
        Ok(Self {
            question,
            options: parsed,
            recommended,
        })
    }
}

impl Tool for AskTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, args: &Value, _: &ToolCtx) -> Result<Vec<Access>, String> {
        Question::parse(args)?;
        // Only an owner dialog or the runner's private journal, no model-selected access.
        Ok(Vec::new())
    }
    fn needs_ui(&self) -> bool {
        true
    }
    fn run(&self, args: &Value, cx: &ToolCtx, _: &mut dyn FnMut(&str)) -> ToolOutput {
        self.answer(args, cx, None)
    }
    fn run_with_ui(
        &self,
        args: &Value,
        cx: &ToolCtx,
        _: &mut dyn FnMut(&str),
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        self.answer(args, cx, Some(ui))
    }
}
