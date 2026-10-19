mod common;

use std::path::Path;
use std::time::{Duration, SystemTime};

use common::*;
use serde_json::json;
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

#[tokio::test(start_paused = true)]
async fn registers_reloaded_servers_while_the_checks_file_is_malformed() {
    let path = claude_config("reload-bad-checks.json", json!({ "echo": echo_entry() }));
    let checks = temp_path("reload-bad-checks-checks.json");
    write_json(&checks, &json!({}));
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let checks_var = checks.display().to_string();
    start_connector(config(&hub, &path, &[("TAILNET_CHECKS", &checks_var)])).await;
    let mut socket = accept(&hub).await;
    assert_eq!(registered_names(&receive(&mut socket).await), ["echo"]);
    let first = echo(&mut socket, 1, "echo").await;

    std::fs::write(&checks, r#"{ "echo": "#).unwrap();
    rewrite(
        &path,
        &claude_json(json!({ "echo": echo_entry(), "added": echo_entry() }), 1),
        1,
    );

    // Only the names available before the reload stay registered until a check succeeds.
    assert_eq!(registered_names(&receive(&mut socket).await), ["echo"]);
    assert_ne!(echo(&mut socket, 2, "echo").await, first);
}

#[tokio::test(start_paused = true)]
async fn shares_pi_servers_alongside_claude_ones() {
    let claude = claude_config(
        "reload-pi-claude.json",
        json!({ "echo": echo_entry(), "both": { "type": "stdio", "command": "missing-mcp" } }),
    );
    let pi_dir = temp_path("reload-pi-agent-dir");
    std::fs::create_dir_all(&pi_dir).unwrap();
    let pi_config = pi_dir.join("mcp.json");
    let echo_path = echo_server_path().display().to_string();
    let pi_json = |servers: serde_json::Value| json!({ "mcpServers": servers }).to_string();
    rewrite(
        &pi_config,
        &pi_json(json!({
            "pi-echo": { "command": echo_path },
            "both": { "command": echo_path },
            "off": { "command": echo_path, "enabled": false },
        })),
        1,
    );
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pi_dir_var = pi_dir.display().to_string();
    start_connector(config(
        &hub,
        &claude,
        &[("PI_CODING_AGENT_DIR", &pi_dir_var)],
    ))
    .await;
    let mut socket = accept(&hub).await;

    // Pi's `both` replaced Claude's, whose command does not exist.
    assert_eq!(
        registered_names(&receive(&mut socket).await),
        ["both", "echo", "pi-echo"]
    );
    echo(&mut socket, 1, "pi-echo").await;

    rewrite(
        &pi_config,
        &pi_json(json!({ "pi-echo": { "command": echo_path }, "off": { "command": echo_path } })),
        2,
    );

    assert_eq!(
        registered_names(&receive(&mut socket).await),
        ["echo", "off", "pi-echo"]
    );
}
