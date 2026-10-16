mod common;

use std::time::Duration;

use common::*;
use serde_json::json;
use tailnet_connector::checks;
use tailnet_connector::connector::Connector;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

// Accepting keeps the backlog from filling up over the many rounds paused time runs through.
fn check_target(listener: TcpListener) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            listener.accept().await.unwrap();
        }
    })
}

#[tokio::test(start_paused = true)]
async fn services_behind_a_failing_check_drop_out_until_reachable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let target = check_target(listener);
    let checks = temp_path("tcp-checks.json");
    write_json(&checks, &json!({ "echo": { "tcp": address.to_string() } }));
    let claude_config = claude_config(
        "tcp-checks-claude.json",
        json!({ "echo": echo_entry(), "unchecked": echo_entry() }),
    );
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let checks = checks.display().to_string();
    start_connector(config(&hub, &claude_config, &[("TAILNET_CHECKS", &checks)])).await;

    let mut socket = accept(&hub).await;
    assert_eq!(
        registered_names(&receive(&mut socket).await),
        ["echo", "unchecked"]
    );

    // Several unchanged rounds pass without another register before this reply.
    tokio::time::sleep(Duration::from_secs(35)).await;
    let reply = call(&mut socket, 1, "echo", "echo", json!({ "text": "up" })).await;
    let first = echo_pid(result_text(&reply));

    target.abort();
    assert_eq!(registered_names(&receive(&mut socket).await), ["unchecked"]);
    assert_eq!(
        call(&mut socket, 2, "echo", "echo", json!({ "text": "down" })).await,
        json!({ "type": "error", "id": 2, "message": "echo is not available on test-machine" })
    );

    check_target(TcpListener::bind(address).await.unwrap());
    assert_eq!(
        registered_names(&receive(&mut socket).await),
        ["echo", "unchecked"]
    );

    // Dropping out stopped the server, so this call starts a new one.
    let reply = call(&mut socket, 3, "echo", "echo", json!({ "text": "back" })).await;
    assert_ne!(echo_pid(result_text(&reply)), first);
}

#[tokio::test]
async fn a_malformed_checks_file_fails_startup() {
    let checks = temp_path("bad-checks.json");
    write_json(&checks, &json!({ "vphone": { "tcp": "no-port" } }));
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let claude_config = claude_config("bad-checks-claude.json", json!({}));
    let checks = checks.display().to_string();

    let Err(error) =
        Connector::start(config(&hub, &claude_config, &[("TAILNET_CHECKS", &checks)])).await
    else {
        panic!("started with a malformed checks file");
    };

    let message = error.to_string();
    assert!(
        message.starts_with(&format!("cannot parse {checks}: "))
            && message.contains("expected host:port"),
        "{message}"
    );
}

#[test]
fn a_missing_checks_file_means_no_checks() {
    assert!(
        checks::read(&temp_path("missing-checks.json"))
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unix_socket_checks_need_a_live_listener() {
    // Socket paths are limited to ~100 bytes, too short for the target dir.
    let path = std::env::temp_dir().join(format!("tailnet-check-{}.sock", std::process::id()));
    let check: checks::Check = serde_json::from_value(json!({ "unixSocket": path })).unwrap();

    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    assert!(checks::passes(&check).await);

    drop(listener);
    assert!(path.exists(), "the stale socket file stays behind");
    assert!(!checks::passes(&check).await);

    std::fs::remove_file(&path).unwrap();
}
