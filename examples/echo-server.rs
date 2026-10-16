//! Stdio MCP server used as a test fixture, mirroring tailnet-mcp's echo-server.ts.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};

#[derive(Clone)]
struct Echo;

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
struct EchoRequest {
    text: String,
}

#[tool_router]
impl Echo {
    #[tool(description = "Echo text back")]
    fn echo(&self, Parameters(EchoRequest { text }): Parameters<EchoRequest>) -> String {
        format!("echo from pid {}: {text}", std::process::id())
    }

    #[tool(description = "Never return")]
    async fn hang(&self) -> String {
        std::future::pending().await
    }
}

#[tool_handler]
impl ServerHandler for Echo {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = Echo.serve(rmcp::transport::stdio()).await?;
    server.waiting().await?;

    Ok(())
}
