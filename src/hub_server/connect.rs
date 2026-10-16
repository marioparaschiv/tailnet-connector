//! `/connect`: connectors register their services here and answer the calls routed to them.

use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rmcp::model::CallToolResult;
use tokio::sync::oneshot;
use tokio::time::{Instant, interval_at};
use url::Url;

use crate::hub::protocol::{ConnectorMessage, HubMessage};
use crate::hub_server::HubState;
use crate::hub_server::hosts::{Caller, Host, Hosts};

const HEARTBEAT: Duration = Duration::from_secs(30);

#[derive(Debug)]
enum InvalidMessage {
    Json(serde_json::Error),
    EmptyMachine,
    EmptyServiceName,
    Url { url: String, error: url::ParseError },
}

impl fmt::Display for InvalidMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(f, "{error}"),
            Self::EmptyMachine => f.write_str("machine is empty"),
            Self::EmptyServiceName => f.write_str("a service name is empty"),
            Self::Url { url, error } => write!(f, "service url {url:?} is invalid: {error}"),
        }
    }
}

pub(super) async fn handle(
    upgrade: WebSocketUpgrade,
    State(state): State<HubState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    let status = match (state.read_status)().await {
        Ok(status) => status,
        Err(error) => {
            eprintln!("cannot read tailscale status for connector upgrade: {error}");

            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    // Preferred over the name the connector reports, so tools line up with list_machines.
    let tailnet_name = status.machine_name(peer.ip()).map(str::to_owned);

    upgrade.on_upgrade(move |socket| attach(socket, state.hosts, tailnet_name))
}

async fn attach(mut socket: WebSocket, hosts: Hosts, tailnet_name: Option<String>) {
    let (caller, mut calls) = Caller::channel();
    let mut pending = HashMap::<u64, oneshot::Sender<Result<CallToolResult, String>>>::new();
    let mut next_id = 1;
    let mut machine: Option<String> = None;
    // A laptop going to sleep never closes its socket, so dead peers are found by missed pongs.
    let mut heartbeat = interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
    let mut alive = true;

    loop {
        tokio::select! {
            message = socket.recv() => {
                let parsed = match message {
                    Some(Ok(Message::Text(text))) => parse(text.as_str().as_bytes()),
                    Some(Ok(Message::Binary(data))) => parse(&data),
                    Some(Ok(Message::Pong(_))) => {
                        alive = true;
                        continue;
                    }
                    Some(Ok(Message::Ping(_))) => continue,
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                };

                let message = match parsed {
                    Ok(message) => message,
                    Err(error) => {
                        let from = machine.as_deref().unwrap_or("unregistered connector");
                        eprintln!("invalid message from {from}: {error}");
                        let close = CloseFrame {
                            code: close_code::POLICY,
                            reason: "invalid message".into(),
                        };
                        // Closing anyway, so a peer that is already gone changes nothing.
                        let _ = socket.send(Message::Close(Some(close))).await;
                        break;
                    }
                };

                match message {
                    ConnectorMessage::Register { machine: reported, services } => {
                        let name = tailnet_name.clone().unwrap_or(reported);

                        if let Some(old) = machine.as_deref().filter(|old| *old != name) {
                            hosts.remove_if_same(old, &caller);
                        }

                        let host = Host {
                            services,
                            caller: caller.clone(),
                        };
                        hosts.insert(name.clone(), host);
                        machine = Some(name);
                    }
                    // A caller that stopped waiting has nothing to be told.
                    ConnectorMessage::Result { id, result } => {
                        if let Some(reply) = pending.remove(&id) {
                            let _ = reply.send(Ok(result));
                        }
                    }
                    ConnectorMessage::Error { id, message } => {
                        if let Some(reply) = pending.remove(&id) {
                            let _ = reply.send(Err(message));
                        }
                    }
                }
            }
            Some(call) = calls.recv() => {
                let id = next_id;
                next_id += 1;
                let message = HubMessage::Call {
                    id,
                    service: call.service,
                    tool: call.tool,
                    arguments: call.arguments,
                };
                let text =
                    serde_json::to_string(&message).expect("hub messages serialize to JSON");

                pending.insert(id, call.reply);

                if socket.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                if !alive {
                    break;
                }

                alive = false;

                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    break;
                }
            }
        }
    }

    // Dropping the replies fails every pending call. The socket outlives this cleanup, so a peer
    // that sees it close already finds its host gone.
    drop(pending);

    // A reconnect under the same name may already have replaced this host.
    if let Some(machine) = machine {
        hosts.remove_if_same(&machine, &caller);
    }
}

fn parse(data: &[u8]) -> Result<ConnectorMessage, InvalidMessage> {
    let message = serde_json::from_slice(data).map_err(InvalidMessage::Json)?;

    let ConnectorMessage::Register { machine, services } = &message else {
        return Ok(message);
    };

    if machine.is_empty() {
        return Err(InvalidMessage::EmptyMachine);
    }

    for service in services {
        if service.name.is_empty() {
            return Err(InvalidMessage::EmptyServiceName);
        }

        if let Some(url) = &service.url {
            Url::parse(url).map_err(|error| InvalidMessage::Url {
                url: url.clone(),
                error,
            })?;
        }
    }

    Ok(message)
}
