//! One `Effects` value per simple command: what the command does beyond its own name, read off
//! the tokenizer's words once. The danger rules, the replay checks and the publish checks read
//! this value instead of each re-parsing the words.
//!
//! Pure: nothing here touches the file system. A consumer resolves the raw strings
//! (`dirs`, `reads`) against its own working directory and guard.
use crate::danger::{base_name, peel, short_has, Stage};

/// What one simple command does, after its launchers (`command`, `env`, `exec`, `nice`, ...)
/// are unwrapped.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Effects {
    /// The command and its arguments after the launchers.
    pub(crate) argv: Vec<String>,
    /// Every `env -C` / `env --chdir` of the launcher chain, in order.
    pub(crate) dirs: Vec<String>,
    /// Run through `sudo`, `doas`, `su`, `pkexec` or `run0`.
    pub(crate) privileged: bool,
    /// The targets are not known (`xargs`, or a computed directory before it).
    pub(crate) unknown_targets: bool,
    /// Scripts a launcher runs (`su -c`, `flock -c`, `env -S`, `watch`).
    pub(crate) inline: Vec<String>,
    /// Raw strings the command may read: `<` redirect targets and every non-option word, plus
    /// the tail after a leading `@`, after `=` and after `=@` (`curl -d @file`, `--flag=path`).
    pub(crate) reads: Vec<String>,
    /// The command prints a login token (`gh auth token`, `git credential fill`, ...).
    pub(crate) credential_source: bool,
}

/// Commands that print a credential: (command, leading non-option words).
const CREDENTIAL_SOURCES: &[(&str, &[&str])] = &[
    ("gh", &["auth", "token"]),
    ("git", &["credential", "fill"]),
    ("secret-tool", &["lookup"]),
    ("security", &["find-generic-password"]),
    ("security", &["find-internet-password"]),
    ("gcloud", &["auth", "print-access-token"]),
    ("gcloud", &["auth", "print-identity-token"]),
    ("aws", &["configure", "export-credentials"]),
    ("op", &["read"]),
];

/// The effects of the simple command `words` with `redirects`. `unknown_in`: an enclosing
/// command already made the targets unknown.
pub(crate) fn of(words: &[String], redirects: &[(String, String)], unknown_in: bool) -> Effects {
    let peeled = peel(words, unknown_in);
    let argv: Vec<String> = words.get(peeled.start..).unwrap_or(&[]).to_vec();
    let mut reads: Vec<String> = redirects
        .iter()
        .filter(|(op, _)| op == "<")
        .map(|(_, target)| target.clone())
        .collect();
    for word in argv.iter().skip(1) {
        add_reads(word, &mut reads);
    }
    let credential_source = credential_source(&argv);
    Effects {
        argv,
        dirs: peeled.dirs,
        privileged: peeled.privileged,
        unknown_targets: peeled.unknown,
        inline: peeled.inline,
        reads,
        credential_source,
    }
}

/// The path-like strings one argument stands for.
fn add_reads(word: &str, reads: &mut Vec<String>) {
    let mut push = |text: &str| {
        if let Some(tail) = text.strip_prefix('@') {
            if !tail.is_empty() {
                reads.push(tail.to_string());
            }
        }
        if !text.is_empty() {
            reads.push(text.to_string());
        }
    };
    if word.starts_with('-') {
        if let Some((_, value)) = word.split_once('=') {
            push(value);
        }
        return;
    }
    push(word);
    if let Some((_, value)) = word.split_once('=') {
        push(value);
    }
}

fn credential_source(argv: &[String]) -> bool {
    let Some(first) = argv.first() else {
        return false;
    };
    let name = base_name(first);
    let args = &argv[1..];
    let positional: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    if CREDENTIAL_SOURCES
        .iter()
        .any(|(cmd, words)| *cmd == name && positional.starts_with(words))
    {
        return true;
    }
    // `gh auth status --show-token` prints the token too.
    name == "gh"
        && positional.starts_with(&["auth", "status"])
        && args
            .iter()
            .any(|a| a == "--show-token" || (!a.starts_with("--") && a.starts_with('-') && short_has(a, 't')))
}

/// `true` when the command line takes input that can change between the owner's answer and a
/// replay of the same text: an input redirection or process substitution, a reader that turns
/// its input into arguments (`xargs`, `parallel`, `read`, `mapfile`, `readarray`,
/// `--files-from`), or a pipeline fed by anything but a literal producer
/// (`cat reviewed_key | tee -a ~/.ssh/authorized_keys`).
pub(crate) fn mutable_input(stages: &[Stage]) -> bool {
    let is_reader = |w: &str| {
        matches!(w, "xargs" | "parallel" | "read" | "mapfile" | "readarray")
            || w.ends_with("/xargs")
            || w.ends_with("/parallel")
            || w.starts_with("--files-from")
    };
    let mut feeder: Option<&Stage> = None;
    for stage in stages {
        if stage.mark.is_some() {
            continue;
        }
        if stage
            .redirects
            .iter()
            .any(|(op, _)| matches!(op.as_str(), "<" | "<>"))
            || stage.words.iter().any(|w| is_reader(w) || w.starts_with("<("))
        {
            return true;
        }
        if stage.pipe_in && !feeder.is_some_and(literal_producer) {
            return true;
        }
        feeder = Some(stage);
    }
    false
}

/// A stage whose output is fixed by its own text: `echo`/`printf`/`true`/`:` with no `$`,
/// backtick or redirect.
fn literal_producer(stage: &Stage) -> bool {
    stage.redirects.is_empty()
        && stage
            .words
            .first()
            .is_some_and(|w| matches!(base_name(w), "echo" | "printf" | "true" | ":"))
        && !stage.words.iter().any(|w| w.contains('$') || w.contains('`'))
}

/// `true` when `word` is `/proc/<anything>/environ`, where the middle part may be dynamic
/// (`/proc/$PPID/environ`, `/proc/self/environ`).
pub(crate) fn is_proc_environ(word: &str) -> bool {
    let Some(rest) = word.strip_prefix("/proc/") else {
        return false;
    };
    rest.strip_suffix("/environ").is_some_and(|mid| !mid.is_empty())
}
