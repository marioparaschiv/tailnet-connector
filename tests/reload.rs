mod common;

use std::path::Path;
use std::time::{Duration, SystemTime};

use common::*;
use serde_json::{Value, json};
use tailnet_connector::reload;
use tokio::net::TcpListener;

// Several writes can land within one mtime tick, so each rewrite gets its own explicit mtime.
fn rewrite(path: &Path, contents: &str, generation: u64) {
    std::fs::write(path, contents).unwrap();
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(generation))
        .unwrap();
}

fn claude_json(mcp_servers: Value, num_startups: u64) -> String {
    json!({ "numStartups": num_startups, "mcpServers": mcp_servers }).to_string()
}

async fn past_next_poll() {
    tokio::time::sleep(reload::POLL + Duration::from_secs(1)).await;
}

async fn echo(socket: &mut Socket, id: u64, service: &str) -> u32 {
    let reply = call(socket, id, service, "echo", json!({ "text": "hi" })).await;

    echo_pid(result_text(&reply))
}

#[tokio::test(start_paused = true)]
async fn registers_again_when_mcp_servers_change() {
    let path = claude_config("reload-adds.json", json!({ "echo": echo_entry() }));
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    start_connector(config(&hub, &path, &[])).await;
    let mut socket = accept(&hub).await;
    assert_eq!(registered_names(&receive(&mut socket).await), ["echo"]);
    let first = echo(&mut socket, 1, "echo").await;

    rewrite(
        &path,
        &claude_json(json!({ "echo": echo_entry(), "added": echo_entry() }), 1),
        1,
    );

    assert_eq!(
        registered_names(&receive(&mut socket).await),
        ["added", "echo"]
    );
    echo(&mut socket, 2, "added").await;
    // Every running server was stopped by the reload.
    assert_ne!(echo(&mut socket, 3, "echo").await, first);
}

#[tokio::test(start_paused = true)]
async fn ignores_other_changes_and_keeps_serving_through_invalid_json() {
    let path = claude_config("reload-ignores.json", json!({ "echo": echo_entry() }));
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    start_connector(config(&hub, &path, &[])).await;
    let mut socket = accept(&hub).await;
    assert_eq!(registered_names(&receive(&mut socket).await), ["echo"]);
    let first = echo(&mut socket, 1, "echo").await;

    // A reply arriving first shows that no register was sent in between.
    rewrite(&path, &claude_json(json!({ "echo": echo_entry() }), 2), 1);
    past_next_poll().await;
    assert_eq!(echo(&mut socket, 2, "echo").await, first);

    rewrite(&path, r#"{ "mcpServers": { "echo": "#, 2);
    past_next_poll().await;
    assert_eq!(echo(&mut socket, 3, "echo").await, first);

    rewrite(
        &path,
        &claude_json(json!({ "echo": echo_entry(), "added": echo_entry() }), 2),
        3,
    );
    assert_eq!(
        registered_names(&receive(&mut socket).await),
        ["added", "echo"]
    );
}
