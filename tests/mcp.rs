mod common;

use std::time::Duration;

use common::*;
use futures_util::FutureExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ErrorCode, Implementation,
    JsonObject, ProtocolVersion, RequestMetaObject, RequestParamsMeta, ResultType, Tool,
};
use rmcp::service::{RunningService, ServiceError};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use tailnet_connector::agent;
use tailnet_connector::hub::protocol::RegisteredService;
use tailnet_connector::hub_server::hosts::{Caller, Host, Hosts};
use tailnet_connector::hub_server::{self, HubState};
use tailnet_connector::tailscale::Status;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

type Agent = RunningService<RoleClient, ()>;

fn state(hosts: &Hosts) -> HubState {
    HubState {
        hosts: hosts.clone(),
        notify: Duration::from_secs(300),
        read_status: || std::future::ready(Ok(status())).boxed(),
    }
}

fn status() -> Status {
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
                "DNSName": "laptop.tail722322.ts.net.",
                "OS": "macOS",
                "TailscaleIPs": ["100.70.1.2"],
                "Online": true,
                "LastSeen": "2026-10-03T19:10:00.1Z",
            },
        },
    }))
    .unwrap()
}

/// Serves the hub and attaches `tailnet-connector agent` to it, with `mcp_servers` as the servers
/// the agent already has.
async fn start_agent(state: HubState, name: &str, mcp_servers: Value) -> Agent {
    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let project = temp_path(name);
    std::fs::create_dir_all(&project).unwrap();
    let claude_config = claude_config(&format!("{name}-claude.json"), mcp_servers);
    let config = config(&hub, &claude_config, &[]);
    tokio::spawn(hub_server::serve(hub, state));

    let (agent_side, client_side) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let (reader, writer) = tokio::io::split(agent_side);
        agent::run(&config, &project, reader, writer).await.unwrap();
    });

    ().serve(tokio::io::split(client_side)).await.unwrap()
}

fn service(name: &str, url: Option<&str>, tools: &[&str]) -> RegisteredService {
    RegisteredService {
        name: name.to_owned(),
        tools: tools
            .iter()
            .map(|tool| Tool::new(tool.to_string(), format!("Runs {tool}"), JsonObject::new()))
            .collect(),
        url: url.map(str::to_owned),
    }
}

/// Adds a host that answers each call with what it was asked, or fails calls to `fail`. Results
/// come off the wire without `resultType`, as from a connector whose server predates 2026-07-28.
fn add_host(hosts: &Hosts, machine: &str, services: Vec<RegisteredService>) {
    let (caller, mut calls) = Caller::channel();
    hosts.insert(machine.to_owned(), Host { services, caller });

    let machine = machine.to_owned();
    tokio::spawn(async move {
        while let Some(call) = calls.recv().await {
            let reply = match call.tool.as_str() {
                "fail" => Err(format!("{} cannot fail", call.service)),
                tool => Ok(serde_json::from_value(json!({
                    "content": [{
                        "type": "text",
                        "text": format!(
                            "{machine} {} {tool} {}",
                            call.service,
                            Value::from(call.arguments.unwrap_or_default())
                        ),
                    }],
                }))
                .unwrap()),
            };
            call.reply.send(reply).unwrap();
        }
    });
}

async fn tool_names(agent: &Agent) -> Vec<String> {
    agent
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect()
}

async fn call(agent: &Agent, name: &str, arguments: Value) -> Result<CallToolResult, ServiceError> {
    let request = CallToolRequestParams::new(name.to_owned())
        .with_arguments(arguments.as_object().cloned().unwrap());

    agent.call_tool(request).await
}

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

fn error(result: Result<CallToolResult, ServiceError>) -> (ErrorCode, String) {
    match result {
        Err(ServiceError::McpError(error)) => (error.code, error.message.into_owned()),
        other => panic!("expected an MCP error, got {other:?}"),
    }
}

