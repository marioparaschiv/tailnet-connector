//! `/mcp`: streamable HTTP sessions for agents, each with its own view of the catalog.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::Request;
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeRequestParams, InitializeResult, JsonObject, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::{NotificationContext, Peer, RequestContext};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::json;
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use crate::hub_server::HubState;
use crate::hub_server::catalog::{Catalog, Catalogs, normalise_url};
use crate::hub_server::hosts::CallError;
use crate::known_servers::{KNOWN_SERVERS_HEADER, KnownServer};

const LIST_MACHINES: &str = "list_machines";

pub(super) fn service(state: &HubState) -> StreamableHttpService<Session, LocalSessionManager> {
    let state = state.clone();
    let catalogs = Catalogs::new(&state.hosts);

    StreamableHttpService::new(
        move || {
            Ok(Session {
                state: state.clone(),
                catalogs: catalogs.clone(),
                known: OnceLock::new(),
            })
        },
        Arc::default(),
        // Agents reach the hub by its Tailscale address or name, which the default list of
        // loopback hosts would turn away.
        StreamableHttpServerConfig::default().disable_allowed_hosts(),
    )
}

/// Hands the agent's known servers to its session through the request extensions, answering an
/// invalid header with 400.
pub(super) async fn read_known_servers(mut request: Request, next: Next) -> Response {
    if let Some(header) = request.headers().get(KNOWN_SERVERS_HEADER) {
        match parse_known_servers(header) {
            Ok(known) => {
                request.extensions_mut().insert(known);
            }
            Err(reason) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("Invalid {KNOWN_SERVERS_HEADER}: {reason}"),
                )
                    .into_response();
            }
        }
    }

    next.run(request).await
}

fn parse_known_servers(header: &HeaderValue) -> Result<Vec<KnownServer>, String> {
    let text = header
        .to_str()
        .map_err(|_| "not visible ASCII".to_owned())?;
    let servers: Vec<KnownServer> =
        serde_json::from_str(text).map_err(|error| format!("not a list of servers: {error}"))?;

    servers
        .into_iter()
        .map(|server| {
            if server.name.is_empty() {
                return Err("a server has an empty name".to_owned());
            }

            let url = server
                .url
                .map(|url| {
                    normalise_url(&url)
                        .map_err(|error| format!("{}: {url} is not a URL: {error}", server.name))
                })
                .transpose()?;

            Ok(KnownServer { url, ..server })
        })
        .collect()
}

/// One agent's session.
pub(super) struct Session {
    state: HubState,
    catalogs: Catalogs,
    /// The servers the agent already has, set on initialize.
    known: OnceLock<Vec<KnownServer>>,
}

impl Session {
    fn known(&self) -> &[KnownServer] {
        self.known.get().map_or(&[], Vec::as_slice)
    }

    fn instructions(&self) -> String {
        let connected = self
            .state
            .hosts
            .snapshot()
            .iter()
            .map(|(machine, host)| {
                let services: Vec<&str> = host
                    .services
                    .iter()
                    .map(|service| service.name.as_str())
                    .collect();

                format!("{machine} ({})", services.join(", "))
            })
            .collect::<Vec<_>>()
            .join("; ");
        let connected = if connected.is_empty() {
            "no machines"
        } else {
            &connected
        };

        format!(
            "Connected now: {connected}. Their tools are named <machine>__<service>__<tool>; call list_machines for the current state."
        )
    }

    async fn list_machines(&self) -> Result<CallToolResult, ErrorData> {
        let status = (self.state.read_status)()
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        let hosts = self.state.hosts.snapshot();
        let machines = status.machines(|name| {
            hosts
                .get(name)
                .map(|host| {
                    host.services
                        .iter()
                        .map(|service| service.name.clone())
                        .collect()
                })
                .unwrap_or_default()
        });
        let text = serde_json::to_string_pretty(&machines).expect("machines serialize to JSON");

        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        result.structured_content = Some(json!({ "machines": machines }));

        Ok(result)
    }
}

impl ServerHandler for Session {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new("tailnet", env!("CARGO_PKG_VERSION")))
        .with_instructions(self.instructions())
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let known = context
            .extensions
            .get::<Parts>()
            .and_then(|parts| parts.extensions.get::<Vec<KnownServer>>())
            .cloned()
            .unwrap_or_default();

        if self.known.set(known).is_err() {
            return Err(ErrorData::invalid_request(
                "the session is already initialized",
                None,
            ));
        }

        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        let catalogs = self.catalogs.clone();
        let known = self.known().to_vec();

        tokio::spawn(notify_tool_list_changes(
            context.peer,
            self.state.notify,
            move || visible_tools(&catalogs.current(), &known),
        ));
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = [list_machines_tool()]
            .into_iter()
            .chain(visible_tools(&self.catalogs.current(), self.known()))
            .collect();

        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if request.name == LIST_MACHINES {
            return self.list_machines().await.map(Into::into);
        }

        let catalog = self.catalogs.current();
        let Some(entry) = catalog
            .visible(self.known())
            .find(|entry| entry.tool.name == request.name)
        else {
            return Err(ErrorData::invalid_params(
                format!(
                    "Unknown tool {}; its machine may have disconnected",
                    request.name
                ),
                None,
            ));
        };

        match entry
            .caller
            .call(
                entry.service.clone(),
                entry.upstream_name.clone(),
                request.arguments,
            )
            .await
        {
            Ok(result) => Ok(result.into()),
            Err(CallError::Connector(message)) => Err(ErrorData::internal_error(message, None)),
            Err(CallError::Disconnected) => Err(ErrorData::internal_error(
                format!("connector for {} disconnected", entry.machine),
                None,
            )),
        }
    }
}

