//! Explicit owner-only native ChatGPT PKCE/logout; no factory or settings writes.
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{codes, OxaError};
use oxa_tui::{InputEvent, ProcessTerminal, TermOpts, Terminal};
use oxc_gateway::openai::{
    chatgpt_tokens,
    login::{Error, Login},
};
use std::{
    io::Write,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const USAGE: &str = "Usage: oxa login chatgpt --experimental [--paste] [--replace] | oxa logout chatgpt";
const NOTICE: &str = "Experimental ChatGPT sign-in uses the existing public client identity; it is not the future participating-app program. Cloud settings are not changed.";
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    logout: bool,
    paste: bool,
    replace: bool,
}
fn usage() -> OxaError {
    OxaError::new(codes::USAGE, USAGE)
}
fn failed(error: impl std::fmt::Display) -> OxaError {
    OxaError::new(codes::INVALID, error.to_string())
}
fn parse(logout: bool, words: &[String]) -> Result<Plan, OxaError> {
    if words.first().map(String::as_str) != Some("chatgpt") {
        return Err(usage());
    }
    let mut plan = Plan {
        logout,
        paste: false,
        replace: false,
    };
    let mut experimental = false;
    for word in &words[1..] {
        match word.as_str() {
            "--experimental" if !logout && !experimental => experimental = true,
            "--paste" if !logout && !plan.paste => plan.paste = true,
            "--replace" if !logout && !plan.replace => plan.replace = true,
            _ => return Err(usage()),
        }
    }
    if !logout && !experimental {
        return Err(usage());
    }
    Ok(plan)
}
struct Signals {
    cancelled: Arc<AtomicBool>,
    ids: Vec<signal_hook::SigId>,
}
impl Signals {
    fn new() -> Result<Self, OxaError> {
        let mut signals = Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            ids: vec![],
        };
        for number in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            signals.ids.push(
                signal_hook::flag::register(number, signals.cancelled.clone())
                    .map_err(|_| failed("Cannot install sign-in cancellation"))?,
            );
        }
        Ok(signals)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}
