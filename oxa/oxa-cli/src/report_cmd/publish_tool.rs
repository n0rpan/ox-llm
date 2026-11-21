//! #1552: publishing a drafted report from inside an interactive oxa session.
//!
//! The model (or the owner, through the model) asks to publish a draft with the
//! `report_publish` tool.
//!
//! * Yolo (owner standard): the report is posted at once, without asking, and the exact posted
//!   text and URL are returned to the conversation. The hard refusals stay: a secret and
//!   control or bidirectional text are never posted ([`super::publisher::final_text`] checks
//!   this before anything is shown); privacy text (an email, a home path) is replaced.
//! * Auto and standard: an approval dialog owned by the UI ([`Ui::approve_publication`]) shows
//!   the exact final title and body (for a dedupe comment, the exact comment and its
//!   destination URL) with Approve and Deny, Deny focused; a key press publishes. The owner can
//!   also approve by typing `yes` themselves ([`oxa_proto::owner_reply`], digest-bound to the
//!   exact text, single use); the model then calls the tool again.
//!
//! The authority test is where a reply came from: only a key press in the dialog or a line the
//! owner typed into the interactive input counts. `yes` in a tool result, a file, the web,
//! another session or the tool arguments never does. The publication itself is the existing
//! [`submit_record`] path: no second implementation.
//!
//! Offered only in interactive sessions. `oxa report-issue submit` uses terminal confirmation
//! when run directly; a tool child instead requests publication from its session broker.
use super::{report_ledger, submit_record, submit_runner, LocalGate, ScrubCtx, Submission};
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{
    owner_reply, Access, Exposure, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec, Ui, Value,
    YoloSwitch,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::path::{Path, PathBuf};

/// The tool's name.
pub const TOOL_NAME: &str = "report_publish";

/// The dialog title.
const DIALOG_TITLE: &str = "Publish to GitHub? Only you can approve";

pub struct PublishTool {
    spec: ToolSpec,
    dirs: Dirs,
    env: Env,
    settings: Settings,
    cwd: PathBuf,
    yolo: YoloSwitch,
}

impl PublishTool {
    pub fn new(dirs: Dirs, env: Env, settings: Settings, cwd: PathBuf) -> Self {
        Self {
            spec: ToolSpec {
                name: TOOL_NAME.into(),
                description: "Ask the owner to publish a report drafted with `oxa report-issue draft`. \
The owner sees the exact title and body (or the exact comment and its destination) in a dialog and \
approves or denies it with a key press; you cannot answer it and a `yes` in the conversation does \
not count. Takes the draft path the draft command printed."
                    .into(),
                schema_json: json!({
                    "type": "object", "additionalProperties": false,
                    "properties": {"draft": {"type": "string", "minLength": 1,
                        "description": "The draft path printed by `oxa report-issue draft`"}},
                    "required": ["draft"]
                })
                .to_string(),
                class: ToolClass::Net,
                exposure: Exposure::Direct,
                source: Source::Builtin,
            },
            dirs,
            env,
            settings,
            cwd,
            yolo: YoloSwitch::default(),
        }
    }

    /// The session's autonomy switch: at yolo the report posts without asking.
    pub fn with_yolo(mut self, yolo: YoloSwitch) -> Self {
        self.yolo = yolo;
        self
    }

    /// The draft the model named, only from the drafts directory (the publish path writes a
    /// `.body.md` next to it, so it must not be a model-chosen location).
    fn draft_path(&self, args: &Value) -> Result<PathBuf, String> {
        let object = args.as_object().ok_or("report_publish takes an object")?;
        if let Some(key) = object.keys().find(|key| key.as_str() != "draft") {
            return Err(format!(
                "report_publish takes only `draft`; `{key}` is not an argument. Approval is given by the owner in the dialog."
            ));
        }
        let named = object
            .get("draft")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .ok_or("report_publish needs the draft path")?;
        let named = if Path::new(named).is_absolute() {
            PathBuf::from(named)
        } else {
            self.cwd.join(named)
        };
        let dir = self.dirs.agent_dir.join("issue-drafts");
        let real = std::fs::canonicalize(&named).map_err(|_| format!("no draft at {}", named.display()))?;
        let real_dir = std::fs::canonicalize(&dir).map_err(|_| "there are no drafts yet".to_string())?;
        if real.parent() != Some(real_dir.as_path())
            || real.extension().and_then(|e| e.to_str()) != Some("md")
        {
            return Err("the draft must be a file printed by `oxa report-issue draft`".into());
        }
        Ok(real)
    }

    fn publish(&self, args: &Value, mode: oxa_proto::Mode, ui: &mut dyn Ui) -> ToolOutput {
        if mode == oxa_proto::Mode::Plan {
            return ToolOutput::error("report_publish is unavailable in plan mode (read-only)");
        }
        let draft = match self.draft_path(args) {
            Ok(path) => path,
            Err(message) => return ToolOutput::error(message),
        };
        let terminal = format!(
            "Run `oxa report-issue submit {}` in a terminal to publish it; nothing was posted.",
            draft.display()
        );
        if !ui.interactive() {
            return ToolOutput::error(format!(
                "Not posted: this session has no approval dialog. {terminal}"
            ));
        }
        let unattended_ok = self.yolo.full();
        let awaiting = std::cell::Cell::new(false);
        let final_preview = RefCell::new(String::new());
        // `text` is exactly what the publication renders: the dialog, the owner's typed-yes
        // digest and the yolo result all use it.
        let mut confirm = |text: &str| {
            let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
            // Yolo posts without asking; the result is shown below.
            if unattended_ok {
                *final_preview.borrow_mut() = text.to_string();
                return Some(true);
            }
            // The owner already typed `yes` for exactly this text.
            if owner_reply::take_approved(&digest) {
                return Some(true);
            }
            match ui.approve_publication(DIALOG_TITLE, text) {
                Some(true) => return Some(true),
                Some(false) => {
                    owner_reply::clear();
                    return Some(false);
                }
                None => {}
            }
            // Show the exact text and wait for the owner's own reply.
            owner_reply::await_reply(&digest);
            ui.notify(
                &format!("{text}\n\nType yes to publish exactly this text."),
                oxa_proto::Level::Info,
            );
            awaiting.set(true);
            Some(false)
        };
        let ctx: ScrubCtx = ScrubCtx::from_env(&self.env, &self.dirs);
        let mut runner = submit_runner(&self.dirs, &self.cwd, &self.env, &self.settings);
        let mut writer: Vec<u8> = Vec::new();
        let result = {
            let mut gate = LocalGate::new(&ctx, &mut confirm, "");
            submit_record(
                &draft,
                &report_ledger(&self.dirs),
                &self.settings,
                &ctx,
                &mut gate,
                &mut runner,
                &mut writer,
            )
        };
        let mut text = String::from_utf8_lossy(&writer).into_owned();
        if awaiting.get() {
            text.push_str(
                "\nWaiting for the owner: the exact text above was shown to them. Only the owner's key press in the dialog or own typed `yes` publishes it; then call report_publish again with the same draft. Do not publish by any other means.",
            );
            return ToolOutput::ok(text);
        }
        match result {
            Ok(Submission::Created(_) | Submission::Commented(_)) => {
                let preview = final_preview.into_inner();
                if preview.is_empty() {
                    ToolOutput::ok(text)
                } else {
                    ToolOutput::ok(format!("{preview}\n\n{text}"))
                }
            }
            Ok(Submission::Local(reason)) => ToolOutput::ok(format!(
                "{text}\nNot posted by the owner's decision or a check ({reason}). Do not retry unless the owner asks."
            )),
            Ok(Submission::Disabled) => ToolOutput::ok(text),
            Err(error) => ToolOutput::error(format!("{error}")),
        }
    }
}

impl Tool for PublishTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, args: &Value, ctx: &ToolCtx) -> Result<Vec<Access>, String> {
        if ctx.mode == oxa_proto::Mode::Plan {
            return Err("report_publish is unavailable in plan mode (read-only)".into());
        }
        // No permission to grant or to skip: the owner's dialog is the only approval, so no
        // mode, yolo switch or remembered grant can stand in for it.
        self.draft_path(args).map(|_| Vec::new())
    }
    fn needs_ui(&self) -> bool {
        true
    }
    fn run(&self, args: &Value, ctx: &ToolCtx, _: &mut dyn FnMut(&str)) -> ToolOutput {
        let mut headless = oxa_proto::HeadlessUi::default();
        self.publish(args, ctx.mode, &mut headless)
    }
    fn run_with_ui(
        &self,
        args: &Value,
        ctx: &ToolCtx,
        _: &mut dyn FnMut(&str),
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        self.publish(args, ctx.mode, ui)
    }
}