fn visible_tools(catalog: &Catalog, known: &[KnownServer]) -> Vec<Tool> {
    catalog
        .visible(known)
        .map(|entry| entry.tool.clone())
        .collect()
}

fn list_machines_tool() -> Tool {
    let input_schema = JsonObject::from_iter([
        ("type".to_owned(), json!("object")),
        ("properties".to_owned(), json!({})),
    ]);

    Tool::new(
        LIST_MACHINES,
        "List every machine on the Tailscale network with its online status, OS, addresses and \
         last-seen time, plus the MCP services its connector has registered. Those services are \
         exposed here as <machine>__<service>__<tool>.",
        input_schema,
    )
    .with_title("List tailnet machines")
    .with_annotations(ToolAnnotations::new().read_only(true).open_world(false))
}

/// Every `every`, tells the agent its tools changed if they differ from the ones it had when this
/// was called or was last told about, until its session ends. Every list_changed lands in the
/// agent's context, so changes are batched per window and a flap back to the previous tools within
/// one never notifies.
fn notify_tool_list_changes(
    peer: Peer<RoleServer>,
    every: Duration,
    tools: impl Fn() -> Vec<Tool>,
) -> impl Future<Output = ()> {
    let mut notified = tools();
    let mut ticks = interval_at(Instant::now() + every, every);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);

    async move {
        loop {
            ticks.tick().await;

            if peer.is_transport_closed() {
                return;
            }

            let current = tools();

            if current == notified {
                continue;
            }

            notified = current;

            if let Err(error) = peer.notify_tool_list_changed().await {
                eprintln!("list_changed notify failed: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rmcp::service::RunningService;
    use rmcp::{ClientHandler, RoleClient, ServiceExt};
    use tokio::sync::mpsc;
    use tokio::time::sleep;

    use super::*;
    use crate::hub::protocol::RegisteredService;
    use crate::hub_server::hosts::{Caller, Host, Hosts};

    const WINDOW: Duration = Duration::from_secs(300);

    struct Hub;

    impl ServerHandler for Hub {}

    struct Agent(mpsc::UnboundedSender<()>);

    impl ClientHandler for Agent {
        async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
            self.0.send(()).unwrap();
        }
    }

    /// An agent's session over an in-memory pipe, notified from `catalogs` as `known` sees it.
    struct Session {
        _hub: RunningService<RoleServer, Hub>,
        _agent: RunningService<RoleClient, Agent>,
        changes: mpsc::UnboundedReceiver<()>,
    }

    impl Session {
        async fn start(catalogs: &Catalogs, known: &[&str]) -> Self {
            let (hub_side, agent_side) = tokio::io::duplex(64 * 1024);
            let hub = tokio::spawn(Hub.serve(hub_side));
            let (changed, changes) = mpsc::unbounded_channel();
            let agent = Agent(changed).serve(agent_side).await.unwrap();
            let hub = hub.await.unwrap().unwrap();

            let catalogs = catalogs.clone();
            let known: Vec<KnownServer> = known
                .iter()
                .map(|name| KnownServer {
                    name: (*name).to_owned(),
                    url: None,
                })
                .collect();
            tokio::spawn(notify_tool_list_changes(
                hub.peer().clone(),
                WINDOW,
                move || visible_tools(&catalogs.current(), &known),
            ));

            Self {
                _hub: hub,
                _agent: agent,
                changes,
            }
        }

        fn notifications(&mut self) -> usize {
            std::iter::from_fn(|| self.changes.try_recv().ok()).count()
        }
    }

    fn host(service: &str) -> Host {
        let (caller, _calls) = Caller::channel();
        let services = vec![RegisteredService {
            name: service.to_owned(),
            tools: vec![Tool::new("run", "Runs", JsonObject::new())],
            url: None,
        }];

        Host { services, caller }
    }

    #[tokio::test(start_paused = true)]
    async fn notifies_each_session_once_per_window_only_when_its_own_tools_changed() {
        let hosts = Hosts::default();
        hosts.insert("laptop".to_owned(), host("echo"));
        let catalogs = Catalogs::new(&hosts);
        let mut sees_all = Session::start(&catalogs, &[]).await;
        let mut has_aside = Session::start(&catalogs, &["aside"]).await;
        // Puts each wait below just after a window ends, once its notifications have arrived.
        sleep(Duration::from_millis(1)).await;

        hosts.insert("desktop".to_owned(), host("aside"));
        hosts.insert("phone".to_owned(), host("aside"));
        sleep(WINDOW).await;

        assert_eq!(sees_all.notifications(), 1);
        assert_eq!(has_aside.notifications(), 0);

        let flapping = host("echo");
        let caller = flapping.caller.clone();
        hosts.insert("tablet".to_owned(), flapping);
        hosts.remove_if_same("tablet", &caller);
        sleep(WINDOW).await;

        assert_eq!(sees_all.notifications(), 0);
        assert_eq!(has_aside.notifications(), 0);

        hosts.insert("tablet".to_owned(), host("echo"));
        sleep(WINDOW).await;

        assert_eq!(sees_all.notifications(), 1);
        assert_eq!(has_aside.notifications(), 1);
    }
}
