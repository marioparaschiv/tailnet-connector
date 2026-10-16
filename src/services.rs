use std::collections::BTreeMap;
use std::fmt;

use futures_util::future::join_all;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
    JsonObject, Tool,
};
use rmcp::service::{ClientInitializeError, RunningService, ServiceError};
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use tokio::process::Command;
use tokio::sync::OnceCell;
use tokio::task::JoinError;

use crate::config::{McpServers, ServerConfig, ServerEntry};

type Client = RunningService<RoleClient, ClientConfig>;

/// The MCP servers whose tools could be listed, keyed by name.
pub struct Services {
    servers: BTreeMap<String, Server>,
}

pub struct Server {
    config: ServerConfig,
    tools: Vec<Tool>,
    client: OnceCell<Client>,
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
    /// Starts every supported server once to list its tools, then stops it again.
    pub async fn start(mcp_servers: McpServers) -> Self {
        let configs = mcp_servers
            .into_iter()
            .filter_map(|(name, entry)| match entry {
                ServerEntry::Supported(config) => Some((name, config)),
                ServerEntry::Unsupported(_) => {
                    eprintln!("skipping {name}: only stdio and http MCP servers can be proxied");

                    None
                }
            });

        let listed = join_all(configs.map(|(name, config)| async move {
            let tools = list_tools(&config).await;

            (name, config, tools)
        }))
        .await;

        let servers = listed
            .into_iter()
            .filter_map(|(name, config, tools)| match tools {
                Ok(tools) => Some((
                    name,
                    Server {
                        config,
                        tools,
                        client: OnceCell::new(),
                    },
                )),
                Err(error) => {
                    eprintln!("cannot list tools of {name}: {error}");

                    None
                }
            })
            .collect();

        Self { servers }
    }

    pub fn get(&self, name: &str) -> Option<&Server> {
        self.servers.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Server)> {
        self.servers
            .iter()
            .map(|(name, server)| (name.as_str(), server))
    }
}

impl Server {
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// Starts the server on first use; a failed start is retried by the next call.
    pub async fn call_tool(
        &self,
        tool: String,
        arguments: Option<JsonObject>,
    ) -> Result<CallToolResult, ClientError> {
        let client = self
            .client
            .get_or_try_init(|| connect(&self.config))
            .await?;
        let mut params = CallToolRequestParams::new(tool);
        params.arguments = arguments;

        client.call_tool(params).await.map_err(ClientError::Request)
    }
}

async fn connect(config: &ServerConfig) -> Result<Client, ClientError> {
    let ServerConfig::Stdio { command } = config;
    let transport = TokioChildProcess::new(Command::new(command)).map_err(ClientError::Spawn)?;
    let info = ClientConfig::new(
        ClientCapabilities::default(),
        Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
    );

    info.serve(transport)
        .await
        .map_err(|error| ClientError::Initialize(Box::new(error)))
}

async fn list_tools(config: &ServerConfig) -> Result<Vec<Tool>, ClientError> {
    let client = connect(config).await?;
    let tools = client.list_all_tools().await;
    client.cancel().await.map_err(ClientError::Stop)?;

    tools.map_err(ClientError::Request)
}
