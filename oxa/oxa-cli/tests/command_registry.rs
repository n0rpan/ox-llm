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

/// #1542: `oxa models` is a declared CLI route with a real handler, so it is never sent to the
/// model as a prompt, and a stray word after it stays a usage error.
#[test]
fn models_is_declared_as_a_cli_route() {
    let words: Vec<String> = vec!["models".into()];
    let resolved = parse_cli(&words).expect("declared CLI route");
    assert_eq!(resolved.command.id, CommandId::Models);
    assert_eq!(resolved.command.surfaces.cli, Availability::Supported);
    for surface in [Surface::Slash, Surface::Menu, Surface::Web] {
        assert!(
            matches!(resolved.command.surfaces.at(surface), Availability::Exception(_)),
            "models declares {surface:?} without a handler"
        );
    }
    assert_eq!(
        oxa_cli::parse_args(&words, &[]).unwrap().subcommand,
        Some(oxa_cli::Subcommand::Models(Vec::new()))
    );
    let json: Vec<String> = vec!["models".into(), "--json".into()];
    assert_eq!(
        oxa_cli::parse_args(&json, &[]).unwrap().subcommand,
        Some(oxa_cli::Subcommand::Models(vec!["--json".into()]))
    );
    assert!(
        oxa_cli::text(&["models"]).is_some(),
        "the route has public CLI help"
    );
}
