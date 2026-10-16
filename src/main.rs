// No console window on Windows; TAILNET_LOG receives the output instead.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

use futures_util::FutureExt;
use tailnet_connector::agent;
use tailnet_connector::config::Config;
use tailnet_connector::connector::Connector;
use tailnet_connector::hub_server::config::HubConfig;
use tailnet_connector::hub_server::hosts::Hosts;
use tailnet_connector::hub_server::{self, HubState};
use tailnet_connector::tailscale::Status;
use tokio::net::TcpListener;

enum Mode {
    Connector,
    Agent,
    Hub,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mode = match std::env::args().nth(1).as_deref() {
        None => Mode::Connector,
        Some("agent") => Mode::Agent,
        Some("hub") => Mode::Hub,
        Some(other) => {
            eprintln!(
                "unknown command {other:?}: run without arguments to connect this machine's MCP servers, with `agent` to serve an agent over stdio, or with `hub` to run the hub"
            );

            return ExitCode::FAILURE;
        }
    };

    if let Mode::Hub = mode {
        return run_hub().await;
    }

    // Nothing can be reported without an output yet, so the exit code is all there is. An agent
    // passes its own stdio, which must stay as it is.
    #[cfg(windows)]
    if let Mode::Connector = mode
        && let Some(path) = std::env::var_os("TAILNET_LOG")
        && tailnet_connector::log_file::append_output_to(std::path::Path::new(&path)).is_err()
    {
        return ExitCode::FAILURE;
    }

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");

            return ExitCode::FAILURE;
        }
    };

    if let Mode::Agent = mode {
        return run_agent(&config).await;
    }

    match Connector::start(config).await {
        Ok(connector) => match connector.run().await {},
        Err(error) => {
            eprintln!("{error}");

            ExitCode::FAILURE
        }
    }
}

async fn run_agent(config: &Config) -> ExitCode {
    // Claude Code starts an agent's stdio servers in the agent's project directory.
    let project = match std::env::current_dir() {
        Ok(project) => project,
        Err(error) => {
            eprintln!("cannot read the working directory: {error}");

            return ExitCode::FAILURE;
        }
    };
    let (stdin, stdout) = rmcp::transport::stdio();

    match agent::run(config, &project, stdin, stdout).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");

            ExitCode::FAILURE
        }
    }
}

async fn run_hub() -> ExitCode {
    let config = match HubConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");

            return ExitCode::FAILURE;
        }
    };
    let address = format!("{}:{}", config.host, config.port);
    let listener = match TcpListener::bind((config.host.as_str(), config.port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot listen on {address}: {error}");

            return ExitCode::FAILURE;
        }
    };
    println!("tailnet MCP listening on http://{address}/mcp");
    println!("connectors attach at ws://{address}/connect");

    let state = HubState {
        hosts: Hosts::default(),
        notify: config.notify,
        read_status: || Status::read().boxed(),
    };

    match hub_server::serve(listener, state).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");

            ExitCode::FAILURE
        }
    }
}
