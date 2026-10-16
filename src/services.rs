use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::future::join_all;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
    JsonObject, Tool,
};
use rmcp::service::{ClientInitializeError, Peer, RunningService, ServiceError};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde::Deserialize;
use tokio::process::Command;
use tokio::task::{AbortHandle, JoinError};

use crate::config::{self, Config, McpServers, ServerConfig};

type Client = RunningService<RoleClient, ClientConfig>;

/// The MCP servers whose tools could be listed, keyed by name.
pub struct Services {
    servers: BTreeMap<String, Arc<Server>>,
}

/// An MCP server that starts on first call and stops once no call has been in flight for `idle`.
pub struct Server {
    name: String,
    config: ServerConfig,
    tools: Vec<Tool>,
    idle: Duration,
    // Held across a start so concurrent calls share it.
    client: tokio::sync::Mutex<Option<Client>>,
    activity: Mutex<Activity>,
}

#[derive(Default)]
struct Activity {
    in_flight: usize,
    idle_stop: Option<AbortHandle>,
}

#[derive(Debug)]
pub enum ClientError {
    Spawn(std::io::Error),
    Initialize(Box<ClientInitializeError>),
    Request(ServiceError),
    Stop(JoinError),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "cannot start: {error}"),
            Self::Initialize(error) => write!(f, "cannot initialize: {error}"),
            Self::Request(error) => error.fmt(f),
            Self::Stop(error) => write!(f, "cannot stop: {error}"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            Self::Initialize(error) => Some(error),
            Self::Request(error) => Some(error),
            Self::Stop(error) => Some(error),
        }
    }
}

impl Services {
    /// Starts every proxyable server once to list its tools, then stops it again.
    pub async fn start(mcp_servers: &McpServers, config: &Config) -> Self {
        let configs =
            mcp_servers
                .iter()
                .filter_map(|(name, entry)| match ServerConfig::deserialize(entry) {
                    Err(_) => {
                        eprintln!(
                            "skipping {name}: only stdio and http MCP servers can be proxied"
                        );

                        None
                    }
                    // Proxying the hub back to itself would loop every call.
                    Ok(ServerConfig::Http { url, .. }) if config::host(&url) == config.hub_host => {
                        None
                    }
                    Ok(server) => Some((name, server)),
                });

        let listed = join_all(configs.map(|(name, server)| async move {
            let tools = list_tools(&server).await;

            (name, server, tools)
        }))
        .await;

        let servers = listed
            .into_iter()
            .filter_map(|(name, server, tools)| match tools {
                Ok(tools) => Some((
                    name.clone(),
                    Arc::new(Server {
                        name: name.clone(),
                        config: server,
                        tools,
                        idle: config.idle,
                        client: tokio::sync::Mutex::default(),
                        activity: Mutex::default(),
                    }),
                )),
                Err(error) => {
                    eprintln!("cannot list tools of {name}: {error}");

                    None
                }
            })
            .collect();

        Self { servers }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<Server>> {
        self.servers.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.servers.keys().map(String::as_str)
    }

    pub async fn stop_all(&self) {
        join_all(self.servers.values().map(|server| server.stop())).await;
    }
}

impl Server {
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// Starts the server if it isn't running; a failed start is retried by the next call.
    pub async fn call_tool(
        self: &Arc<Self>,
        tool: String,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ClientError> {
        let _call = InFlight::begin(self);
        let peer = self.peer().await?;
        let mut params = CallToolRequestParams::new(tool);
        params.arguments = arguments;

        peer.call_tool(params).await.map_err(ClientError::Request)
    }

    /// Stops the server if it is running, logging rather than returning a failure to stop.
    pub async fn stop(&self) {
        if let Some(idle_stop) = self.activity().idle_stop.take() {
            idle_stop.abort();
        }

        let client = self.client.lock().await.take();
        self.shut_down(client).await;
    }

    async fn stop_if_idle(&self) {
        let mut slot = self.client.lock().await;

        if self.activity().in_flight > 0 {
            return;
        }

        let client = slot.take();
        drop(slot);
        self.shut_down(client).await;
    }

    async fn shut_down(&self, client: Option<Client>) {
        let Some(client) = client else {
            return;
        };

        if let Err(error) = client.cancel().await {
            eprintln!("cannot stop {}: {error}", self.name);
        }
    }

    async fn peer(&self) -> Result<Peer<RoleClient>, ClientError> {
        let mut client = self.client.lock().await;

        if let Some(client) = client.as_ref() {
            return Ok(client.peer().clone());
        }

        Ok(client.insert(connect(&self.config).await?).peer().clone())
    }

    fn activity(&self) -> MutexGuard<'_, Activity> {
        self.activity
            .lock()
            .expect("activity lock is never poisoned")
    }
}

/// Counts a call as in flight for as long as it lives, arming the idle stop once the last one ends.
struct InFlight<'a>(&'a Arc<Server>);

impl<'a> InFlight<'a> {
    fn begin(server: &'a Arc<Server>) -> Self {
        let mut activity = server.activity();
        activity.in_flight += 1;

        if let Some(idle_stop) = activity.idle_stop.take() {
            idle_stop.abort();
        }

        Self(server)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut activity = self.0.activity();
        activity.in_flight -= 1;

        if activity.in_flight > 0 {
            return;
        }

        let server = Arc::clone(self.0);
        let idle_stop = tokio::spawn(async move {
            tokio::time::sleep(server.idle).await;
            server.stop_if_idle().await;
        });
        activity.idle_stop = Some(idle_stop.abort_handle());
    }
}

async fn connect(config: &ServerConfig) -> Result<Client, ClientError> {
    let info = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
    );

    let client = match config {
        ServerConfig::Stdio { command, args, env } => {
            let mut command = program(command).map_err(ClientError::Spawn)?;
            command.args(args).envs(env);
            let transport = TokioChildProcess::new(command).map_err(ClientError::Spawn)?;

            info.serve(transport).await
        }
        ServerConfig::Http { url, headers } => {
            let transport = StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(url.to_string())
                    .custom_headers(headers.clone()),
            );

            info.serve(transport).await
        }
    };

    client.map_err(|error| ClientError::Initialize(Box::new(error)))
}

#[cfg(not(windows))]
fn program(command: &str) -> std::io::Result<Command> {
    Ok(Command::new(command))
}

// Windows only finds `.exe` files on PATH, so shims such as `npx.cmd` are resolved through
// PATHEXT here. CREATE_NO_WINDOW keeps console servers from opening a window, since the
// connector itself has no console to share.
#[cfg(windows)]
fn program(command: &str) -> std::io::Result<Command> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut command = rmcp::transport::which_command(command)?;
    command.creation_flags(CREATE_NO_WINDOW);

    Ok(command)
}

async fn list_tools(config: &ServerConfig) -> Result<Vec<Tool>, ClientError> {
    let client = connect(config).await?;
    let tools = client.list_all_tools().await;
    client.cancel().await.map_err(ClientError::Stop)?;

    tools.map_err(ClientError::Request)
}
