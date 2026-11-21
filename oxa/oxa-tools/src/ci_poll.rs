//! #1540: a hint on a `bash` command that waits for CI by polling. The product, not the prompt,
//! moves the model to the native `wait` call (`until` `ci`): the note is appended to the result,
//! the command runs and reports as it always did.

/// One shell word of a command, and whether it starts a simple command (the shell's own
/// positions: the start, and after a separator or a control keyword).
struct ShellWord {
    text: String,
    command_start: bool,
}

/// `command` with every single- or double-quoted region removed, so text quoted inside a
/// command (`echo 'gh pr checks --watch'`, a commit message) is not read as a command.
fn without_quotes(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '\'' => {
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                }
                out.push(' ');
            }
            '"' => {
                let mut escaped = false;
                for q in chars.by_ref() {
                    if escaped {
                        escaped = false;
                    } else {
                        match q {
                            '\\' => escaped = true,
                            '"' => break,
                            _ => {}
                        }
                    }
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// The words of a command, unquoted. A word starts a simple command after the shell's
/// separators and after a control keyword (`do`, `then`, ...), which is enough to tell
/// `gh run watch` from the same words inside `echo` or a quoted string.
fn shell_words(command: &str) -> Vec<ShellWord> {
    const KEYWORDS: &[&str] = &[
        "do", "then", "else", "elif", "if", "while", "until", "for", "!", "time",
    ];
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = true;
    let flush = |cur: &mut String, start: &mut bool, out: &mut Vec<ShellWord>| {
        if cur.is_empty() {
            return;
        }
        let text = std::mem::take(cur);
        out.push(ShellWord {
            command_start: *start,
            text: text.clone(),
        });
        *start = KEYWORDS.contains(&text.as_str());
    };
    for c in without_quotes(command).chars() {
        if c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '{' | '}') {
            flush(&mut cur, &mut start, &mut out);
            start = true;
        } else {
            cur.push(c);
        }
    }
    flush(&mut cur, &mut start, &mut out);
    out
}

/// A 40-character object id, as `wait`'s `sha` requires it.
fn is_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The workflow name a `wait` `until` needs: `--workflow NAME` / `--workflow=NAME` / `-w NAME`,
/// or the name of `gh workflow run|view NAME`. `None` when the command does not name one.
fn workflow_of(words: &[ShellWord]) -> Option<String> {
    let valid = |name: &str| {
        !name.is_empty()
            && name.len() <= 100
            && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ' ' | '-'))
    };
    for (i, word) in words.iter().enumerate() {
        let name = if matches!(word.text.as_str(), "--workflow" | "-w") {
            words.get(i + 1).map(|next| next.text.as_str())
        } else {
            word.text.strip_prefix("--workflow=")
        };
        if let Some(name) = name.filter(|name| valid(name)) {
            return Some(name.to_string());
        }
    }
    for (i, word) in words.iter().enumerate() {
        if word.command_start
            && word.text == "gh"
            && words.get(i + 1).is_some_and(|next| next.text == "workflow")
            && words
                .get(i + 2)
                .is_some_and(|next| matches!(next.text.as_str(), "run" | "view"))
        {
            if let Some(name) = words.get(i + 3).filter(|next| valid(&next.text)) {
                return Some(name.text.clone());
            }
        }
    }
    None
}

