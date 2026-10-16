use std::process::Command;
use std::time::Duration;

use futures_util::FutureExt;
use tailnet_connector::hub_server::hosts::Hosts;
use tailnet_connector::hub_server::{self, HubState};
use tailnet_connector::tailscale::Status;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn status(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: hub\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();

    response.lines().next().unwrap().to_owned()
}

#[tokio::test]
async fn answers_unknown_paths_with_404() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = HubState {
        hosts: Hosts::default(),
        notify: Duration::from_secs(300),
        read_status: || Status::read().boxed(),
    };
    tokio::spawn(hub_server::serve(listener, state));

    for path in ["/", "/nope", "/mcp/extra", "/connect/extra"] {
        assert_eq!(status(port, path).await, "HTTP/1.1 404 Not Found", "{path}");
    }
}

#[test]
fn exits_naming_an_invalid_variable() {
    for (name, value) in [
        ("TAILNET_MCP_PORT", "0"),
        ("TAILNET_MCP_NOTIFY_SECONDS", "4"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tailnet-connector"))
            .arg("hub")
            .env_remove("TAILNET_MCP_PORT")
            .env_remove("TAILNET_MCP_NOTIFY_SECONDS")
            .env("TAILNET_MCP_HOST", "127.0.0.1")
            .env(name, value)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(!output.status.success(), "{name}");
        assert!(stderr.starts_with(&format!("{name}=")), "{stderr}");
    }
}
