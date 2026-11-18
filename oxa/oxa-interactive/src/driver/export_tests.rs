//! User export actions must never replace their own saved conversation.
use super::catalog_tests::with_loop;
use super::*;
use oxa_fake::TestEnv;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};

const SESSION: &[u8] = b"{\"type\":\"session\",\"version\":3,\"id\":\"synthetic-export\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/synthetic-project\"}\n{\"type\":\"message\",\"id\":\"sentinel\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"message\":{\"role\":\"user\",\"content\":\"SYNTHETIC_SESSION_SENTINEL\"}}\n";

#[test]
fn export_refuses_source_path_and_link_aliases_before_copy_or_html_admission() {
    for kind in [
        "same",
        "relative",
        "hard-jsonl",
        "symlink-jsonl",
        "hard-html",
        "symlink-html",
        "default-html",
    ] {
        let env = TestEnv::new();
        let source = env.work().join("source.jsonl");
        fs::write(&source, SESSION).unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
        with_loop(|lp, _term, _info, rx| {
            lp.session.file = Some(source.clone());
            lp.session.id = "synthetic-export".into();
            lp.vm.status.cwd = env.work().display().to_string();
            let target = match kind {
                "same" => Some(source.clone()),
                "relative" => Some(PathBuf::from("source.jsonl")),
                "hard-jsonl" | "symlink-jsonl" => Some(env.work().join("alias.jsonl")),
                "hard-html" | "symlink-html" => Some(env.work().join("alias.html")),
                "default-html" => None,
                _ => unreachable!(),
            };
            let resolved = target
                .clone()
                .unwrap_or_else(|| default_output_name(Some(&source), "synthetic-export", &env.work()));
            if kind.starts_with("hard-") {
                fs::hard_link(&source, &resolved).unwrap();
            } else if kind.starts_with("symlink-") || kind == "default-html" {
                symlink(&source, &resolved).unwrap();
            }

            lp.export(target);

            assert_eq!(
                fs::read(&source).unwrap(),
                SESSION,
                "source bytes, header and sentinel: {kind}"
            );
            assert_eq!(
                fs::metadata(&source).unwrap().permissions().mode() & 0o777,
                0o600,
                "private source mode: {kind}"
            );
            assert!(
                rx.try_recv().is_err(),
                "an aliased HTML writer must not be queued: {kind}"
            );
            assert!(
                lp.vm.blocks.iter().any(|b| matches!(b,
                    Block::Notice { level: Level::Error, text } if text.contains("session file")
                )),
                "clear source-alias refusal: {kind}"
            );
        });
    }
}

#[test]
fn export_keeps_new_jsonl_and_html_destinations_working() {
    let env = TestEnv::new();
    let source = env.work().join("source.jsonl");
    fs::write(&source, SESSION).unwrap();
    with_loop(|lp, _term, _info, rx| {
        lp.session.file = Some(source.clone());
        lp.vm.status.cwd = env.work().display().to_string();
        let jsonl = env.work().join("copy.jsonl");
        lp.export(Some(jsonl.clone()));
        assert_eq!(fs::read(jsonl).unwrap(), SESSION);
        assert_eq!(fs::read(&source).unwrap(), SESSION);
        assert!(rx.try_recv().is_err(), "JSONL copy does not need a worker");

        let html = env.work().join("copy.html");
        lp.export(Some(html.clone()));
        assert!(matches!(rx.try_recv(), Ok(SessionCmd::Export { path }) if path == html));
        assert_eq!(fs::read(&source).unwrap(), SESSION);
    });
}
