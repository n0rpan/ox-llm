#![cfg(all(feature = "upstream-openai", feature = "anthropic-client", feature = "ledger"))]
//! The ChatGPT plan's model list against a local fake of the backend; no real token or API.
#[path = "common/openai.rs"]
#[allow(dead_code)]
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
        .plan_models(
            &backend.base_url(),
            &path,
            "synthetic-account",
            refresh(),
            Duration::from_secs(5),
        )
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
    let base = backend.base_url();
    let call = || {
        client.plan_models(
            &base,
            &path,
            "synthetic-account",
            refresh(),
            Duration::from_secs(5),
        )
    };
    assert!(matches!(call().await, Err(client::Error::SignedOut)));
    assert!(matches!(
        call().await,
        Err(client::Error::Http { status: 500, .. })
    ));
    assert!(matches!(call().await, Err(client::Error::Protocol)));
    // A response that echoes the account is refused, like any credential echo.
    assert!(matches!(call().await, Err(client::Error::Protocol)));
}

#[tokio::test]
async fn a_request_for_another_account_than_the_cache_key_names_is_refused_unsent() {
    let (_home, path) = Home::new("pin");
    let backend = MockOpenAi::start(vec![Reply::json(json!({"models":[{"slug":"synthetic-sol"}]}))]);
    let result = Client::new()
        .unwrap()
        .plan_models(
            &backend.base_url(),
            &path,
            "another-account",
            refresh(),
            Duration::from_secs(5),
        )
        .await;
    assert!(matches!(result, Err(client::Error::Credentials)));
    assert!(backend.requests().is_empty(), "no request for the wrong account");
}

#[tokio::test]
async fn a_token_expired_reply_refreshes_once_and_retries_once() {
    let (_home, path) = Home::new("expired");
    let refresher = MockOpenAi::start(vec![Reply::json(
        json!({"access_token":"synthetic-recovered-access","refresh_token":"synthetic-rotated","expires_in":3600}),
    )]);
    let backend = MockOpenAi::start(vec![
        Reply::error(401, "token_expired", "synthetic expiry"),
        Reply::json(json!({"models":[{"slug":"synthetic-sol","context_window":1000}]})),
    ]);
    let endpoint = refresher.base_url();
    let refresh = Refresh {
        endpoint: &endpoint,
        client_id: "synthetic-client",
        timeout: Duration::from_secs(2),
    };
    let models = Client::new()
        .unwrap()
        .plan_models(
            &backend.base_url(),
            &path,
            "synthetic-account",
            refresh,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(models, vec![("synthetic-sol".to_owned(), Some(1000))]);
    assert_eq!(refresher.requests().len(), 1);
    let requests = backend.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].header("authorization"),
        Some("Bearer synthetic-recovered-access")
    );
}

#[tokio::test]
async fn a_second_token_expired_reply_is_not_retried_again() {
    let (_home, path) = Home::new("expired-twice");
    let refresher = MockOpenAi::start(vec![Reply::json(
        json!({"access_token":"synthetic-recovered-access","refresh_token":"synthetic-rotated","expires_in":3600}),
    )]);
    let backend = MockOpenAi::start(vec![
        Reply::error(401, "token_expired", "synthetic expiry"),
        Reply::error(401, "token_expired", "synthetic expiry"),
    ]);
    let endpoint = refresher.base_url();
    let refresh = Refresh {
        endpoint: &endpoint,
        client_id: "synthetic-client",
        timeout: Duration::from_secs(2),
    };
    let result = Client::new()
        .unwrap()
        .plan_models(
            &backend.base_url(),
            &path,
            "synthetic-account",
            refresh,
            Duration::from_secs(5),
        )
        .await;
    assert!(matches!(result, Err(client::Error::SignedOut)));
    assert_eq!(backend.requests().len(), 2);
    assert_eq!(refresher.requests().len(), 1);
}

#[tokio::test]
async fn a_reply_echoing_the_token_of_an_earlier_attempt_is_refused() {
    // The stored token is already expired locally, so attempt 1 refreshes it (retiring the
    // stored one); the backend then says token_expired for the refreshed token, attempt 2 refreshes
    // again, and the final reply echoes the first refreshed token.
    let (_home, path) = Home::new("echo-earlier");
    let past = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() - 10;
    fs::write(
        &path,
        json!({"access_token":TOKEN,"refresh_token":"synthetic-refresh","account_id":"synthetic-account","expires_at":past})
            .to_string(),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let refresher = MockOpenAi::start(vec![
        Reply::json(
            json!({"access_token":"synthetic-first-access","refresh_token":"synthetic-first-refresh","expires_in":3600}),
        ),
        Reply::json(
            json!({"access_token":"synthetic-second-access","refresh_token":"synthetic-second-refresh","expires_in":3600}),
        ),
    ]);
    let backend = MockOpenAi::start(vec![
        Reply::error(401, "token_expired", "synthetic expiry"),
        Reply::json(json!({"models":[{"slug":"synthetic-first-access"}]})),
    ]);
    let endpoint = refresher.base_url();
    let refresh = Refresh {
        endpoint: &endpoint,
        client_id: "synthetic-client",
        timeout: Duration::from_secs(2),
    };
    let result = Client::new()
        .unwrap()
        .plan_models(
            &backend.base_url(),
            &path,
            "synthetic-account",
            refresh,
            Duration::from_secs(5),
        )
        .await;
    assert!(matches!(result, Err(client::Error::Protocol)));
}
