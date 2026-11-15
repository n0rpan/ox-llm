//! Local control commands, independent of the daemon and engine implementation.
use crate::cli_args::Args;
use crate::cli_client::{
    command, notify, plain_name, reason, request, request_timeout, storage_id,
};
use serde_json::json;

pub(crate) fn supports(action: &str) -> bool {
    matches!(
        action,
        "load"
            | "unload"
            | "clear-cache"
            | "flash-cache"
            | "harness-prepare"
            | "harness-forget"
            | "storage-delete"
    )
}

pub(crate) async fn run(args: &Args) -> i32 {
    match args.action.as_str() {
        "harness-forget" => {
            if args
                .value("--harness")
                .is_some_and(|name| !plain_name(name))
            {
                return 2;
            }
            let body = args
                .value("--harness")
                .map_or(json!({}), |name| json!({"harness":name}));
            i32::from(request("/_local/harness/forget", Some(body)).await.0 != 200)
        }
        "harness-prepare" => {
            let Some(name) = args.value("--harness").filter(|name| plain_name(name)) else {
                return 2;
            };
            if command("systemctl", &["--user", "start", "oxc-chat.service"]).await != Some(0) {
                return 1;
            }
            i32::from(
                request_timeout(
                    "/_local/harness/prepare",
                    Some(json!({"harness":name})),
                    300,
                )
                .await
                .0 != 200,
            )
        }
        "flash-cache" => {
            let Some(pref) = args.value("--pref") else {
                return 2;
            };
            if request("/_local/flash/cache", Some(json!({"pref":pref})))
                .await
                .0
                == 200
            {
                return 0;
            }
            notify("The Flash tier was not changed: a request is running, or this model has no tiers. Try again when it is idle.").await;
            1
        }
        "load" | "unload" => {
            if args.action == "load"
                && command("systemctl", &["--user", "start", "oxc-chat.service"]).await != Some(0)
            {
                notify("The local chat service could not start. Details: journalctl --user -u oxc-chat.").await;
                return 1;
            }
            let (code, body) = request_timeout(
                &format!("/_local/{}", args.action),
                Some(json!({})),
                if args.action == "load" { 200 } else { 15 },
            )
            .await;
            if code == 200 {
                return 0;
            }
            let message = reason(
                &body,
                if code == 409 || code == 503 {
                    "A model is working. Try again when it finishes."
                } else {
                    "The local model service is not answering. Run `oxc status` to see why."
                },
            );
            notify(&message).await;
            1
        }
        "clear-cache" => {
            if request_timeout("/_local/cache/clear", Some(json!({})), 30)
                .await
                .0
                == 200
            {
                return 0;
            }
            notify("Prefix cache is busy or unavailable. Try again when the request finishes.")
                .await;
            1
        }
        "storage-delete" => {
            if command("systemctl", &["--user", "start", "oxc-chat.service"]).await != Some(0) {
                notify("The local chat service could not start. Details: journalctl --user -u oxc-chat.").await;
                return 1;
            }
            let Some(id) = args.value("--id").filter(|id| storage_id(id)) else {
                return 2;
            };
            if request_timeout("/_local/storage/delete", Some(json!({"id":id})), 120)
                .await
                .0
                == 200
            {
                return 0;
            }
            notify("Nothing was deleted: that cache is in use or a job is running. Try again when it finishes.").await;
            1
        }
        _ => 2,
    }
}
