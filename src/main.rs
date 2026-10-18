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
use tailnet_connector::install::{self, Role};
use tailnet_connector::tailscale::Status;
use tokio::net::TcpListener;
#[cfg(windows)]
use windows::Win32::Foundation::ERROR_INVALID_HANDLE;
#[cfg(windows)]
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

#[cfg(not(windows))]
const COMMANDS: &str = "run without arguments to connect this machine's MCP servers, with `agent` to serve an agent over stdio, with `hub` to run the hub, with `install --url <hub url>` or `install hub` to run the connector or the hub as a background service, or with `uninstall` or `uninstall hub` to remove that service";
#[cfg(windows)]
const COMMANDS: &str = "run without arguments to connect this machine's MCP servers, with `agent` to serve an agent over stdio, with `hub` to run the hub, with `install --url <hub url>` to run the connector as a background service, or with `uninstall` to remove that service";

#[cfg(not(windows))]
const INSTALL_USAGE: &str =
    "usage: tailnet-connector install --url <hub url> | install hub | uninstall | uninstall hub";
#[cfg(windows)]
const INSTALL_USAGE: &str = "usage: tailnet-connector install --url <hub url> | uninstall";

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Connector,
    Agent,
    Hub,
    InstallConnector {
        url: String,
    },
    #[cfg(not(windows))]
    InstallHub,
    Uninstall(Role),
}

fn parse(args: &[&str]) -> Result<Mode, String> {
    match args {
        [] => Ok(Mode::Connector),
        ["agent", ..] => Ok(Mode::Agent),
        ["hub", ..] => Ok(Mode::Hub),
        ["install", "--url", url] => Ok(Mode::InstallConnector {
            url: (*url).to_owned(),
        }),
        #[cfg(not(windows))]
        ["install", "hub"] => Ok(Mode::InstallHub),
        ["uninstall"] => Ok(Mode::Uninstall(Role::Connector)),
        #[cfg(not(windows))]
        ["uninstall", "hub"] => Ok(Mode::Uninstall(Role::Hub)),
        ["install" | "uninstall", ..] => Err(INSTALL_USAGE.to_owned()),
        [other, ..] => Err(format!("unknown command {other:?}: {COMMANDS}")),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let parsed = parse(&args);

    // Nothing can be reported without an output, so the exit code is all there is.
    #[cfg(windows)]
    if let Err(_) | Ok(Mode::InstallConnector { .. } | Mode::Uninstall(_)) = parsed
        && attach_console().is_err()
    {
        return ExitCode::FAILURE;
    }

    let mode = match parsed {
        Ok(mode) => mode,
        Err(usage) => {
            eprintln!("{usage}");

            return ExitCode::FAILURE;
        }
    };

    match &mode {
        Mode::Connector | Mode::Agent => {}
        Mode::Hub => return run_hub().await,
        Mode::InstallConnector { url } => return report(install::install_connector(url)),
        #[cfg(not(windows))]
        Mode::InstallHub => return report(install::install_hub()),
        Mode::Uninstall(role) => return report(install::uninstall(*role)),
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

/// Sends the output to the console of the terminal that ran the command, since a
/// windows-subsystem exe has none of its own. Without a parent console the output is lost.
#[cfg(windows)]
fn attach_console() -> windows::core::Result<()> {
    // SAFETY: called before anything is written to stdout or stderr.
    match unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } {
        Err(error) if error.code() == ERROR_INVALID_HANDLE.to_hresult() => Ok(()),
        result => result,
    }
}

fn report(result: Result<install::Outcome, install::InstallError>) -> ExitCode {
    match result {
        Ok(outcome) => {
            println!("{outcome}");

            ExitCode::SUCCESS
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_the_existing_roles() {
        assert_eq!(parse(&[]), Ok(Mode::Connector));
        assert_eq!(parse(&["agent"]), Ok(Mode::Agent));
        assert_eq!(parse(&["hub"]), Ok(Mode::Hub));
    }

    #[test]
    fn rejects_unknown_commands() {
        assert_eq!(
            parse(&["serve"]),
            Err(format!("unknown command \"serve\": {COMMANDS}"))
        );
    }

    #[test]
    fn parses_install_and_uninstall_of_the_connector() {
        assert_eq!(
            parse(&["install", "--url", "http://100.64.0.1:8790"]),
            Ok(Mode::InstallConnector {
                url: "http://100.64.0.1:8790".to_owned()
            })
        );
        assert_eq!(parse(&["uninstall"]), Ok(Mode::Uninstall(Role::Connector)));
    }

    #[cfg(not(windows))]
    #[test]
    fn parses_install_and_uninstall_of_the_hub() {
        assert_eq!(parse(&["install", "hub"]), Ok(Mode::InstallHub));
        assert_eq!(parse(&["uninstall", "hub"]), Ok(Mode::Uninstall(Role::Hub)));
    }

    #[cfg(windows)]
    #[test]
    fn rejects_install_and_uninstall_of_the_hub() {
        assert_eq!(parse(&["install", "hub"]), Err(INSTALL_USAGE.to_owned()));
        assert_eq!(parse(&["uninstall", "hub"]), Err(INSTALL_USAGE.to_owned()));
    }

    #[test]
    fn rejects_malformed_install_and_uninstall() {
        for args in [
            &["install"][..],
            &["install", "--url"],
            &["install", "http://100.64.0.1:8790"],
            &["install", "--url", "http://100.64.0.1:8790", "hub"],
            &["install", "hub", "--url", "http://100.64.0.1:8790"],
            &["uninstall", "connector"],
            &["uninstall", "hub", "now"],
        ] {
            assert_eq!(parse(args), Err(INSTALL_USAGE.to_owned()), "{args:?}");
        }
    }
}
