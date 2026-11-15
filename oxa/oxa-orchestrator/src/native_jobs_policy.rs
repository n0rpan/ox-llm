//! Release-mode policy: A2A detached commands use only the owned background manager.
use oxa_core::Deps;
use oxa_proto::{Access, Tool, ToolCtx, ToolOutput, ToolSpec, Ui, Value};
use std::sync::Arc;

const REFUSAL: &str = "Native Bash background jobs are disabled in A2A sessions until v0.2.0; use background_command for owned tasks (issue #258)";
fn detached(args: &Value) -> bool {
    args.get("background").and_then(Value::as_bool) == Some(true) || args.get("job").is_some()
}
struct Foreground {
    inner: Arc<dyn Tool>,
    spec: ToolSpec,
}
impl Tool for Foreground {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        if detached(args) {
            return Err(REFUSAL.into());
        }
        self.inner.access(args, cx)
    }
    fn run(&self, args: &Value, cx: &ToolCtx, live: &mut dyn FnMut(&str)) -> ToolOutput {
        if detached(args) {
            return ToolOutput::error(REFUSAL);
        }
        self.inner.run(args, cx, live)
    }
    fn needs_ui(&self) -> bool {
        self.inner.needs_ui()
    }
    fn run_with_ui(
        &self,
        args: &Value,
        cx: &ToolCtx,
        live: &mut dyn FnMut(&str),
        ui: &mut dyn Ui,
    ) -> ToolOutput {
        if detached(args) {
            return ToolOutput::error(REFUSAL);
        }
        self.inner.run_with_ui(args, cx, live, ui)
    }
    fn default_selected(&self) -> bool {
        self.inner.default_selected()
    }
}
pub(crate) fn augment(mut deps: Deps) -> Deps {
    if std::env::var_os("OXA_SESSION_CONTROL_DIRECTORY").is_some() {
        for tool in &mut deps.tools {
            if tool.spec().name == "bash" {
                let mut spec = tool.spec().clone();
                spec.description.push_str(" In this A2A session only foreground commands are allowed; use background_command for detached work. Native job starts/reads are refused until the post-release lifecycle bridge.");
                *tool = Arc::new(Foreground {
                    inner: tool.clone(),
                    spec,
                });
            }
        }
    }
    deps
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn identifies_native_start_and_reads_without_disabling_foreground() {
        for args in [
            json!({"command":"echo x","background":true}),
            json!({"job":"job-1"}),
            json!({"job":"job-1","wait_s":1}),
        ] {
            assert!(detached(&args));
        }
        for args in [
            json!({"command":"echo x"}),
            json!({"command":"echo x","background":false}),
        ] {
            assert!(!detached(&args));
        }
    }
}
