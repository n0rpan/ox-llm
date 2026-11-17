//! #868: side-effect-free help for every public command and nested verb.
//!
//! One table holds the usage and one synthetic example of each public command path. A
//! subcommand's `--help` / `-h` and `oxa help <command>` both resolve to the same text, and
//! both are answered before the working directory, the network guard, the trust store,
//! settings, resources, package execution or sign-in are touched. A path that no public
//! command has is a usage error (exit 2), never a silent fallback.

/// One public command path and its help text.
pub struct Entry {
    /// The command path, for example `["mcp", "add"]`.
    pub path: &'static [&'static str],
    /// One line, shown after the command name.
    pub summary: &'static str,
    /// Usage lines, without the leading two spaces.
    pub usage: &'static [&'static str],
    /// One synthetic example (placeholder paths and hosts only).
    pub example: &'static str,
}

const BASE: &[Entry] = &[
    Entry {
        path: &["help"],
        summary: "Show help for a command and exit",
        usage: &["oxa help [command]"],
        example: "oxa help mcp add",
    },
    Entry {
        path: &["status"],
        summary: "Read a saved session's current progress and stall signal",
        usage: &["oxa status <session-id|session-file> [--session-dir <dir>]"],
        example: "oxa status 20260101-120000-abcd",
    },
    Entry {
        path: &["install"],
        summary: "Install a package source and add it to settings",
        usage: &["oxa install <source> [-l]"],
        example: "oxa install https://example.invalid/tools.git",
    },
    Entry {
        path: &["remove"],
        summary: "Remove a package source from settings",
        usage: &["oxa remove <source> [-l]"],
        example: "oxa remove https://example.invalid/tools.git",
    },
    Entry {
        path: &["uninstall"],
        summary: "Remove a package source from settings (alias of `oxa remove`)",
        usage: &["oxa uninstall <source> [-l]"],
        example: "oxa uninstall https://example.invalid/tools.git",
    },
    Entry {
        path: &["update"],
        summary: "Update packages from their git or local sources",
        usage: &["oxa update [source] [--extensions]"],
        example: "oxa update",
    },
    Entry {
        path: &["list"],
        summary: "List installed packages",
        usage: &["oxa list"],
        example: "oxa list",
    },
    Entry {
        path: &["login"],
        summary: "Owner-only native experimental ChatGPT sign-in",
        usage: &["oxa login chatgpt --experimental [--paste] [--replace]"],
        example: "oxa login chatgpt --experimental",
    },
    Entry {
        path: &["login", "chatgpt"],
        summary: "Owner-only native experimental ChatGPT sign-in",
        usage: &["oxa login chatgpt --experimental [--paste] [--replace]"],
        example: "oxa login chatgpt --experimental --paste",
    },
    Entry {
        path: &["logout"],
        summary: "Remove the owner ChatGPT token record",
        usage: &["oxa logout chatgpt"],
        example: "oxa logout chatgpt",
    },
    Entry {
        path: &["logout", "chatgpt"],
        summary: "Remove the owner ChatGPT token record",
        usage: &["oxa logout chatgpt"],
        example: "oxa logout chatgpt",
    },
    Entry {
        path: &["send"],
        summary: "Queue a session follow-up, waking its TUI or RPC owner",
        usage: &["oxa send <session-id> \"text\""],
        example: "oxa send 20260101-120000-abcd \"task finished\"",
    },
    Entry {
        path: &["config"],
        summary: "Print every setting with its value and source",
        usage: &["oxa config [-l]"],
        example: "oxa config",
    },
    Entry {
        path: &["mcp"],
        summary: "Manage MCP servers: add, remove, list, login, logout",
        usage: &["oxa mcp <add|remove|list|login|logout>"],
        example: "oxa mcp list",
    },
    Entry {
        path: &["mcp", "add"],
        summary: "Add an MCP server to the user or project configuration",
        usage: &[
            "oxa mcp add <name> [-l] [options] -- <command> [args...]",
            "oxa mcp add <name> [-l] --url <url> [options]",
        ],
        example: "oxa mcp add files -- npx -y @modelcontextprotocol/server-filesystem ~/project",
    },
    Entry {
        path: &["mcp", "remove"],
        summary: "Remove an MCP server from the user or project configuration",
        usage: &["oxa mcp remove <name> [-l]"],
        example: "oxa mcp remove files",
    },
    Entry {
        path: &["mcp", "list"],
        summary: "Connect to every enabled MCP server and report its state",
        usage: &["oxa mcp list [--json]"],
        example: "oxa mcp list --json",
    },
    Entry {
        path: &["mcp", "login"],
        summary: "Run the OAuth sign-in of an MCP server",
        usage: &["oxa mcp login <name> [--timeout <seconds>]"],
        example: "oxa mcp login files --timeout 300",
    },
    Entry {
        path: &["mcp", "logout"],
        summary: "Delete the stored credentials of an MCP server",
        usage: &["oxa mcp logout <name>"],
        example: "oxa mcp logout files",
    },
    Entry {
        path: &["identity"],
        summary: "Show, or rewrite from this machine and the engine, the identity in the system prompt",
        usage: &["oxa identity [--refresh] [--offline]"],
        example: "oxa identity --refresh",
    },
    Entry {
        path: &["report-issue"],
        summary: "Draft a scrubbed ox-llm issue; submit posts it only after you type yes",
        usage: &["oxa report-issue <draft|submit>"],
        example: "oxa report-issue draft --title \"Crash on start\" --notes \"steps to reproduce\"",
    },
    Entry {
        path: &["report-issue", "draft"],
        summary: "Write a scrubbed issue draft under the agent directory",
        usage: &["oxa report-issue draft --title TITLE [--notes-file FILE | --notes TEXT]"],
        example: "oxa report-issue draft --title \"Crash on start\" --notes-file notes.md",
    },
    Entry {
        path: &["report-issue", "submit"],
        summary: "Post a saved draft after you type yes",
        usage: &["oxa report-issue submit DRAFT"],
        example: "oxa report-issue submit ~/.oxa/agent/issue-drafts/20260101-120000.md",
    },
    Entry {
        path: &["work"],
        summary: "Run tasks in isolated worktrees with external acceptance checks",
        usage: &[
            "oxa work <taskfile> [--rerun]",
            "oxa work --issue N --model MODEL [--rerun]",
            "oxa work status [--json]",
            "oxa work board --shadow --repo OWNER/REPO --once",
        ],
        example: "oxa work tasks.json",
    },
    Entry {
        // Migrate this command declaration to the shared registry in #1062.
        path: &["work", "board"],
        summary: "Sample a private read-only GitHub work board",
        usage: &[
            "oxa work board --shadow --repo OWNER/REPO --once",
            "oxa work board --shadow --repo OWNER/REPO --token-stdin [--once]",
        ],
        example: "oxa work board --shadow --repo example/project --once",
    },
    Entry {
        path: &["work", "status"],
        summary: "List the saved work records and their liveness",
        usage: &["oxa work status [--json]"],
        example: "oxa work status --json",
    },
    Entry {
        path: &["wipe"],
        summary: "Delete one session (or every session) and its spill files",
        usage: &[
            "oxa wipe --session <id> [--engine]",
            "oxa wipe --all [--yes] [--engine]",
        ],
        example: "oxa wipe --session 20260101-120000-abcd",
    },
];

