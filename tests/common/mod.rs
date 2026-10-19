#![allow(dead_code)]

use std::collections::HashMap;
use std::env::VarError;
use std::env::consts::EXE_SUFFIX;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde_json::{Value, json};
use tailnet_connector::config::Config;
use tailnet_connector::connector::Connector;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::request::Parts;
use tokio_tungstenite::{WebSocketStream, accept_async};

pub const MACHINE: &str = "test-machine";

pub type Socket = WebSocketStream<TcpStream>;

// Cargo builds examples before running integration tests, next to the test binary's deps dir.
pub fn echo_server_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().unwrap().parent().unwrap();

    profile_dir
        .join("examples")
        .join(format!("echo-server{EXE_SUFFIX}"))
}

pub fn temp_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

pub fn write_json(path: &Path, contents: &Value) {
    std::fs::write(path, contents.to_string()).unwrap();
}

pub fn echo_entry() -> Value {
    json!({ "type": "stdio", "command": echo_server_path() })
}

/// Claude config contents; `numStartups` stands in for the unrelated state Claude Code rewrites.
pub fn claude_json(mcp_servers: Value, num_startups: u64) -> String {
    json!({ "numStartups": num_startups, "mcpServers": mcp_servers }).to_string()
}

/// A Claude config at a fresh temp path holding the given `mcpServers`.
pub fn claude_config(name: &str, mcp_servers: Value) -> PathBuf {
    let path = temp_path(name);
    std::fs::write(&path, claude_json(mcp_servers, 0)).unwrap();

    path
}

/// A Pi agent directory without an `mcp.json`, so tests don't read the real one.
pub fn no_pi_dir() -> PathBuf {
    temp_path("no-pi-agent-dir")
}

/// Config pointing at `hub`, with no checks file and no idle stop unless `vars` sets them.
///
/// Paused time jumps to the next timer whenever the runtime waits on a real process or socket,
/// so a test can cover hours of virtual time; the idle stop stays out of the way unless wanted.
pub fn config(hub: &TcpListener, claude_config: &Path, vars: &[(&str, &str)]) -> Config {
    let mut all = HashMap::from([
        ("TAILNET_IDLE_SECONDS", "10000000".to_owned()),
        (
            "TAILNET_URL",
            format!("http://{}", hub.local_addr().unwrap()),
        ),
        ("TAILNET_MACHINE", MACHINE.to_owned()),
        ("CLAUDE_CONFIG", claude_config.display().to_string()),
        ("PI_CODING_AGENT_DIR", no_pi_dir().display().to_string()),
        (
            "TAILNET_CHECKS",
            temp_path("missing-checks.json").display().to_string(),
        ),
    ]);
    all.extend(vars.iter().map(|(name, value)| (*name, value.to_string())));

    Config::from_lookup(|name| all.get(name).cloned().ok_or(VarError::NotPresent)).unwrap()
}

pub async fn start_connector(config: Config) {
    let connector = Connector::start(config).await.unwrap();
    tokio::spawn(connector.run());
}

pub async fn accept(hub: &TcpListener) -> Socket {
    let (stream, _) = hub.accept().await.unwrap();

    accept_async(stream).await.unwrap()
}

pub async fn receive(socket: &mut Socket) -> Value {
    loop {
        if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

pub async fn send(socket: &mut Socket, message: Value) {
    socket
        .send(Message::text(message.to_string()))
        .await
        .unwrap();
}

/// Sends a call and returns the next message, which must be its reply.
pub async fn call(
    socket: &mut Socket,
    id: u64,
    service: &str,
    tool: &str,
    arguments: Value,
) -> Value {
    send(
        socket,
        json!({ "type": "call", "id": id, "service": service, "tool": tool, "arguments": arguments }),
    )
    .await;

    let reply = receive(socket).await;
    assert_eq!(reply["id"], id, "{reply}");

    reply
}

pub fn registered_names(register: &Value) -> Vec<&str> {
    assert_eq!(register["type"], "register", "{register}");

    register["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|service| service["name"].as_str().unwrap())
        .collect()
}

pub fn result_text(reply: &Value) -> &str {
    assert_eq!(reply["type"], "result", "{reply}");

    reply["result"]["content"][0]["text"].as_str().unwrap()
}

/// The pid of the fixture that answered an `echo` call.
pub fn echo_pid(text: &str) -> u32 {
    text.strip_prefix("echo from pid ")
        .and_then(|rest| rest.split(':').next())
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("not an echo reply: {text}"))
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

pub async fn serve_http_mcp() -> String {
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
