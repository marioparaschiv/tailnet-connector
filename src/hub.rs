use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::config::Config;
use crate::services::Services;

const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(60);

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ConnectorMessage<'a> {
    Register {
        machine: &'a str,
        services: Vec<RegisteredService<'a>>,
    },
    Result {
        id: u64,
        result: CallToolResult,
    },
    Error {
        id: u64,
        message: String,
    },
}

#[derive(Serialize)]
struct RegisteredService<'a> {
    name: &'a str,
    tools: &'a [Tool],
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum HubMessage {
    Call {
        id: u64,
        service: String,
        tool: String,
        arguments: Option<JsonObject>,
    },
}

/// Keeps a registered connection to the hub open forever, reconnecting with exponential backoff.
pub async fn run(config: &Config, services: Arc<Services>) -> Infallible {
    let mut retry = FIRST_RETRY;

    loop {
        match connect_async(&config.connect_url).await {
            Ok((socket, _)) => {
                retry = FIRST_RETRY;

                if let Err(error) = serve(config, &services, socket).await {
                    eprintln!("connection to {} failed: {error}", config.url);
                }
            }
            Err(error) => eprintln!("cannot connect to {}: {error}", config.connect_url),
        }

        eprintln!("supervisor unreachable, retrying in {}s", retry.as_secs());
        tokio::time::sleep(retry).await;
        retry = (retry * 2).min(MAX_RETRY);
    }
}

async fn serve(
    config: &Config,
    services: &Arc<Services>,
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> Result<(), WsError> {
    let (mut sink, mut stream) = socket.split();
    let register = ConnectorMessage::Register {
        machine: &config.machine,
        services: services
            .iter()
            .map(|(name, server)| RegisteredService {
                name,
                tools: server.tools(),
            })
            .collect(),
    };
    sink.send(encode(&register)).await?;

    let names: Vec<&str> = services.iter().map(|(name, _)| name).collect();
    let names = if names.is_empty() {
        "no services available".to_owned()
    } else {
        names.join(", ")
    };
    println!("connected to {} as {}: {names}", config.url, config.machine);

    // Calls run as separate tasks so a slow tool never holds up the others.
    let mut calls = JoinSet::new();

    loop {
        tokio::select! {
            message = stream.next() => match message {
                None => return Ok(()),
                Some(message) => {
                    let Message::Text(text) = message? else {
                        continue;
                    };

                    match serde_json::from_str::<HubMessage>(&text) {
                        Ok(call) => {
                            calls.spawn(handle_call(config.machine.clone(), Arc::clone(services), call));
                        }
                        Err(error) => eprintln!("bad call from supervisor: {error}"),
                    }
                }
            },
            Some(reply) = calls.join_next() => match reply {
                Ok(reply) => sink.send(reply).await?,
                Err(error) => eprintln!("call failed: {error}"),
            },
        }
    }
}

async fn handle_call(machine: String, services: Arc<Services>, call: HubMessage) -> Message {
    let HubMessage::Call {
        id,
        service,
        tool,
        arguments,
    } = call;

    let reply = match services.get(&service) {
        None => ConnectorMessage::Error {
            id,
            message: format!("{service} is not available on {machine}"),
        },
        Some(server) => match server.call_tool(tool, arguments).await {
            Ok(result) => ConnectorMessage::Result { id, result },
            Err(error) => ConnectorMessage::Error {
                id,
                message: error.to_string(),
            },
        },
    };

    encode(&reply)
}

fn encode(message: &ConnectorMessage) -> Message {
    Message::text(serde_json::to_string(message).expect("connector messages serialize to JSON"))
}
