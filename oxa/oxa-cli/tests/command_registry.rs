//! Supported registry routes must describe real, side-effect-free CLI help paths.
use oxa_interactive::command_registry::{entries, parse_cli, Availability, CommandId, Surface};

#[test]
fn supported_registry_cli_paths_have_real_public_help() {
    for entry in entries(Surface::Cli) {
        let words: Vec<_> = entry.path.iter().map(|part| (*part).to_string()).collect();
        assert_eq!(parse_cli(&words).unwrap().command.id, entry.id);
        let actual = oxa_cli::parse_args(&words, &[]).unwrap();
        assert!(
            actual.subcommand.is_some() || actual.help,
            "advertised registry route is only a prompt in the real CLI: {:?}",
            entry.path
        );
        assert!(
            oxa_cli::text(entry.path).is_some(),
            "advertised registry route has no public CLI help: {:?}",
            entry.path
        );
    }
}

#[test]
fn suite_status_json_resolves_to_the_native_status_command() {
    let words = vec!["status".to_owned(), "--json".to_owned()];
    let resolved = parse_cli(&words).expect("status is declared");
    assert_eq!(resolved.command.id, CommandId::Status);
    assert!(resolved.command.arguments.iter().any(|arg| arg.name == "json"));
    assert_eq!(resolved.args, &words[1..]);
    let parsed = oxa_cli::parse_args(&words, &[]).expect("status parses");
    assert!(
        matches!(parsed.subcommand, Some(oxa_cli::Subcommand::Status(args))
        if args == ["--json"])
    );
}

#[test]
fn engine_admission_does_not_reserve_oxa_prompt_words() {
    let words = vec!["admission".to_owned(), "criteria".to_owned()];
    assert!(parse_cli(&words).is_none());
    let args = oxa_cli::parse_args(&words, &[]).unwrap();
    assert!(args.subcommand.is_none());
    assert_eq!(args.messages, words);
}

#[test]
fn work_status_declaration_matches_the_existing_cli_route() {
    let words = vec!["work".into(), "status".into(), "--json".into()];
    let resolved = parse_cli(&words).unwrap();
    assert_eq!(resolved.command.path, &["work", "status"]);
    assert_eq!(resolved.args, &["--json"]);
    let parsed = oxa_cli::parse_args(&words, &[]).unwrap();
    assert!(matches!(parsed.subcommand, Some(oxa_cli::Subcommand::Work(rest))
        if rest == ["status", "--json"]));
}

#[test]
fn models_declaration_matches_the_existing_cli_route() {
    let words: Vec<String> = ["models", "--json"].iter().map(|w| w.to_string()).collect();
    let resolved = parse_cli(&words).expect("declared CLI route");
    assert_eq!(resolved.command.id, CommandId::Models);
    assert_eq!(resolved.args, &["--json"]);
    assert_eq!(resolved.command.surfaces.cli, Availability::Supported);
    for surface in [Surface::Slash, Surface::Menu, Surface::Web] {
        assert!(
            matches!(resolved.command.surfaces.at(surface), Availability::Exception(_)),
            "Models declares {surface:?} without a handler"
        );
    }
    let parsed = oxa_cli::parse_args(&words, &[]).unwrap();
    assert!(
        matches!(parsed.subcommand, Some(oxa_cli::Subcommand::Models(rest))
        if rest == ["--json"])
    );
    // #1559: a search word is part of the route, and a leading `--offline` keeps it a command
    // so the catalog can report the offline refusal itself.
    let searched: Vec<String> = ["models", "anthropic"].iter().map(|w| w.to_string()).collect();
    let resolved = parse_cli(&searched).expect("search word resolves");
    assert_eq!(resolved.command.id, CommandId::Models);
    assert_eq!(resolved.args, &["anthropic"]);
    assert!(matches!(oxa_cli::parse_args(&searched, &[]).unwrap().subcommand,
            Some(oxa_cli::Subcommand::Models(rest)) if rest == ["anthropic"]));
    let bare: Vec<String> = vec!["models".into()];
    assert_eq!(
        oxa_cli::parse_args(&bare, &[]).unwrap().subcommand,
        Some(oxa_cli::Subcommand::Models(Vec::new()))
    );
    let offline: Vec<String> = vec!["--offline".into(), "models".into()];
    assert_eq!(
        oxa_cli::parse_args(&offline, &[]).unwrap().subcommand,
        Some(oxa_cli::Subcommand::Models(Vec::new()))
    );
    assert!(
        oxa_cli::text(&["models"]).is_some(),
        "the route has public CLI help"
    );
}

#[test]
fn memory_verbs_are_declared_with_their_surfaces() {
    for (words, id) in [
        (vec!["memory"], CommandId::Memory),
        (vec!["memory", "list"], CommandId::MemoryList),
        (vec!["memory", "show", "build-notes"], CommandId::MemoryShow),
        (vec!["memory", "rm", "build-notes"], CommandId::MemoryRm),
    ] {
        let words: Vec<String> = words.into_iter().map(String::from).collect();
        let resolved = parse_cli(&words).expect("declared CLI route");
        assert_eq!(resolved.command.id, id, "{words:?}");
        assert_eq!(resolved.command.surfaces.cli, Availability::Supported);
        for surface in [Surface::Slash, Surface::Menu, Surface::Web] {
            assert!(
                matches!(resolved.command.surfaces.at(surface), Availability::Exception(_)),
                "{id:?} declares {surface:?} without a handler: {words:?}"
            );
        }
    }
    // The verbs win the longest match over their parent.
    let words: Vec<String> = ["memory", "list"].iter().map(|w| w.to_string()).collect();
    assert_eq!(parse_cli(&words).unwrap().command.id, CommandId::MemoryList);
}