#[tokio::test]
async fn lists_and_calls_every_host_tool_through_the_agent() {
    let hosts = Hosts::default();
    add_host(
        &hosts,
        "laptop",
        vec![service("echo", None, &["echo", "fail", "not valid"])],
    );
    add_host(
        &hosts,
        "desktop",
        vec![service("ghidra", None, &["decompile"])],
    );
    let (caller, calls) = Caller::channel();
    drop(calls);
    let services = vec![service("aside", None, &["exec"])];
    hosts.insert("phone".to_owned(), Host { services, caller });
    let agent = start_agent(state(&hosts), "mcp-lists", json!({})).await;

    assert_eq!(
        agent.peer_info().unwrap().instructions.as_deref(),
        Some(
            "Connected now: desktop (ghidra); laptop (echo); phone (aside). Their tools are named <machine>__<service>__<tool>; call list_machines for the current state."
        )
    );
    let tools = agent.list_all_tools().await.unwrap();
    let names: Vec<_> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(
        names,
        [
            "list_machines",
            "desktop__ghidra__decompile",
            "laptop__echo__echo",
            "laptop__echo__fail",
            "phone__aside__exec",
        ]
    );
    assert_eq!(
        tools[2].description.as_deref(),
        Some("[laptop / echo] Runs echo")
    );

    let result = call(&agent, "laptop__echo__echo", json!({ "text": "hi" }))
        .await
        .unwrap();
    assert_eq!(text(&result), r#"laptop echo echo {"text":"hi"}"#);

    assert_eq!(
        error(call(&agent, "laptop__echo__fail", json!({})).await),
        (ErrorCode::INTERNAL_ERROR, "echo cannot fail".to_owned())
    );
    assert_eq!(
        error(call(&agent, "phone__aside__exec", json!({})).await),
        (
            ErrorCode::INTERNAL_ERROR,
            "connector for phone disconnected".to_owned()
        )
    );
    assert_eq!(
        error(call(&agent, "tablet__echo__echo", json!({})).await),
        (
            ErrorCode::INVALID_PARAMS,
            "Unknown tool tablet__echo__echo; its machine may have disconnected".to_owned()
        )
    );
}

#[tokio::test]
async fn marks_relayed_results_complete_for_2026_07_28_agents() {
    let hosts = Hosts::default();
    add_host(&hosts, "laptop", vec![service("echo", None, &["echo"])]);
    let agent = start_agent(state(&hosts), "mcp-result-type", json!({})).await;
    // Claude Code skips initialize and names its protocol version on every request.
    let mut request = CallToolRequestParams::new("laptop__echo__echo");
    request.set_meta(RequestMetaObject::with_client_context(
        ProtocolVersion::V_2026_07_28,
        Implementation::new("claude-code", "1"),
        ClientCapabilities::default(),
    ));

    let result = agent.call_tool(request).await.unwrap();

    assert_eq!(result.result_type, Some(ResultType::COMPLETE));
}

#[tokio::test]
async fn says_when_no_machines_are_connected() {
    let agent = start_agent(state(&Hosts::default()), "mcp-empty", json!({})).await;

    assert_eq!(
        agent.peer_info().unwrap().instructions.as_deref(),
        Some(
            "Connected now: no machines. Their tools are named <machine>__<service>__<tool>; call list_machines for the current state."
        )
    );
    assert_eq!(tool_names(&agent).await, ["list_machines"]);
}

#[tokio::test]
async fn hides_the_hub_copies_of_servers_the_agent_already_has() {
    let hosts = Hosts::default();
    add_host(
        &hosts,
        "laptop",
        vec![
            service("aside", None, &["exec"]),
            service("linear", Some("https://mcp.linear.app/mcp"), &["search"]),
            service("github", Some("https://github.example/mcp"), &["issues"]),
            service("echo", None, &["echo"]),
        ],
    );
    let agent = start_agent(
        state(&hosts),
        "mcp-known",
        json!({
            "aside": { "command": "aside", "args": ["mcp"] },
            "my-linear": { "type": "http", "url": "https://MCP.linear.app:443/mcp" },
            "github": { "type": "http", "url": "https://other.example/mcp" },
        }),
    )
    .await;

    assert_eq!(
        tool_names(&agent).await,
        [
            "list_machines",
            "laptop__github__issues",
            "laptop__echo__echo"
        ]
    );
    assert_eq!(
        error(call(&agent, "laptop__aside__exec", json!({})).await).0,
        ErrorCode::INVALID_PARAMS
    );
}

#[tokio::test]
async fn lists_machines_with_their_registered_services() {
    let hosts = Hosts::default();
    add_host(
        &hosts,
        "laptop",
        vec![service("echo", None, &[]), service("aside", None, &[])],
    );
    let agent = start_agent(state(&hosts), "mcp-machines", json!({})).await;

    let tools = agent.list_all_tools().await.unwrap();
    assert_eq!(tools[0].title.as_deref(), Some("List tailnet machines"));
    let annotations = tools[0].annotations.as_ref().unwrap();
    assert_eq!(
        (annotations.read_only_hint, annotations.open_world_hint),
        (Some(true), Some(false))
    );

    let result = call(&agent, "list_machines", json!({})).await.unwrap();
    let machines: Value = serde_json::from_str(text(&result)).unwrap();

    assert_eq!(
        result.structured_content,
        Some(json!({ "machines": machines }))
    );
    assert_eq!(
        machines,
        json!([
            {
                "name": "laptop",
                "hostName": "Marios-MacBook-Pro",
                "dnsName": "laptop.tail722322.ts.net",
                "os": "macOS",
                "ips": ["100.70.1.2"],
                "self": false,
                "online": true,
                "lastSeen": "2026-10-03T19:10:00.1Z",
                "services": ["echo", "aside"],
            },
            {
                "name": "vps",
                "hostName": "vps",
                "dnsName": "vps.tail722322.ts.net",
                "os": "linux",
                "ips": ["100.65.233.54"],
                "self": true,
                "online": true,
                "lastSeen": null,
                "services": [],
            },
        ])
    );
}

/// The status line and body of an initialize POST carrying `known_servers` as the header.
async fn initialize(port: u16, known_servers: Option<&str>) -> (String, String) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" },
        },
    })
    .to_string();
    let header = known_servers
        .map(|value| format!("x-tailnet-known-servers: {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{header}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();

    (head.lines().next().unwrap().to_owned(), body.to_owned())
}

#[tokio::test]
async fn answers_an_invalid_known_servers_header_with_400_and_the_reason() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(hub_server::serve(listener, state(&Hosts::default())));

    for (header, reason) in [
        (
            "not json",
            "not a list of servers: expected ident at line 1 column 2",
        ),
        (r#"[{"name":""}]"#, "a server has an empty name"),
        (
            r#"[{"name":"linear","url":"mcp.linear.app"}]"#,
            "linear: mcp.linear.app is not a URL: relative URL without a base",
        ),
    ] {
        assert_eq!(
            initialize(port, Some(header)).await,
            (
                "HTTP/1.1 400 Bad Request".to_owned(),
                format!("Invalid x-tailnet-known-servers: {reason}")
            ),
            "{header}"
        );
    }

    for header in [None, Some(r#"[{"name":"aside"}]"#)] {
        assert_eq!(
            initialize(port, header).await.0,
            "HTTP/1.1 200 OK",
            "{header:?}"
        );
    }
}
