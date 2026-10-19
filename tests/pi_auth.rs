mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use common::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tailnet_connector::pi_config;
use tailnet_connector::services::Services;
use tokio::net::TcpListener;

type Form = Vec<(String, String)>;

/// An OAuth-protected MCP server accepting only `a2`, and a token endpoint that grants `a2` for
/// the refresh token `r1`.
struct Remote {
    url: String,
    /// The forms the token endpoint received.
    refreshes: Arc<Mutex<Vec<Form>>>,
}

async fn serve_remote() -> Remote {
    let refreshes = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&refreshes);
    let token = post(move |body: String| async move {
        let form: Form = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        let valid = form.contains(&("refresh_token".into(), "r1".into()));
        recorded.lock().unwrap().push(form);

        let (status, body) = if valid {
            (
                StatusCode::OK,
                json!({ "access_token": "a2", "token_type": "Bearer", "expires_in": 3600, "refresh_token": "r2" }),
            )
        } else {
            (StatusCode::BAD_REQUEST, json!({ "error": "invalid_grant" }))
        };

        (
            status,
            [(header::CONTENT_TYPE, "application/json")],
            body.to_string(),
        )
    });
    let router = http_mcp_router()
        .layer(middleware::from_fn(require_a2))
        .route("/token", token);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });

    Remote { url, refreshes }
}

async fn require_a2(request: Request, next: Next) -> Response {
    if request
        .headers()
        .get(header::AUTHORIZATION)
        .is_some_and(|v| v == "Bearer a2")
    {
        return next.run(request).await;
    }

    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
    )
        .into_response()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// A Pi agent directory signed in to `remote` with `a1`, expiring at `expire_at`.
fn pi_dir(name: &str, remote: &Remote, expire_at: u64) -> PathBuf {
    let dir = temp_path(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let origin = remote.url.trim_end_matches("/mcp");
    write_json(
        &dir.join("mcp.json"),
        &json!({ "mcpServers": { "remote": { "url": remote.url } } }),
    );
    write_json(
        &dir.join("mcp-auth.json"),
        &json!({
            auth_key(remote): {
                "serverUrl": remote.url,
                "discovery": {
                    "authorizationServerUrl": origin,
                    "authorizationServerMetadata": {
                        "token_endpoint": format!("{origin}/token"),
                        "token_endpoint_auth_methods_supported": ["none"],
                    },
                    "resourceMetadata": { "resource": remote.url },
                },
                "clientInformation": { "client_id": "client", "token_endpoint_auth_method": "none" },
                "tokens": { "access_token": "a1", "token_type": "Bearer", "refresh_token": "r1", "scope": "read" },
                "tokensExpireAt": expire_at,
            },
            "mcp__other|https://other.example.com/mcp": { "tokens": { "access_token": "x" } },
        }),
    );

    dir
}

fn auth_key(remote: &Remote) -> String {
    format!("mcp__remote|{}", remote.url)
}

async fn start(dir: &Path) -> Services {
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = config(
        &hub,
        &temp_path("pi-auth-no-claude.json"),
        &[("PI_CODING_AGENT_DIR", dir.to_str().unwrap())],
    );
    let entries = pi_config::read(&config.pi_config).unwrap();

    Services::start(&pi_config::as_claude_entries(entries), &config).await
}

async fn authorization(services: &Services) -> String {
    let result = services
        .get("remote")
        .expect("the remote server listed its tools")
        .call_tool(
            "header".into(),
            json!({ "name": "authorization" }).as_object().cloned(),
        )
        .await
        .unwrap();

    result.content[0].as_text().unwrap().text.clone()
}

fn stored(dir: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join("mcp-auth.json")).unwrap()).unwrap()
}

#[tokio::test]
async fn refreshes_a_rejected_token_and_saves_it_for_pi() {
    let remote = serve_remote().await;
    let dir = pi_dir("pi-auth-rejected", &remote, now_millis() + 3_600_000);

    let services = start(&dir).await;

    assert_eq!(authorization(&services).await, "Bearer a2");
    let refreshes = remote.refreshes.lock().unwrap().clone();
    assert_eq!(refreshes.len(), 1);
    for field in [
        ("grant_type", "refresh_token"),
        ("refresh_token", "r1"),
        ("client_id", "client"),
        ("resource", remote.url.as_str()),
    ] {
        assert!(
            refreshes[0].contains(&(field.0.into(), field.1.into())),
            "{refreshes:?}"
        );
    }

    let stored = stored(&dir);
    let state = &stored[auth_key(&remote)];
    assert_eq!(state["tokens"]["access_token"], "a2");
    assert_eq!(state["tokens"]["refresh_token"], "r2");
    assert_eq!(state["tokens"]["scope"], "read");
    assert!(state["tokensExpireAt"].as_u64().unwrap() > now_millis() + 3_500_000);
    assert_eq!(state["clientInformation"]["client_id"], "client");
    assert_eq!(
        stored["mcp__other|https://other.example.com/mcp"]["tokens"]["access_token"],
        "x"
    );
}

#[tokio::test]
async fn refreshes_an_expired_token_before_sending_it() {
    let remote = serve_remote().await;
    let dir = pi_dir("pi-auth-expired", &remote, now_millis() - 1000);

    let services = start(&dir).await;

    assert_eq!(authorization(&services).await, "Bearer a2");
    assert_eq!(remote.refreshes.lock().unwrap().len(), 1);
}

/// Pi holds the refresh lock and stores a new token, which the connector then uses as it is.
#[tokio::test]
async fn waits_for_a_refresh_by_pi_instead_of_refreshing_again() {
    let remote = serve_remote().await;
    let dir = pi_dir("pi-auth-pi-refreshes", &remote, now_millis() - 1000);
    let hash: String = Sha256::digest(auth_key(&remote).as_bytes())[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let lock = dir.join(format!("mcp-auth-refresh-{hash}.lock"));
    std::fs::create_dir(&lock).unwrap();
    let pi = {
        let (dir, key) = (dir.clone(), auth_key(&remote));

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let mut states = stored(&dir);
            states[&key]["tokens"]["access_token"] = "a2".into();
            states[&key]["tokensExpireAt"] = (now_millis() + 3_600_000).into();
            write_json(&dir.join("mcp-auth.json"), &states);
            std::fs::remove_dir(&lock).unwrap();
        })
    };

    let services = start(&dir).await;
    pi.await.unwrap();

    assert_eq!(authorization(&services).await, "Bearer a2");
    assert!(remote.refreshes.lock().unwrap().is_empty());
}
