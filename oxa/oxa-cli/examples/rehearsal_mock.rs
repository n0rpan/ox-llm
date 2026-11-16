//! Synthetic interactive fixture for the integration/UX rehearsal lanes only.
#[allow(dead_code)]
#[path = "../../oxc-gateway/tests/common/anthropic.rs"]
mod anthropic;
use anthropic::{MockAnthropic, Reply, CREDENTIAL};
use serde_json::json;
use std::{fs, io, net::TcpListener, os::unix::fs::PermissionsExt, path::PathBuf};
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}
fn main() -> io::Result<()> {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("supply a new absolute scratch directory outside a repository"),
    );
    assert!(root.is_absolute());
    fs::create_dir(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    for dir in [
        "home/.oxa/agent",
        "home/.config/oxc/llm",
        "work",
        "tmp",
        "shims",
        "traces",
    ] {
        fs::create_dir_all(root.join(dir))?;
    }
    let mock = MockAnthropic::start(
        (0..128)
            .map(|_| Reply::text("Synthetic native gateway answer.", 11, 0, 0, 5))
            .collect(),
    );
    let trap = TcpListener::bind("127.0.0.1:0")?;
    trap.set_nonblocking(true)?;
    let trap_url = format!("http://{}", trap.local_addr()?);
    let settings = json!({"defaultProjectTrust":"always","defaultModel":"synthetic-cloud","defaultThinkingLevel":"off",
        "retry":{"enabled":false},"oxa":{"engine":{"inProcessCloud":true},"cloud":{"daily_budget_usd":1,
        "models":{"synthetic-cloud":{"provider":"anthropic","upstream_model":"synthetic-anthropic","base_url":mock.base_url(),
        "context_window":128000,"max_output":64,"pricing":{"input_per_million":2,"output_per_million":3}}}}}});
    fs::write(root.join("home/.oxa/agent/settings.json"), settings.to_string())?;
    let key = root.join("home/.config/oxc/llm/cloud-secrets.json");
    fs::write(&key, json!({"anthropic":{"api_key":CREDENTIAL}}).to_string())?;
    fs::set_permissions(key, fs::Permissions::from_mode(0o600))?;
    fs::write(root.join("work/note.txt"), "Synthetic fixture note.\n")?;
    for name in ["python", "python3"] {
        let shim = root.join("shims").join(name);
        fs::write(
            &shim,
            "#!/bin/sh\nprintf invoked >> \"$PYTHON_MARKER\"\nexit 91\n",
        )?;
        fs::set_permissions(shim, fs::Permissions::from_mode(0o700))?;
    }
    let binary = std::env::current_exe()?
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("oxa");
    let mut env = vec![
        ("HOME", root.join("home").display().to_string()),
        ("PATH", root.join("shims").display().to_string()),
        (
            "OXA_AGENT_DIR",
            root.join("home/.oxa/agent").display().to_string(),
        ),
        ("TMPDIR", root.join("tmp").display().to_string()),
        ("OXA_ENGINE_URL", trap_url),
        ("PYTHON_MARKER", root.join("python-marker").display().to_string()),
        ("OXA_TEST_ISOLATION", "1".into()),
        ("OXA_SYSTEM_SKILLS_DIR", String::new()),
        ("TERM", "xterm-256color".into()),
        ("LANG", "C.UTF-8".into()),
    ];
    for (name, suffix) in [
        ("XDG_CONFIG_HOME", ".config"),
        ("XDG_CACHE_HOME", ".cache"),
        ("XDG_DATA_HOME", ".local/share"),
    ] {
        env.push((name, root.join("home").join(suffix).display().to_string()));
    }
    let script = format!(
        "#!/bin/sh\ncd {} || exit 1\nexec /usr/bin/env -i {} {} \"$@\"\n",
        quote(&root.join("work").display().to_string()),
        env.iter()
            .map(|(k, v)| format!("{k}={}", quote(v)))
            .collect::<Vec<_>>()
            .join(" "),
        quote(&binary.display().to_string())
    );
    fs::write(root.join("run-oxa"), script)?;
    fs::set_permissions(root.join("run-oxa"), fs::Permissions::from_mode(0o700))?;
    println!(
        "Fixture ready: {} (128 synthetic replies). Keep this process alive; press Enter to stop.",
        root.display()
    );
    // Detect bare TCP connects too. The mock and trap are fixture processes, not product daemons.
    let marker = root.join("controller-marker");
    let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = stopped.clone();
    let watcher = std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            match trap.accept() {
                Ok(_) => {
                    fs::write(&marker, b"connected").unwrap();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("controller trap: {e}"),
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    stopped.store(true, std::sync::atomic::Ordering::Relaxed);
    watcher.join().unwrap();
    println!("Synthetic upstream requests: {}", mock.requests().len());
    mock.assert_healthy();
    Ok(())
}
