use std::process::ExitCode;
use std::sync::Arc;

use tailnet_connector::config::{self, Config, ConfigError, McpServers};
use tailnet_connector::hub;
use tailnet_connector::services::Services;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let (config, mcp_servers) = match load() {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("{error}");

            return ExitCode::FAILURE;
        }
    };

    let services = Arc::new(Services::start(mcp_servers).await);

    match hub::run(&config, services).await {}
}

fn load() -> Result<(Config, McpServers), ConfigError> {
    let config = Config::from_env()?;
    let mcp_servers = config::read_mcp_servers(&config.claude_config)?;

    Ok((config, mcp_servers))
}
