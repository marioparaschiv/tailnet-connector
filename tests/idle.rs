mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use rmcp::model::JsonObject;
use serde_json::{Value, json};
use tailnet_connector::services::{ClientError, Server, Services};
use tokio::net::TcpListener;

const IDLE: Duration = Duration::from_secs(300);

async fn start_server(name: &str, entry: Value) -> Arc<Server> {
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = config(
        &hub,
        &temp_path("unused.json"),
        &[("TAILNET_IDLE_SECONDS", "300")],
    );
    let mcp_servers = json!({ name: entry }).as_object().unwrap().clone();
    let services = Services::start(&mcp_servers, &config).await;

    Arc::clone(services.get(name).unwrap())
}

fn arguments(text: &str) -> Option<JsonObject> {
    json!({ "text": text }).as_object().cloned()
}

async fn echo(server: &Arc<Server>) -> Result<u32, ClientError> {
    let result = server.call_tool("echo".to_owned(), arguments("hi")).await?;
    let text = result.content[0].as_text().unwrap().text.clone();

    Ok(echo_pid(&text))
}

/// Waits for the stopped fixture to be reaped without moving the paused clock.
#[cfg(target_os = "linux")]
async fn exited(pid: u32) {
    while std::path::Path::new(&format!("/proc/{pid}")).exists() {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn stops_after_sitting_idle_and_starts_again() {
    let server = start_server("echo", echo_entry()).await;

    let first = echo(&server).await.unwrap();
    tokio::time::sleep(IDLE - Duration::from_secs(1)).await;
    assert_eq!(echo(&server).await.unwrap(), first);

    tokio::time::sleep(IDLE + Duration::from_secs(1)).await;
    assert_ne!(echo(&server).await.unwrap(), first);

    #[cfg(target_os = "linux")]
    exited(first).await;
}

#[tokio::test(start_paused = true)]
async fn concurrent_calls_share_one_start() {
    let server = start_server("echo", echo_entry()).await;

    let (a, b) = tokio::join!(echo(&server), echo(&server));

    assert_eq!(a.unwrap(), b.unwrap());
}

#[tokio::test(start_paused = true)]
async fn keeps_running_while_a_call_is_in_flight() {
    let server = start_server("echo", echo_entry()).await;
    let first = echo(&server).await.unwrap();

    let hang = server.call_tool("hang".to_owned(), None);
    tokio::pin!(hang);

    tokio::select! {
        biased;
        _ = &mut hang => panic!("hang returned"),
        _ = tokio::time::sleep(IDLE * 3) => {}
    }

    assert_eq!(echo(&server).await.unwrap(), first);
}

#[tokio::test(start_paused = true)]
async fn retries_a_failed_start_on_the_next_call() {
    let command = temp_path("retried-echo-server");
    std::fs::copy(echo_server_path(), &command).unwrap();
    let server = start_server("echo", json!({ "type": "stdio", "command": command })).await;

    let moved = temp_path("retried-echo-server.moved");
    std::fs::rename(&command, &moved).unwrap();
    let error = echo(&server).await.unwrap_err();
    assert!(error.to_string().starts_with("cannot start: "), "{error}");

    std::fs::rename(&moved, &command).unwrap();
    echo(&server).await.unwrap();
}
