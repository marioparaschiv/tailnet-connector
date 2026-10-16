mod common;

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use common::*;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::http::request::Parts;

#[tokio::test]
async fn registers_fixture_tools_and_proxies_calls() {
    let claude_config = claude_config("proxies-calls.json", json!({ "echo": echo_entry() }));
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    start_connector(config(&hub, &claude_config, &[])).await;

    let mut socket = accept(&hub).await;
    let register = receive(&mut socket).await;

    assert_eq!(register["machine"], MACHINE);
    assert_eq!(registered_names(&register), ["echo"]);
    let echo_tool = register["services"][0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "echo")
        .unwrap();
    assert_eq!(
        echo_tool["inputSchema"]["properties"]["text"]["type"],
        "string"
    );

    // The hanging call stays in flight while the others are answered.
    send(
        &mut socket,
        json!({ "type": "call", "id": 1, "service": "echo", "tool": "hang" }),
    )
    .await;
    send(
        &mut socket,
        json!({ "type": "call", "id": 2, "service": "echo", "tool": "echo", "arguments": { "text": "hi" } }),
    )
    .await;
    send(
        &mut socket,
        json!({ "type": "call", "id": 3, "service": "nope", "tool": "echo" }),
    )
    .await;
    send(
        &mut socket,
        json!({ "type": "call", "id": 4, "service": "echo", "tool": "missing" }),
    )
    .await;

    let mut replies = HashMap::new();

    for _ in 0..3 {
        let reply = receive(&mut socket).await;
        replies.insert(reply["id"].as_u64().unwrap(), reply);
    }

    let text = result_text(&replies[&2]);
    assert!(
        text.starts_with("echo from pid ") && text.ends_with(": hi"),
        "{text}"
    );

    assert_eq!(
        replies[&3],
        json!({ "type": "error", "id": 3, "message": "nope is not available on test-machine" })
    );

    assert_eq!(replies[&4]["type"], "error");
    assert_eq!(replies[&4]["message"], "Mcp error: -32602: tool not found");
}

#[tokio::test(start_paused = true)]
async fn reconnects_with_backoff_and_registers_again() {
    let claude_config = claude_config("reconnects.json", json!({}));
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    start_connector(config(&hub, &claude_config, &[])).await;

    let mut socket = accept(&hub).await;
    assert_eq!(receive(&mut socket).await["type"], "register");
    let mut last = Instant::now();
    drop(socket);

    let mut wait_for_attempt = async |expected: u64| {
        let (stream, _) = hub.accept().await.unwrap();
        assert_eq!(last.elapsed(), Duration::from_secs(expected));
        last = Instant::now();

        stream
    };

    // Dropping before the handshake fails the connect, so the delay keeps doubling.
    drop(wait_for_attempt(1).await);
    drop(wait_for_attempt(2).await);
    let stream = wait_for_attempt(4).await;

    let mut socket = accept_async(stream).await.unwrap();
    assert_eq!(receive(&mut socket).await["type"], "register");
    drop(socket);

    let mut socket = accept_async(wait_for_attempt(1).await).await.unwrap();
    assert_eq!(receive(&mut socket).await["machine"], MACHINE);
}

#[derive(Clone)]
struct HeaderServer;

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct HeaderRequest {
    name: String,
}

#[tool_router]
impl HeaderServer {
    #[tool(description = "Return a request header, empty when missing")]
    fn header(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(HeaderRequest { name }): Parameters<HeaderRequest>,
    ) -> String {
        parts
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }
}

#[tool_handler]
impl ServerHandler for HeaderServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
}

