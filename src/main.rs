// No console window on Windows; TAILNET_LOG receives the output instead.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

use tailnet_connector::agent;
use tailnet_connector::config::Config;
use tailnet_connector::connector::Connector;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let agent_mode = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("agent") => true,
        Some(other) => {
            eprintln!(
                "unknown command {other:?}: run without arguments to connect this machine's MCP servers, or with `agent` to serve an agent over stdio"
            );

            return ExitCode::FAILURE;
        }
    };

    // Nothing can be reported without an output yet, so the exit code is all there is. An agent
    // passes its own stdio, which must stay as it is.
    #[cfg(windows)]
    if !agent_mode
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

    if agent_mode {
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
