mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use common::*;
use rmcp::handler::server::tool::Extension;
use rmcp::model::{CallToolRequestParams, ServerCapabilities, ServerConfig};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ClientHandler, RoleClient, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde_json::{Value, json};
use tailnet_connector::agent;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::http::request::Parts;

/// Stands in for the hub, answering with the known servers the agent sent it.
#[derive(Clone)]
struct FakeHub;

#[tool_router]
impl FakeHub {
    #[tool(description = "Return the x-tailnet-known-servers header of this session")]
    fn known_servers(&self, Extension(parts): Extension<Parts>) -> String {
        parts.headers["x-tailnet-known-servers"]
            .to_str()
            .unwrap()
            .to_owned()
    }
}

#[tool_handler]
impl ServerHandler for FakeHub {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Connected now: test-machine (echo).")
    }
}

async fn serve_fake_hub(hub: TcpListener) {
    let service = StreamableHttpService::new(
        || Ok(FakeHub),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    tokio::spawn(async move { axum::serve(hub, router).await });
}

/// The agent's side of its MCP connection, recording the tool list changes it is told about.
struct Agent(mpsc::UnboundedSender<()>);

impl ClientHandler for Agent {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.0.send(()).unwrap();
    }
}

async fn known_servers(agent: &RunningService<RoleClient, Agent>) -> Value {
    let result = agent
        .call_tool(CallToolRequestParams::new("known_servers"))
        .await
        .unwrap();
    let text = result.content[0].as_text().unwrap().text.as_str();

    serde_json::from_str(text).unwrap()
}

fn project_dir(name: &str) -> PathBuf {
    let path = temp_path(name);
    std::fs::create_dir_all(&path).unwrap();

    path
}

// Several writes can land within one mtime tick, so each rewrite gets its own explicit mtime.
fn write_mcp_json(project: &Path, mcp_servers: Value, generation: u64) {
    let path = project.join(".mcp.json");
    write_json(&path, &json!({ "mcpServers": mcp_servers }));
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(generation))
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn tells_the_hub_which_servers_the_agent_has_and_updates_it() {
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hub_url = format!("http://{}/mcp", hub.local_addr().unwrap());
    let project = project_dir("agent-project");
    let claude_config = temp_path("agent-claude.json");
    write_json(
        &claude_config,
        &json!({
            "mcpServers": {
                "tailnet": { "command": "/usr/local/bin/tailnet-connector", "args": ["agent"] },
                "tailnet-http": { "type": "http", "url": hub_url },
                "linear": { "type": "http", "url": "https://mcp.linear.app/mcp" },
                "aside": { "command": "aside", "args": ["mcp"] },
            },
            "projects": {
                project.to_str().unwrap(): {
                    "mcpServers": { "linear": { "type": "http", "url": "https://linear.example.com/mcp" } },
                },
                "/some/other/project": {
                    "mcpServers": { "elsewhere": { "command": "elsewhere" } },
                },
            },
        }),
    );
    write_mcp_json(
        &project,
        json!({
            "chrome-devtools": { "command": "npx", "args": ["chrome-devtools-mcp"] },
            "unexpanded": { "type": "http", "url": "${API_URL}/mcp" },
        }),
        1,
    );
    let config = config(&hub, &claude_config, &[]);
    serve_fake_hub(hub).await;

    let (agent_side, client_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let (reader, writer) = tokio::io::split(agent_side);
        agent::run(&config, &project, reader, writer).await.unwrap();
    });
    let (tool_list_changed, mut tool_list_changes) = mpsc::unbounded_channel();
    let agent = Agent(tool_list_changed)
        .serve(tokio::io::split(client_side))
        .await
        .unwrap();

    assert_eq!(
        agent.peer_info().unwrap().instructions.as_deref(),
        Some("Connected now: test-machine (echo).")
    );
    let tools = agent.list_all_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "known_servers");
    assert_eq!(
        known_servers(&agent).await,
        json!([
            { "name": "aside" },
            { "name": "chrome-devtools" },
            { "name": "linear", "url": "https://linear.example.com/mcp" },
        ])
    );

    write_mcp_json(
        &temp_path("agent-project"),
        json!({
            "chrome-devtools": { "command": "npx", "args": ["chrome-devtools-mcp"] },
            "github": { "type": "http", "url": "https://api.githubcopilot.com/mcp/" },
        }),
        2,
    );
    tool_list_changes.recv().await.unwrap();

    assert_eq!(
        known_servers(&agent).await,
        json!([
            { "name": "aside" },
            { "name": "chrome-devtools" },
            { "name": "github", "url": "https://api.githubcopilot.com/mcp/" },
            { "name": "linear", "url": "https://linear.example.com/mcp" },
        ])
    );
}
