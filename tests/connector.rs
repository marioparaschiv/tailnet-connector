use std::collections::HashMap;
use std::env::VarError;
use std::env::consts::EXE_SUFFIX;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tailnet_connector::config::{self, Config, McpServers};
use tailnet_connector::hub;
use tailnet_connector::services::Services;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, accept_async};

const MACHINE: &str = "test-machine";

// Cargo builds examples before running integration tests, next to the test binary's deps dir.
fn echo_server_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().unwrap().parent().unwrap();

    profile_dir
        .join("examples")
        .join(format!("echo-server{EXE_SUFFIX}"))
}

fn write_claude_config(name: &str, contents: &Value) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, contents.to_string()).unwrap();

    path
}

fn config(hub: &TcpListener) -> Config {
    let vars = HashMap::from([
        (
            "TAILNET_URL",
            format!("http://{}", hub.local_addr().unwrap()),
        ),
        ("TAILNET_MACHINE", MACHINE.to_owned()),
    ]);

    Config::from_lookup(|name| vars.get(name).cloned().ok_or(VarError::NotPresent)).unwrap()
}

fn start_connector(config: Config, services: Services) {
    tokio::spawn(async move { hub::run(&config, Arc::new(services)).await });
}

async fn accept(hub: &TcpListener) -> WebSocketStream<TcpStream> {
    let (stream, _) = hub.accept().await.unwrap();

    accept_async(stream).await.unwrap()
}

async fn receive(socket: &mut WebSocketStream<TcpStream>) -> Value {
    loop {
        if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

async fn send(socket: &mut WebSocketStream<TcpStream>, message: Value) {
    socket
        .send(Message::text(message.to_string()))
        .await
        .unwrap();
}

#[tokio::test]
async fn registers_fixture_tools_and_proxies_calls() {
    let claude_config = write_claude_config(
        "proxies-calls.json",
        &json!({
            "mcpServers": {
                "echo": { "type": "stdio", "command": echo_server_path() },
                "remote": { "type": "http", "url": "http://example.com/mcp" },
            },
        }),
    );
    let services = Services::start(config::read_mcp_servers(&claude_config).unwrap()).await;
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    start_connector(config(&hub), services);

    let mut socket = accept(&hub).await;
    let register = receive(&mut socket).await;

    assert_eq!(register["type"], "register");
    assert_eq!(register["machine"], MACHINE);
    assert_eq!(register["services"].as_array().unwrap().len(), 1);
    assert_eq!(register["services"][0]["name"], "echo");
    assert_eq!(register["services"][0]["tools"][0]["name"], "echo");
    assert_eq!(
        register["services"][0]["tools"][0]["inputSchema"]["properties"]["text"]["type"],
        "string"
    );
    assert_eq!(register["services"][0]["tools"][1]["name"], "hang");

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

    let echoed = &replies[&2];
    let text = echoed["result"]["content"][0]["text"].as_str().unwrap();

    assert_eq!(echoed["type"], "result");
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
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    start_connector(config(&hub), Services::start(McpServers::new()).await);

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
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("malformed-claude.json");
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