#[cfg(feature = "share")]
const SHARE: &[Entry] = &[
    Entry {
        path: &["share"],
        summary: "Manage private guest keys and daily limits",
        usage: &["oxa share key <add|list|revoke>"],
        example: "oxa share key list",
    },
    Entry {
        path: &["share", "key"],
        summary: "Manage private guest keys and daily limits",
        usage: &["oxa share key <add|list|revoke>"],
        example: "oxa share key list",
    },
    Entry {
        path: &["share", "key", "add"],
        summary: "Create a guest key and print its bearer once",
        usage: &["oxa share key add NAME --daily-requests N --daily-tokens N"],
        example: "oxa share key add Tablet --daily-requests 100 --daily-tokens 100000",
    },
    Entry {
        path: &["share", "key", "list"],
        summary: "List guest keys without their bearers",
        usage: &["oxa share key list"],
        example: "oxa share key list",
    },
    Entry {
        path: &["share", "key", "revoke"],
        summary: "Revoke a guest key by id",
        usage: &["oxa share key revoke ID"],
        example: "oxa share key revoke 3f2a1b",
    },
];

/// Every public command path of this build, in table order.
fn entries() -> Vec<&'static Entry> {
    #[cfg(not(feature = "share"))]
    {
        BASE.iter().collect()
    }
    #[cfg(feature = "share")]
    {
        let mut all: Vec<&'static Entry> = BASE.iter().collect();
        all.extend(SHARE.iter());
        all
    }
}

