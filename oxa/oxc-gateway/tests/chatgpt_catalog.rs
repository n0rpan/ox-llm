#![cfg(all(feature = "upstream-openai", feature = "anthropic-client", feature = "ledger"))]
//! The ChatGPT plan's model list against a local fake of the backend; no real token or API.
#[path = "common/openai.rs"]
mod mock;
use mock::{MockOpenAi, Reply};
use oxc_gateway::openai::client::{self, Client, Refresh};
use serde_json::json;
use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const TOKEN: &str = "synthetic-plan-access";

struct Home(PathBuf);
impl Home {
    fn new(tag: &str) -> (Self, PathBuf) {
        let root = std::env::temp_dir().join(format!("chatgpt-catalog-{}-{tag}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("chatgpt-auth.json");
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        write!(
            file,
            "{}",
            json!({"access_token":TOKEN,"refresh_token":"synthetic-refresh","account_id":"synthetic-account","expires_at":now+3600})
        )
        .unwrap();
        (Self(root), path)
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn refresh() -> Refresh<'static> {
    Refresh {
        endpoint: "http://127.0.0.1:9/token",
        client_id: "synthetic-client",
        timeout: Duration::from_secs(2),
    }
}

#[tokio::test]
async fn the_plan_list_is_fetched_like_the_codex_cli_and_keeps_only_ids_and_windows() {
    let (_home, path) = Home::new("list");
    let backend = MockOpenAi::start(vec![Reply::json(json!({"models":[
        {"slug":"synthetic-sol","display_name":"Sol","visibility":"list","context_window":272000,"supported_in_api":false},
        {"slug":"synthetic-old","visibility":"hide","context_window":1000},
        {"slug":"synthetic-nowindow","visibility":"list"}]}))]);
    let models = Client::new()
        .unwrap()
        .plan_models(&backend.base_url(), &path, refresh(), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        models,
        vec![
            ("synthetic-sol".to_owned(), Some(272000)),
            ("synthetic-nowindow".to_owned(), None)
        ]
    );
    let requests = backend.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert!(
        request.target.contains("/models?client_version="),
        "{}",
        request.target
    );
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(request.header("chatgpt-account-id"), Some("synthetic-account"));
    assert_eq!(request.header("originator"), Some("oxc"));
}

#[tokio::test]
async fn a_rejected_sign_in_and_a_failed_list_are_typed_errors() {
    let (_home, path) = Home::new("errors");
    let backend = MockOpenAi::start(vec![
        Reply::error(401, "invalid_request_error", "no"),
        Reply::error(500, "server_error", "no"),
        Reply::json(json!({"models":"not a list"})),
        Reply::json(json!({"models":[{"slug":"synthetic-account"}]})),
    ]);
    let client = Client::new().unwrap();
    let call = || client.plan_models(&backend.base_url(), &path, refresh(), Duration::from_secs(5));
    assert!(matches!(call().await, Err(client::Error::SignedOut)));
    assert!(matches!(
        call().await,
        Err(client::Error::Http { status: 500, .. })
    ));
    assert!(matches!(call().await, Err(client::Error::Protocol)));
    // A response that echoes the account is refused, like any credential echo.
    assert!(matches!(call().await, Err(client::Error::Protocol)));
}