/// #1540: a hint on a bash command that waits for CI by polling, naming the exact native
/// `wait` call when the command and its output give the head commit and the workflow. It is
/// a note on the result only: the command runs, exits and reports as it always did.
///
/// Polling means a `gh` CI command (`run`, `workflow`, `pr checks`) inside a shell loop
/// (`for` / `while` / `until`), under `watch`, behind an earlier `sleep`, with `--watch`, or
/// as `gh run watch` itself. Quoted text is ignored. The head is a 40-hex id named in the
/// command, or the first one in the output when the command reads it (`git rev-parse`,
/// `--json headSha`); the workflow is a name the command passes. Either is left as a
/// placeholder when the command does not give it.
pub(crate) fn ci_poll_note(command: &str, output: &str, wait_tool: bool) -> Option<String> {
    let words = shell_words(command);
    let mut ci = Vec::new();
    // A `gh` call is the command itself, or an argument of `watch` (whose simple command runs
    // to the next separator, so its words never start one).
    let mut inside_watch = false;
    for (i, word) in words.iter().enumerate() {
        if word.command_start {
            inside_watch = word.text == "watch";
        }
        if word.text != "gh" || !(word.command_start || inside_watch) {
            continue;
        }
        let ci_command = match words.get(i + 1).map(|next| next.text.as_str()) {
            Some("run" | "workflow") => true,
            Some("pr") => words.get(i + 2).is_some_and(|next| next.text == "checks"),
            _ => false,
        };
        if ci_command {
            ci.push(i);
        }
    }
    if ci.is_empty() {
        return None;
    }
    let run_watch = ci.iter().any(|&i| {
        words.get(i + 1).is_some_and(|next| next.text == "run")
            && words.get(i + 2).is_some_and(|next| next.text == "watch")
    });
    let polling = run_watch
        || words.iter().any(|word| word.text == "--watch")
        || words
            .iter()
            .any(|word| word.command_start && word.text == "watch")
        || words
            .iter()
            .any(|word| word.command_start && matches!(word.text.as_str(), "for" | "while" | "until"))
        || ci
            .iter()
            .any(|&i| words[..i].iter().any(|word| word.text == "sleep"));
    if !polling {
        return None;
    }

    let sha = words
        .iter()
        .find(|word| is_sha(&word.text))
        .map(|word| word.text.to_ascii_lowercase())
        .or_else(|| {
            let reads_head = words.iter().any(|word| word.text == "rev-parse")
                || words.iter().any(|word| {
                    let text = word.text.to_ascii_lowercase();
                    text.contains("headsha") || text.contains("headrefoid")
                });
            reads_head.then(|| {
                output
                    .split(|c: char| !c.is_ascii_hexdigit())
                    .find(|part| is_sha(part))
                    .map(str::to_ascii_lowercase)
            })?
        });
    let workflow = workflow_of(&words);
    let until = match (&sha, &workflow) {
        (Some(sha), Some(workflow)) => {
            format!(r#"{{"kind":"ci","sha":"{sha}","workflow":"{workflow}"}}"#)
        }
        (Some(sha), None) => format!(r#"{{"kind":"ci","sha":"{sha}","workflow":"<workflow>"}}"#),
        (None, Some(workflow)) => {
            format!(r#"{{"kind":"ci","sha":"<40-hex head>","workflow":"{workflow}"}}"#)
        }
        (None, None) => r#"{"kind":"ci","sha":"<40-hex head>","workflow":"<workflow>"}"#.to_string(),
    };
    Some(if wait_tool {
        format!(
            "CI polling: this command waits on CI by polling from bash. One native wait call returns when the run completes, so prefer wait {{\"seconds\":3600,\"reason\":\"Wait for CI\",\"until\":{until}}} - the sha is the exact 40-hex head commit (git rev-parse HEAD) and workflow the workflow name. This is a hint, not a block; the command ran normally."
        )
    } else {
        "CI polling: this command waits on CI by polling from bash. Run one blocking check (a single command that returns when CI finishes) instead of a loop. This is a hint, not a block; the command ran normally.".to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1540: polling loops get the native `wait` suggestion; the exact head and workflow are
    /// used when the command names them.
    #[test]
    fn ci_poll_note_suggests_the_native_wait() {
        const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
        for command in [
            "gh run watch 123 --exit-status",
            "gh pr checks 42 --watch",
            "watch -n 30 gh pr checks 42",
            "while :; do gh run list --workflow lane; sleep 30; done",
            "for i in 1 2 3; do gh pr checks 42; sleep 60; done",
            "until gh pr checks 42; do sleep 60; done",
            "sleep 300; gh run view 123",
            "gh pr checks 42; sleep 300; gh pr checks 42",
        ] {
            let note = ci_poll_note(command, "", true).unwrap_or_else(|| panic!("{command}"));
            assert!(note.contains(r#"wait {"seconds":3600"#), "{command}: {note}");
            assert!(note.contains(r#"{"kind":"ci""#), "{command}: {note}");
            assert!(note.contains("not a block"), "{command}: {note}");
        }
        let note = ci_poll_note(
            &format!("while :; do gh run list --commit {SHA} --workflow lane; sleep 30; done"),
            "",
            true,
        )
        .expect("polling loop");
        assert!(
            note.contains(&format!(r#""sha":"{SHA}","workflow":"lane""#)),
            "{note}"
        );
        // The head read by the command itself counts as known.
        let note = ci_poll_note(
            "while :; do git rev-parse HEAD; gh run watch 123; done",
            &format!("{SHA}\n"),
            true,
        )
        .expect("polling loop");
        assert!(note.contains(&format!(r#""sha":"{SHA}""#)), "{note}");
        assert!(note.contains(r#""workflow":"<workflow>""#), "{note}");
        // A workflow named without the head leaves the sha a placeholder.
        let note = ci_poll_note("gh run watch 123 --workflow=checks", "", true).expect("watch");
        assert!(note.contains(r#""workflow":"checks""#), "{note}");
        assert!(note.contains(r#""sha":"<40-hex head>""#), "{note}");
    }

    /// #1540: single checks, ordinary commands and quoted mentions are not polling loops.
    #[test]
    fn ci_poll_note_ignores_ordinary_commands() {
        for (command, output) in [
            ("gh run view 123 --log-failed | tail -100", ""),
            ("gh pr view 42 --json state,statusCheckRollup", ""),
            ("gh run list --workflow lane", ""),
            ("gh workflow view lane", ""),
            ("sleep 5", ""),
            ("for f in src/*.rs; do grep -n wait \"$f\"; done", ""),
            ("echo 'gh pr checks 42 --watch'", ""),
            ("printf '%s\\n' 'while :; do gh run watch 1; done'", ""),
            ("git log -1 --format=%H", ""),
            ("git log --grep 'gh run watch' | head", ""),
            ("gh pr checks 42", ""),
        ] {
            assert!(ci_poll_note(command, output, true).is_none(), "{command}");
        }
    }

    /// Without the `wait` tool the note names no tool that is not there.
    #[test]
    fn ci_poll_note_without_the_wait_tool_names_no_wait_call() {
        let note = ci_poll_note("gh pr checks 42 --watch", "", false).expect("watch");
        assert!(!note.contains("wait {"), "{note}");
        assert!(note.contains("one blocking check"), "{note}");
    }
}
