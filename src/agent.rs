//! `agent` mode: an agent's MCP server over stdio that proxies the hub, telling it which MCP
//! servers the agent already has so the hub leaves those out of the agent's tools.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::pin::pin;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ClientCapabilities, ClientConfig,
    ContentBlock, Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerConfig,
};
use rmcp::service::{
    NotificationContext, Peer, RequestContext, RunningService, ServerInitializeError, ServiceError,
};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientHandler, ErrorData, RoleClient, RoleServer, ServerHandler, ServiceExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{RwLock, mpsc};
use tokio::time::{Instant, MissedTickBehavior, interval_at};
use tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};

use crate::config::{Config, ConfigError};
use crate::known_servers::{KnownServer, KnownServers};
use crate::reload;
use crate::services::ClientError;

const KNOWN_SERVERS_HEADER: HeaderName = HeaderName::from_static("x-tailnet-known-servers");

#[derive(Debug)]
pub enum AgentError {
    Config(ConfigError),
    Header(InvalidHeaderValue),
    Hub(ClientError),
    Serve(Box<ServerInitializeError>),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => error.fmt(f),
            Self::Header(error) => write!(f, "cannot send the known servers to the hub: {error}"),
            Self::Hub(error) => write!(f, "hub: {error}"),
            Self::Serve(error) => write!(f, "cannot serve the agent: {error}"),
        }
    }
}

impl std::error::Error for AgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Header(error) => Some(error),
            Self::Hub(error) => Some(error),
            Self::Serve(error) => Some(error),
        }
    }
}

/// Serves the agent on `reader`/`writer` until it disconnects, reconnecting to the hub whenever
/// the servers configured for `project` change.
pub async fn run<R, W>(
    config: &Config,
    project: &Path,
    reader: R,
    writer: W,
) -> Result<(), AgentError>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let mut known = KnownServers::read(config, project).map_err(AgentError::Config)?;
    let (tool_list_changed, mut tool_list_changes) = mpsc::unbounded_channel();
    let hub = Hub::connect(config.mcp_url.clone(), known.servers(), tool_list_changed).await?;
    let proxy = Proxy {
        instructions: hub.instructions().await,
        hub: Arc::new(hub),
    };
    let hub = Arc::clone(&proxy.hub);
    let agent = proxy
        .serve((reader, writer))
        .await
        .map_err(|error| AgentError::Serve(Box::new(error)))?;
    let peer = agent.peer().clone();
    let mut disconnected = pin!(agent.waiting());
    let mut reload = interval_at(Instant::now() + reload::POLL, reload::POLL);
    reload.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = &mut disconnected => return Ok(()),
            Some(()) = tool_list_changes.recv() => notify_tool_list_changed(&peer).await,
            _ = reload.tick() => match known.changed() {
                Ok(None) => {}
                Ok(Some(servers)) => match hub.reconnect(servers).await {
                    Ok(()) => notify_tool_list_changed(&peer).await,
                    Err(error) => eprintln!("cannot reconnect to the hub: {error}"),
                },
                Err(error) => eprintln!("cannot reload the MCP config: {error}"),
            },
        }
    }
}

async fn notify_tool_list_changed(peer: &Peer<RoleServer>) {
    if let Err(error) = peer.notify_tool_list_changed().await {
        eprintln!("cannot tell the agent its tools changed: {error}");
    }
}

type HubClient = RunningService<RoleClient, HubEvents>;

/// The hub session, reopened with the agent's current servers whenever they change or the
/// hub drops it.
struct Hub {
    url: String,
    tool_list_changed: mpsc::UnboundedSender<()>,
    session: RwLock<(Vec<KnownServer>, HubClient)>,
}

impl Hub {
    async fn connect(
        url: String,
        known: &[KnownServer],
        tool_list_changed: mpsc::UnboundedSender<()>,
    ) -> Result<Self, AgentError> {
        let client = Self::open(&url, known, &tool_list_changed).await?;

        Ok(Self {
            url,
            tool_list_changed,
            session: RwLock::new((known.to_vec(), client)),
        })
    }

    async fn open(
        url: &str,
        known: &[KnownServer],
        tool_list_changed: &mpsc::UnboundedSender<()>,
    ) -> Result<HubClient, AgentError> {
        let known = serde_json::to_string(known).expect("known servers serialize to JSON");
        let header = HeaderValue::from_str(&known).map_err(AgentError::Header)?;
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(url)
                .custom_headers(HashMap::from([(KNOWN_SERVERS_HEADER, header)])),
        );

        HubEvents(tool_list_changed.clone())
            .serve(transport)
            .await
            .map_err(|error| AgentError::Hub(ClientError::Initialize(Box::new(error))))
    }

    async fn instructions(&self) -> Option<String> {
        let session = self.session.read().await;

        session.1.peer_info()?.instructions.clone()
    }

    async fn reconnect(&self, known: &[KnownServer]) -> Result<(), AgentError> {
        let client = Self::open(&self.url, known, &self.tool_list_changed).await?;
        // The replaced session is cancelled as it drops.
        *self.session.write().await = (known.to_vec(), client);

        Ok(())
    }

    /// Runs `request` against the hub, reopening the session and retrying once if the hub
    /// dropped it, as it does when it restarts.
    async fn request<T, F, Fut>(&self, request: F) -> Result<T, AgentError>
    where
        F: Fn(Peer<RoleClient>) -> Fut,
        Fut: Future<Output = Result<T, ServiceError>>,
    {
        let peer = self.session.read().await.1.peer().clone();

        match request(peer).await {
            Err(ServiceError::TransportSend(_) | ServiceError::TransportClosed) => {
                let known = self.session.read().await.0.clone();
                self.reconnect(&known).await?;
                let peer = self.session.read().await.1.peer().clone();

                request(peer)
                    .await
                    .map_err(|error| AgentError::Hub(ClientError::Request(error)))
            }
            result => result.map_err(|error| AgentError::Hub(ClientError::Request(error))),
        }
    }
}

/// Passes the hub's tool list changes on to the agent.
struct HubEvents(mpsc::UnboundedSender<()>);

impl ClientHandler for HubEvents {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        // The receiver only goes away once the agent disconnected, when there is no one to tell.
        let _ = self.0.send(());
    }

    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(ClientCapabilities::default(), implementation())
    }
}

struct Proxy {
    hub: Arc<Hub>,
    instructions: Option<String>,
}

impl ServerHandler for Proxy {
    fn get_info(&self) -> ServerConfig {
        let info = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(implementation());

        match &self.instructions {
            Some(instructions) => info.with_instructions(instructions),
            None => info,
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        match self
            .hub
            .request(|peer| async move { peer.list_all_tools().await })
            .await
        {
            Ok(tools) => Ok(ListToolsResult::with_all_items(tools)),
            Err(AgentError::Hub(ClientError::Request(ServiceError::McpError(error)))) => Err(error),
            Err(error) => Err(ErrorData::internal_error(error.to_string(), None)),
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        match self
            .hub
            .request(|peer| {
                let request = request.clone();

                async move { peer.call_tool(request).await }
            })
            .await
        {
            Ok(result) => Ok(result.into()),
            Err(AgentError::Hub(ClientError::Request(ServiceError::McpError(error)))) => Err(error),
            // An error result reaches the agent's model, where a protocol error would not.
            Err(error) => {
                Ok(CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into())
            }
        }
    }
}

fn implementation() -> Implementation {
    Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}