/// The help text of exactly this command path, or `None` when no public command has it.
pub fn text(path: &[&str]) -> Option<String> {
    entries().into_iter().find(|e| e.path == path).map(render)
}

/// Existing table's early CLI bridge; the shared #1062 registry will generate this route.
pub(crate) fn from_argv(argv: &[String]) -> Option<Result<String, &'static str>> {
    let (name, words) = argv.split_first()?;
    if name == "help" {
        let path: Vec<_> = words.iter().map(String::as_str).collect();
        return Some(text(if path.is_empty() { &["help"] } else { &path }).ok_or("unknown help command"));
    }
    requested(name, words).map(Ok)
}

/// The longest public command path that is a prefix of `words`.
fn longest(words: &[&str]) -> Option<&'static Entry> {
    (1..=words.len())
        .rev()
        .find_map(|n| entries().into_iter().find(|e| e.path == &words[..n]))
}

/// The help text when the words after a command name ask for it with `--help` or `-h`.
///
/// The path is the command name plus the leading words that are not options, so
/// `oxa mcp add NAME --help` resolves `mcp add` and `oxa wipe --session x --help` resolves
/// `wipe`. `None` when no word asks for help.
pub fn requested(name: &str, words: &[String]) -> Option<String> {
    if !words.iter().any(|w| w == "--help" || w == "-h") {
        return None;
    }
    let mut path = vec![name];
    for word in words {
        if word == "--help" || word == "-h" || word.starts_with('-') {
            break;
        }
        path.push(word.as_str());
    }
    longest(&path).map(render)
}

/// The text of one entry: the command, its summary, the usage lines and one example.
fn render(entry: &Entry) -> String {
    let mut out = String::new();
    out.push_str(&format!("oxa {} - {}\n\n", entry.path.join(" "), entry.summary));
    out.push_str("Usage:\n");
    for line in entry.usage {
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str(&format!("\nExample:\n  {}\n", entry.example));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry renders a usage line and exactly one example.
    #[test]
    fn every_entry_has_usage_and_one_example() {
        for entry in entries() {
            let text = render(entry);
            assert!(text.contains("Usage:\n"), "{}", entry.path.join(" "));
            assert_eq!(text.matches("Example:\n").count(), 1, "{}", entry.path.join(" "));
            assert!(!entry.usage.is_empty(), "{}", entry.path.join(" "));
            assert!(!entry.example.is_empty(), "{}", entry.path.join(" "));
        }
    }

    /// No two entries claim the same path.
    #[test]
    fn paths_are_unique() {
        let all = entries();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.path, b.path, "duplicate help path");
            }
        }
    }

    /// `requested` resolves the longest prefix and ignores option words.
    #[test]
    fn requested_resolves_nested_paths() {
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(requested("mcp", &words(&["add", "--help"]))
            .unwrap()
            .contains("oxa mcp add -"));
        assert!(requested("mcp", &words(&["add", "files", "--help"]))
            .unwrap()
            .contains("oxa mcp add -"));
        assert!(requested("wipe", &words(&["--session", "x", "--help"]))
            .unwrap()
            .contains("oxa wipe -"));
        assert!(requested("config", &words(&["-l", "-h"]))
            .unwrap()
            .contains("oxa config -"));
        assert!(requested("mcp", &words(&["list"])).is_none());
    }

    /// An unknown path has no text; the caller turns that into a usage error.
    #[test]
    fn unknown_paths_have_no_text() {
        assert!(text(&["frobnicate"]).is_none());
        assert!(text(&["mcp", "frobnicate"]).is_none());
    }
}
