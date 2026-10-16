mod common;

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use common::*;
use futures_util::future::BoxFuture;
use futures_util::{FutureExt, SinkExt, StreamExt};
use serde_json::{Value, json};
use tailnet_connector::hub_server::hosts::{CallError, HostMap, Hosts};
use tailnet_connector::hub_server::{self, HubState};
use tailnet_connector::tailscale::{Status, StatusError};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

type ReadStatus = fn() -> BoxFuture<'static, Result<Status, StatusError>>;

const PEER_NAME: &str = "macbook";
const HEARTBEAT: Duration = Duration::from_secs(30);

fn status_with_peer_at(ip: &str) -> Status {
    serde_json::from_value(json!({
        "Self": {
            "HostName": "vps",
            "DNSName": "vps.tail722322.ts.net.",
            "OS": "linux",
            "TailscaleIPs": ["100.65.233.54"],
            "Online": true,
        },
        "Peer": {
            "nodekey:a": {
                "HostName": "Marios-MacBook-Pro",
                "DNSName": "macbook.tail722322.ts.net.",
                "OS": "macOS",
                "TailscaleIPs": [ip],
                "Online": true,
            },
        },
    }))
    .unwrap()
}

fn loopback_peer() -> BoxFuture<'static, Result<Status, StatusError>> {
    async { Ok(status_with_peer_at("127.0.0.1")) }.boxed()
}

fn no_matching_peer() -> BoxFuture<'static, Result<Status, StatusError>> {
    async { Ok(status_with_peer_at("100.70.1.2")) }.boxed()
}

fn unreadable_status() -> BoxFuture<'static, Result<Status, StatusError>> {
    async { Err(StatusError::Spawn(io::Error::other("tailscale not found"))) }.boxed()
}

fn start_hub(listener: TcpListener, read_status: ReadStatus) -> Hosts {
    let hosts = Hosts::default();
    let state = HubState {
        hosts: hosts.clone(),
        notify: Duration::from_secs(300),
        read_status,
    };
    tokio::spawn(hub_server::serve(listener, state));

    hosts
}

async fn hub(read_status: ReadStatus) -> (Hosts, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();

    (start_hub(listener, read_status), address)
}

async fn try_connect(address: SocketAddr) -> Result<Socket, WsError> {
    let stream = TcpStream::connect(address).await.unwrap();

    client_async(format!("ws://{address}/connect"), stream)
        .await
        .map(|(socket, _)| socket)
}

async fn connect(address: SocketAddr) -> Socket {
    try_connect(address).await.unwrap()
}

fn echo_tool(description: &str) -> Value {
    json!({
        "name": "echo",
        "description": description,
        "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } },
    })
}

async fn register(socket: &mut Socket, machine: &str, services: Value) {
    send(
        socket,
        json!({ "type": "register", "machine": machine, "services": services }),
    )
    .await;
}

/// Answers the next call like the hub tests' fake connector: `fail` errors, anything else echoes.
async fn answer(socket: &mut Socket) {
    let call = receive(socket).await;
    let reply = if call["tool"] == "fail" {
        json!({ "type": "error", "id": call["id"], "message": "upstream exploded" })
    } else {
        let text = format!(
            "{}/{}: {}",
            call["service"].as_str().unwrap(),
            call["tool"].as_str().unwrap(),
            call["arguments"]
        );
        json!({ "type": "result", "id": call["id"], "result": { "content": [{ "type": "text", "text": text }] } })
    };

    send(socket, reply).await;
}

/// Closes `socket` and waits until the hub has dropped it, which it does only after cleaning up.
async fn close(mut socket: Socket) {
    socket.close(None).await.unwrap();

    while let Some(Ok(_)) = socket.next().await {}
}

async fn close_frame(socket: &mut Socket) -> Option<CloseFrame> {
    loop {
        if let Message::Close(frame) = socket.next().await.unwrap().unwrap() {
            return frame;
        }
    }
}

fn services(hosts: &HostMap, machine: &str) -> Vec<(String, Option<String>)> {
    hosts[machine]
        .services
        .iter()
        .map(|service| (service.name.clone(), service.url.clone()))
        .collect()
}

