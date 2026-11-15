//! R6 (wave H): every refusal, stop and notice added for local-first use has one
//! shape: line 1 `<what happened>: <reason>`, line 2 `Fix: <command or action>`. Goldens of
//! each text; the per-feature tests check where each one appears.

use oxa_core::{reload, repair};
use oxa_proto::{codes, offline_error, remote_engine_error, remote_engine_notice, OxaError};

fn shape(text: &str) {
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(lines.len(), 2, "two lines: {text:?}");
    assert!(lines[0].contains(": "), "what: reason: {text:?}");
    assert!(lines[1].starts_with("Fix: "), "the fix: {text:?}");
}

#[test]
fn goldens() {
    let cause = repair::Cause::MissingRequired {
        tool: "read".into(),
        missing: vec!["path".into()],
    };
    let golden = [
        (
            repair::stop_text(&cause),
            "Run stopped: the model wrote a malformed tool call 3 times in a row: missing required (Tool \"read\" is missing the required parameter(s): path.)\nFix: send the request again, rephrase it, or switch model with /model",
        ),
        (
            repair::notice_text(&cause, 1),
            "Malformed tool call, repair 1 of 2: missing required (Tool \"read\" is missing the required parameter(s): path.)\nFix: none needed: the model was asked to write the call again",
        ),
        (
            offline_error("git for package https://example.com/p").to_string(),
            "Refused: git for package https://example.com/p needs the network: offline mode is on (OXA_OFFLINE=1, --offline or oxa.offline)\nFix: turn offline mode off: unset OXA_OFFLINE, drop --offline, set \"oxa.offline\": false",
        ),
        (
            remote_engine_error("10.0.0.5").to_string(),
            "Refused to use the engine at 10.0.0.5: it is not a loopback address, so prompts and files would leave this machine over plain HTTP\nFix: set \"oxa.engine.allowRemote\": true in the user settings.json, or use an engine on 127.0.0.1",
        ),
        (
            remote_engine_notice("10.0.0.5"),
            "Remote engine 10.0.0.5: prompts and files leave this machine unencrypted (plain HTTP)\nFix: use an engine on 127.0.0.1 or an encrypted tunnel (ssh -L) to keep them private",
        ),
        (
            reload::loading_text(&OxaError::new(codes::ENGINE_DOWN, "engine not reachable")),
            "Model loading: the engine is loading the model or restarting (engine not reachable)\nFix: none needed: oxa sends the same request again when the engine is ready; Esc cancels",
        ),
        (
            reload::RELOADED_TEXT.to_string(),
            "Engine reloaded: it held nothing of this session, so the whole conversation was prefilled again (the model was unloaded and loaded, or the engine restarted)\nFix: none needed: oxa read the engine's capabilities and limits again",
        ),
    ];
    for (got, want) in golden {
        assert_eq!(got, want);
        shape(&got);
    }
    assert_eq!(offline_error("x").code, codes::OFFLINE);
    assert_eq!(remote_engine_error("h").code, codes::REMOTE_ENGINE);
}
