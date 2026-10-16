use std::collections::BTreeSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rmcp::model::{CallToolResult, JsonObject, Tool};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::config::Config;
use crate::services::{Server, Services};

const FIRST_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_secs(60);

/// What the hub should see: the listed services and which of them are available right now.
pub struct Registration {
    pub(crate) services: Arc<Services>,
    pub(crate) available: BTreeSet<String>,
}

impl Registration {
    /// The server behind `name`, if it is listed and available.
    pub fn server(&self, name: &str) -> Option<&Arc<Server>> {
        self.services
            .get(name)
            .filter(|_| self.available.contains(name))
    }

    pub fn available(&self) -> impl Iterator<Item = (&str, &Server)> {
        self.available
            .iter()
            .filter_map(|name| Some((name.as_str(), self.services.get(name)?.as_ref())))
    }

    /// The available names joined for logs, or `when_empty` if there are none.
    pub fn names(&self, when_empty: &str) -> String {
        let names: Vec<&str> = self.available().map(|(name, _)| name).collect();

        if names.is_empty() {
            return when_empty.to_owned();
        }

        names.join(", ")
    }
}

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
pub async fn run(config: &Config, mut registration: watch::Receiver<Registration>) -> Infallible {
    let mut retry = FIRST_RETRY;

    loop {
        match connect_async(&config.connect_url).await {
            Ok((socket, _)) => {
                retry = FIRST_RETRY;

                if let Err(error) = serve(config, &mut registration, socket).await {
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
    registration: &mut watch::Receiver<Registration>,
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> Result<(), WsError> {
    let (mut sink, mut stream) = socket.split();
    let (register, names) = {
        let registration = registration.borrow_and_update();

        (
            register_message(&config.machine, &registration),
            registration.names("no services available"),
        )
    };
    sink.send(register).await?;
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
                            let HubMessage::Call { ref service, .. } = call;
                            let server = registration.borrow().server(service).cloned();
                            calls.spawn(handle_call(config.machine.clone(), server, call));
                        }
                        Err(error) => eprintln!("bad call from supervisor: {error}"),
                    }
                }
            },
            Ok(()) = registration.changed() => {
                let register = register_message(&config.machine, &registration.borrow_and_update());
                sink.send(register).await?;
            }
            Some(reply) = calls.join_next() => match reply {
                Ok(reply) => sink.send(reply).await?,
                Err(error) => eprintln!("call failed: {error}"),
            },
        }
    }
}

fn register_message(machine: &str, registration: &Registration) -> Message {
    encode(&ConnectorMessage::Register {
        machine,
        services: registration
            .available()
            .map(|(name, server)| RegisteredService {
                name,
                tools: server.tools(),
            })
            .collect(),
    })
}

async fn handle_call(machine: String, server: Option<Arc<Server>>, call: HubMessage) -> Message {
    let HubMessage::Call {
        id,
        service,
        tool,
        arguments,
    } = call;

    let reply = match server {
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
