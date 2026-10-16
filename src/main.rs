// No console window on Windows; TAILNET_LOG receives the output instead.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

use tailnet_connector::config::Config;
use tailnet_connector::connector::Connector;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    // Nothing can be reported without an output yet, so the exit code is all there is.
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("TAILNET_LOG")
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

    match Connector::start(config).await {
        Ok(connector) => match connector.run().await {},
        Err(error) => {
            eprintln!("{error}");

            ExitCode::FAILURE
        }
    }
}