/// Standalone command only: dispatch reaches this before session/tools/RPC setup.
/// No endpoint, credential path, code or token can be supplied as an argument.
pub(crate) fn run(
    logout: bool,
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    env: &Env,
    terminal: bool,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    let plan = parse(logout, words)?;
    if env.get("OXA_CODING_AGENT").is_some()
        || env.get("AI_AGENT").is_some()
        || env.get("OXA_SUBAGENT_DEPTH").is_some()
    {
        return Err(failed(
            "ChatGPT login/logout requires the owner outside an agent tool",
        ));
    }
    if plan.logout {
        let signals = Signals::new()?;
        // No creation or credential read for logout. The token module validates
        // the existing private entry and serializes against all refresh writers.
        let path = dirs.home.join(".config/oxc/llm/chatgpt-auth.json");
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            if signals.cancelled.load(Ordering::SeqCst) {
                return Err(failed(Error::Cancelled));
            }
            if Instant::now() >= deadline {
                return Err(failed(Error::Expired));
            }
            if chatgpt_tokens::try_logout(&path).map_err(failed)?.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        writeln!(out, "ChatGPT signed out.").map_err(|_| failed("Cannot write sign-in result"))?;
        return Ok(0);
    }
    if !terminal {
        return Err(failed("ChatGPT sign-in requires an owner terminal"));
    }
    if oxa_proto::offline() {
        return Err(oxa_proto::offline_error("ChatGPT sign-in"));
    }
    let user = Settings::load(dirs, cwd, false);
    let client_id = match user.get("oxa.cloud.subscription.client_id") {
        None => oxc_gateway::openai::login::CLIENT_ID,
        Some(value) => value
            .as_str()
            .ok_or_else(|| failed("Invalid user ChatGPT client identity"))?,
    };
    // Raw terminal supplies bounded non-echoed paste and ctrl-C. Drop restores
    // it before the exchange; no sign-in URL/code ever enters chat history.
    let mut term = ProcessTerminal::open(TermOpts {
        kitty_keyboard: false,
        bracketed_paste: false,
        focus_reporting: false,
        query_timeout_ms: 0,
        ..TermOpts::default()
    })?;
    let mut login = Login::with_client_id(client_id).map_err(failed)?;
    let signals = Signals::new()?;
    writeln!(
        out,
        "{NOTICE}\r\nOpen this URL in your browser:\r\n{}\r",
        login.authorization_url()
    )
    .map_err(|_| failed("Cannot write sign-in instructions"))?;
    if plan.paste {
        writeln!(
            out,
            "Paste the complete callback URL and press Enter (input hidden).\r"
        )
        .map_err(|_| failed("Cannot write sign-in instructions"))?;
    }
    out.flush()
        .map_err(|_| failed("Cannot write sign-in instructions"))?;
    let mut pasted = String::new();
    let code = loop {
        login.check(&signals.cancelled).map_err(failed)?;
        if !plan.paste {
            if let Some(code) = login.poll_callback(&signals.cancelled).map_err(failed)? {
                break code;
            }
        }
        let events = term.poll(10);
        if let Some(value) = input(&events, plan.paste, &mut pasted, &signals.cancelled)? {
            break login
                .pasted_callback(&value, &signals.cancelled)
                .map_err(failed)?;
        }
    };
    drop(term);
    // Directory preparation reads no key or previous token. Install is the
    // existing module's locked transaction and refuses an account switch even
    // with --replace. Existing runtime account pinning remains unchanged.
    let path = chatgpt_tokens::prepare_login_path(&dirs.home).map_err(failed)?;
    login
        .finish(code, &path, plan.replace, &[], &signals.cancelled)
        .map_err(failed)?;
    writeln!(out, "ChatGPT sign-in complete.").map_err(|_| failed("Cannot write sign-in result"))?;
    Ok(0)
}
fn input(
    events: &[InputEvent],
    paste: bool,
    text: &mut String,
    cancelled: &AtomicBool,
) -> Result<Option<String>, OxaError> {
    // A terminal closure cancels the entire batch before a queued Enter can accept input.
    if events.contains(&InputEvent::TerminalClosed) {
        cancelled.store(true, Ordering::SeqCst);
        return Err(OxaError::new(
            oxa_proto::codes::TERMINAL_CLOSED,
            "terminal disconnected",
        ));
    }
    for event in events {
        match event {
            InputEvent::Key(key) if key.is("ctrl+c") || key.is("escape") => {
                cancelled.store(true, Ordering::SeqCst);
                return Err(failed(Error::Cancelled));
            }
            InputEvent::Key(key) if paste && key.is("enter") => return Ok(Some(std::mem::take(text))),
            InputEvent::Key(key) if paste && key.is("backspace") => {
                text.pop();
            }
            InputEvent::Key(key) if paste => {
                if let Some(value) = &key.text {
                    text.push_str(value);
                }
            }
            InputEvent::Paste(value) if paste => text.push_str(value),
            _ => {}
        }
        if text.len() > 8192 {
            return Err(failed(Error::Callback));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_loss_cancels_before_queued_callback_acceptance() {
        for paste in [false, true] {
            for closed_first in [false, true] {
                let stop = AtomicBool::new(false);
                let mut text = "synthetic callback".to_string();
                let mut events = vec![
                    InputEvent::Key(oxa_tui::Key::press("enter")),
                    InputEvent::TerminalClosed,
                ];
                if closed_first {
                    events.reverse();
                }
                let error = input(&events, paste, &mut text, &stop).unwrap_err();
                assert_eq!(error.code, oxa_proto::codes::TERMINAL_CLOSED);
                assert!(stop.load(Ordering::SeqCst));
                assert_eq!(text, "synthetic callback");
            }
        }
    }

    fn words(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn explicit_command_and_hidden_input_are_bounded_and_fixed() {
        for values in [
            vec![],
            vec!["chatgpt"],
            vec!["chatgpt", "--experimental", "--experimental"],
            vec!["chatgpt", "--experimental", "synthetic-code"],
            vec!["other", "--experimental"],
        ] {
            assert_eq!(parse(false, &words(&values)).unwrap_err().message, USAGE);
        }
        assert!(parse(
            false,
            &words(&["chatgpt", "--experimental", "--paste", "--replace"])
        )
        .is_ok());
        assert!(parse(true, &words(&["chatgpt"])).is_ok());
        assert!(parse(true, &words(&["chatgpt", "--replace"])).is_err());
        let stop = AtomicBool::new(false);
        let mut text = String::new();
        assert!(input(
            &[InputEvent::Paste("synthetic callback".into())],
            true,
            &mut text,
            &stop
        )
        .unwrap()
        .is_none());
        assert_eq!(
            input(
                &[InputEvent::Key(oxa_tui::Key::press("enter"))],
                true,
                &mut text,
                &stop
            )
            .unwrap()
            .as_deref(),
            Some("synthetic callback")
        );
        assert!(text.is_empty());
        assert!(input(&[InputEvent::Paste("a".repeat(8193))], true, &mut text, &stop).is_err());
        assert!(input(
            &[InputEvent::Key(oxa_tui::Key::press("ctrl+c"))],
            false,
            &mut String::new(),
            &stop
        )
        .is_err());
        assert!(stop.load(Ordering::SeqCst));
    }
}