#[tokio::test]
async fn registers_a_real_connector_under_its_tailscale_peer_name() {
    let url = serve_http_mcp().await;
    let claude_config = claude_config(
        "hub-connect-peer.json",
        json!({ "echo": echo_entry(), "remote": { "type": "http", "url": url } }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = config(&listener, &claude_config, &[]);
    let hosts = start_hub(listener, loopback_peer);
    start_connector(config).await;

    hosts
        .subscribe()
        .wait_for(|hosts| hosts.contains_key(PEER_NAME))
        .await
        .unwrap();
    let snapshot = hosts.snapshot();
    assert_eq!(
        services(&snapshot, PEER_NAME),
        [("echo".to_owned(), None), ("remote".to_owned(), Some(url))]
    );

    let caller = &snapshot[PEER_NAME].caller;
    let arguments = json!({ "text": "hi" });
    let result = caller
        .call(
            "echo".to_owned(),
            "echo".to_owned(),
            arguments.as_object().cloned(),
        )
        .await
        .unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(
        text.starts_with("echo from pid ") && text.ends_with(": hi"),
        "{text}"
    );

    let error = caller
        .call("nope".to_owned(), "echo".to_owned(), None)
        .await
        .unwrap_err();
    assert_eq!(
        error,
        CallError::Connector(format!("nope is not available on {MACHINE}"))
    );
}

#[tokio::test]
async fn names_a_real_connector_without_a_matching_peer_by_its_reported_name() {
    let claude_config = claude_config("hub-connect-reported.json", json!({ "echo": echo_entry() }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = config(&listener, &claude_config, &[]);
    let hosts = start_hub(listener, no_matching_peer);
    start_connector(config).await;

    hosts
        .subscribe()
        .wait_for(|hosts| hosts.contains_key(MACHINE))
        .await
        .unwrap();
    let snapshot = hosts.snapshot();
    assert_eq!(snapshot.keys().collect::<Vec<_>>(), [MACHINE]);
    assert_eq!(services(&snapshot, MACHINE), [("echo".to_owned(), None)]);
}

#[tokio::test]
async fn routes_calls_to_a_registered_connector_and_removes_it_on_close() {
    let (hosts, address) = hub(loopback_peer).await;
    let mut changes = hosts.subscribe();
    let mut socket = connect(address).await;

    register(
        &mut socket,
        "reported-name",
        json!([{ "name": "aside", "tools": [echo_tool("Echo text back")] }]),
    )
    .await;
    changes
        .wait_for(|hosts| hosts.contains_key(PEER_NAME))
        .await
        .unwrap();
    assert_eq!(hosts.snapshot().keys().collect::<Vec<_>>(), [PEER_NAME]);

    let caller = hosts.snapshot()[PEER_NAME].caller.clone();
    let arguments = json!({ "text": "hi" });
    let (result, ()) = tokio::join!(
        caller.call(
            "aside".to_owned(),
            "echo".to_owned(),
            arguments.as_object().cloned()
        ),
        answer(&mut socket),
    );
    assert_eq!(
        result.unwrap().content[0].as_text().unwrap().text,
        r#"aside/echo: {"text":"hi"}"#
    );

    let (error, ()) = tokio::join!(
        caller.call("aside".to_owned(), "fail".to_owned(), None),
        answer(&mut socket),
    );
    assert_eq!(
        error,
        Err(CallError::Connector("upstream exploded".to_owned()))
    );

    changes.borrow_and_update();
    close(socket).await;
    assert!(changes.has_changed().unwrap());
    assert!(changes.borrow_and_update().is_empty());
}

#[tokio::test]
async fn fails_pending_calls_when_the_connector_disconnects() {
    let (hosts, address) = hub(loopback_peer).await;
    let mut socket = connect(address).await;

    register(&mut socket, "x", json!([{ "name": "aside", "tools": [] }])).await;
    hosts
        .subscribe()
        .wait_for(|hosts| hosts.contains_key(PEER_NAME))
        .await
        .unwrap();

    let caller = hosts.snapshot()[PEER_NAME].caller.clone();
    let pending = tokio::spawn(async move {
        caller
            .call("aside".to_owned(), "hang".to_owned(), None)
            .await
    });
    assert_eq!(receive(&mut socket).await["tool"], "hang");

    close(socket).await;
    assert_eq!(pending.await.unwrap(), Err(CallError::Disconnected));
    assert!(hosts.snapshot().is_empty());
}

#[tokio::test]
async fn a_reconnect_under_the_same_name_is_not_removed_when_the_old_socket_closes() {
    let (hosts, address) = hub(loopback_peer).await;
    let mut changes = hosts.subscribe();
    let first_service = |hosts: &HostMap| {
        hosts
            .get(PEER_NAME)
            .map(|host| host.services[0].name.clone())
    };

    let mut first = connect(address).await;
    register(&mut first, "x", json!([{ "name": "old", "tools": [] }])).await;
    changes
        .wait_for(|hosts| first_service(hosts).as_deref() == Some("old"))
        .await
        .unwrap();

    let mut second = connect(address).await;
    register(&mut second, "x", json!([{ "name": "new", "tools": [] }])).await;
    changes
        .wait_for(|hosts| first_service(hosts).as_deref() == Some("new"))
        .await
        .unwrap();

    close(first).await;
    assert_eq!(first_service(&hosts.snapshot()).as_deref(), Some("new"));

    close(second).await;
    assert!(hosts.snapshot().is_empty());
}

#[tokio::test]
async fn moves_a_host_registered_again_under_a_new_name() {
    let (hosts, address) = hub(no_matching_peer).await;
    let mut changes = hosts.subscribe();
    let mut socket = connect(address).await;

    register(&mut socket, "old-name", json!([])).await;
    changes
        .wait_for(|hosts| hosts.contains_key("old-name"))
        .await
        .unwrap();

    register(&mut socket, "new-name", json!([])).await;
    changes
        .wait_for(|hosts| hosts.contains_key("new-name"))
        .await
        .unwrap();
    assert_eq!(hosts.snapshot().keys().collect::<Vec<_>>(), ["new-name"]);
}

#[tokio::test]
async fn closes_with_1008_on_an_invalid_message() {
    let (hosts, address) = hub(loopback_peer).await;
    let register = |machine: &str, service: Value| {
        json!({ "type": "register", "machine": machine, "services": [service] }).to_string()
    };

    for invalid in [
        "not json".to_owned(),
        json!({ "type": "ping" }).to_string(),
        json!({ "type": "result", "id": 1 }).to_string(),
        register("", json!({ "name": "aside", "tools": [] })),
        register("x", json!({ "name": "", "tools": [] })),
        register(
            "x",
            json!({ "name": "aside", "tools": [], "url": "not a url" }),
        ),
    ] {
        let mut socket = connect(address).await;
        socket.send(Message::text(invalid.clone())).await.unwrap();

        let frame = close_frame(&mut socket).await.unwrap();
        assert_eq!(frame.code, CloseCode::Policy, "{invalid}");
        assert_eq!(frame.reason, "invalid message", "{invalid}");
    }

    assert!(hosts.snapshot().is_empty());
}

#[tokio::test]
async fn drops_the_upgrade_when_tailscale_status_cannot_be_read() {
    let (hosts, address) = hub(unreadable_status).await;

    let Err(WsError::Http(response)) = try_connect(address).await else {
        panic!("expected the upgrade to be refused");
    };
    assert_eq!(response.status(), 503);
    assert!(hosts.snapshot().is_empty());
}

#[tokio::test(start_paused = true)]
async fn terminates_a_peer_that_stops_answering_pings() {
    let (hosts, address) = hub(no_matching_peer).await;
    let mut changes = hosts.subscribe();

    let mut awake = connect(address).await;
    register(&mut awake, "awake", json!([])).await;
    // Reading is what answers pings.
    tokio::spawn(async move { while let Some(Ok(_)) = awake.next().await {} });

    let mut asleep = connect(address).await;
    register(&mut asleep, "asleep", json!([])).await;
    changes
        .wait_for(|hosts| hosts.contains_key("awake") && hosts.contains_key("asleep"))
        .await
        .unwrap();
    let start = Instant::now();

    changes
        .wait_for(|hosts| !hosts.contains_key("asleep"))
        .await
        .unwrap();
    assert!(start.elapsed() <= 2 * HEARTBEAT, "{:?}", start.elapsed());

    tokio::time::sleep(10 * HEARTBEAT).await;
    assert_eq!(hosts.snapshot().keys().collect::<Vec<_>>(), ["awake"]);
    drop(asleep);
}
