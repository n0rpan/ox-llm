//! Supported registry routes must describe real, side-effect-free CLI help paths.
use oxa_interactive::command_registry::{entries, parse_cli, Surface};

#[test]
fn supported_registry_cli_paths_have_real_public_help() {
    for entry in entries(Surface::Cli) {
        let words: Vec<_> = entry.path.iter().map(|part| (*part).to_string()).collect();
        assert_eq!(parse_cli(&words).unwrap().command.id, entry.id);
        assert!(
            oxa_cli::text(entry.path).is_some(),
            "advertised registry route has no public CLI help: {:?}",
            entry.path
        );
    }
}