async fn serve_http_mcp() -> String {
    let service = StreamableHttpService::new(
        || Ok(HeaderServer),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let router = axum::Router::new().nest_service("/mcp", service);
    tokio::spawn(async move { axum::serve(listener, router).await });

    url
}

#[tokio::test]
async fn proxies_http_servers_with_their_headers() {
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let claude_config = claude_config(
        "proxies-http.json",
        json!({
            "remote": {
                "type": "http",
                "url": serve_http_mcp().await,
                "headers": { "X-Tailnet-Test": "secret" },
            },
        }),
    );
    start_connector(config(&hub, &claude_config, &[])).await;

    let mut socket = accept(&hub).await;
    let register = receive(&mut socket).await;
    assert_eq!(registered_names(&register), ["remote"]);
    assert_eq!(register["services"][0]["tools"][0]["name"], "header");

    let reply = call(
        &mut socket,
        1,
        "remote",
        "header",
        json!({ "name": "x-tailnet-test" }),
    )
    .await;
    assert_eq!(result_text(&reply), "secret");
}

#[tokio::test]
async fn layers_stdio_env_over_the_connector_environment() {
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let claude_config = claude_config(
        "stdio-env.json",
        json!({
            "echo": {
                "type": "stdio",
                "command": echo_server_path(),
                "args": [],
                "env": { "TAILNET_FIXTURE": "from-config" },
            },
        }),
    );
    start_connector(config(&hub, &claude_config, &[])).await;

    let mut socket = accept(&hub).await;
    receive(&mut socket).await;

    let reply = call(
        &mut socket,
        1,
        "echo",
        "env",
        json!({ "name": "TAILNET_FIXTURE" }),
    )
    .await;
    assert_eq!(result_text(&reply), "from-config");

    let reply = call(&mut socket, 2, "echo", "env", json!({ "name": "PATH" })).await;
    assert_eq!(result_text(&reply), std::env::var("PATH").unwrap());
}

#[tokio::test]
async fn skips_unsupported_entries_and_the_hub_itself() {
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub_url = format!("http://{}", hub.local_addr().unwrap());
    let claude_config = claude_config(
        "skips-entries.json",
        json!({
            "tailnet": { "type": "http", "url": format!("{hub_url}/mcp") },
            "legacy": { "type": "sse", "url": "http://127.0.0.1:9/sse" },
            "echo": echo_entry(),
        }),
    );

    let mut connector = tokio::process::Command::new(env!("CARGO_BIN_EXE_tailnet-connector"))
        .env("TAILNET_URL", &hub_url)
        .env("TAILNET_MACHINE", MACHINE)
        .env("CLAUDE_CONFIG", &claude_config)
        .env("TAILNET_CHECKS", temp_path("missing-checks.json"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let mut socket = accept(&hub).await;
    assert_eq!(registered_names(&receive(&mut socket).await), ["echo"]);

    let mut stdout = BufReader::new(connector.stdout.take().unwrap()).lines();
    assert_eq!(
        stdout.next_line().await.unwrap().unwrap(),
        "available: echo"
    );
    assert_eq!(
        stdout.next_line().await.unwrap().unwrap(),
        format!("connected to {hub_url} as {MACHINE}: echo")
    );

    connector.kill().await.unwrap();
    let mut stderr = String::new();
    connector
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .await
        .unwrap();

    assert_eq!(
        stderr,
        "skipping legacy: only stdio and http MCP servers can be proxied\n"
    );
}

fn run_binary(envs: &[(&str, &Path)]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tailnet-connector"));

    for name in [
        "TAILNET_URL",
        "TAILNET_MACHINE",
        "TAILNET_IDLE_SECONDS",
        "CLAUDE_CONFIG",
        "TAILNET_CHECKS",
        "TAILNET_CHECK_SECONDS",
    ] {
        command.env_remove(name);
    }

    let output = command.envs(envs.iter().copied()).output().unwrap();
    assert!(!output.status.success());

    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn fails_at_startup_without_tailnet_url() {
    assert_eq!(run_binary(&[]), "TAILNET_URL is required\n");
}

#[test]
fn fails_at_startup_on_invalid_number() {
    let stderr = run_binary(&[
        ("TAILNET_URL", Path::new("http://127.0.0.1:9")),
        ("TAILNET_IDLE_SECONDS", Path::new("-1")),
    ]);

    assert_eq!(
        stderr,
        "TAILNET_IDLE_SECONDS=\"-1\" is invalid: must be a whole number of seconds, at least 1\n"
    );
}

#[test]
fn fails_at_startup_on_malformed_claude_config() {
    let path = temp_path("malformed-claude.json");
    std::fs::write(&path, "{ \"mcpServers\": ").unwrap();

    let stderr = run_binary(&[
        ("TAILNET_URL", Path::new("http://127.0.0.1:9")),
        ("CLAUDE_CONFIG", &path),
    ]);

    assert!(
        stderr.starts_with(&format!("cannot parse {}: ", path.display())),
        "{stderr}"
    );
}

#[test]
fn fails_at_startup_on_malformed_checks_file() {
    let claude_config = claude_config("checks-malformed-claude.json", json!({}));
    let checks = temp_path("malformed-checks.json");
    std::fs::write(&checks, r#"{ "vphone": { "tcp": "no-port" } }"#).unwrap();

    let stderr = run_binary(&[
        ("TAILNET_URL", Path::new("http://127.0.0.1:9")),
        ("CLAUDE_CONFIG", &claude_config),
        ("TAILNET_CHECKS", &checks),
    ]);

    assert!(
        stderr.starts_with(&format!("cannot parse {}: ", checks.display()))
            && stderr.contains("expected host:port"),
        "{stderr}"
    );
}
