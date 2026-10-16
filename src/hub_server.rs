//! `hub` mode: one port serving agents on `/mcp` and connectors on `/connect`.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::any;
use futures_util::future::BoxFuture;
use tokio::net::TcpListener;

use crate::hub_server::hosts::Hosts;
use crate::tailscale::{Status, StatusError};

pub mod config;
mod connect;
pub mod hosts;
mod mcp;

/// What every handler shares.
#[derive(Clone)]
pub struct HubState {
    pub hosts: Hosts,
    /// How often each agent session is told if its tools changed.
    pub notify: Duration,
    /// Names each connector at its upgrade; `Status::read` outside tests.
    pub read_status: fn() -> BoxFuture<'static, Result<Status, StatusError>>,
}

/// `/connect` and `/mcp`; any other path is a 404.
pub fn router(state: HubState) -> Router {
    Router::new()
        .route("/connect", any(connect::handle))
        .route("/mcp", any(mcp::handle))
        .with_state(state)
}

/// Serves `router` on `listener`. Handlers can extract `ConnectInfo<SocketAddr>` for the peer.
pub async fn serve(listener: TcpListener, state: HubState) -> io::Result<()> {
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}
