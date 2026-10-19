use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_tungstenite::tungstenite::http::{HeaderName, Uri};

use crate::config::{self, Config, ConfigError, Harness, McpServers};
use crate::{pi_config, reload};

/// Carries an agent's known servers to the hub as a JSON array.
pub const KNOWN_SERVERS_HEADER: HeaderName = HeaderName::from_static("x-tailnet-known-servers");

/// An MCP server an agent already has, which the hub then leaves out of that agent's tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownServer {
    pub name: String,
    /// Set for http servers, which the hub matches by URL; stdio servers match by name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Tracks the MCP servers the harness gives an agent working in `project`.
pub struct KnownServers {
    harness: Harness,
    /// The harness's user config: the Claude config, or Pi's `mcp.json`.
    user_file: PathBuf,
    project_file: PathBuf,
    project: String,
    hub_host: String,
    modified: [Option<SystemTime>; 2],
    servers: Vec<KnownServer>,
}

#[derive(Deserialize)]
struct Scope {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: McpServers,
}

#[derive(Deserialize)]
struct ClaudeConfig {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: McpServers,
    #[serde(default)]
    projects: BTreeMap<String, Scope>,
}

#[derive(Deserialize)]
struct Entry {
    url: Option<String>,
    command: Option<String>,
}

impl KnownServers {
    pub fn read(config: &Config, project: &Path) -> Result<Self, ConfigError> {
        let (user_file, project_file) = match config.harness {
            Harness::ClaudeCode => (config.claude_config.clone(), project.join(".mcp.json")),
            Harness::Pi => (
                config.pi_config.clone(),
                project.join(".pi").join("mcp.json"),
            ),
        };
        let mut known = Self {
            harness: config.harness,
            user_file,
            project_file,
            project: project.to_string_lossy().into_owned(),
            hub_host: config.hub_host.clone(),
            modified: [None, None],
            servers: Vec::new(),
        };
        known.modified = known.modified();
        known.servers = known.load()?;

        Ok(known)
    }

    pub fn servers(&self) -> &[KnownServer] {
        &self.servers
    }

    /// Returns the new servers once either config file was rewritten with a different set.
    pub fn changed(&mut self) -> Result<Option<&[KnownServer]>, ConfigError> {
        let modified = self.modified();

        if modified == self.modified {
            return Ok(None);
        }

        self.modified = modified;
        let servers = self.load()?;

        if servers == self.servers {
            return Ok(None);
        }

        self.servers = servers;

        Ok(Some(&self.servers))
    }

    fn modified(&self) -> [Option<SystemTime>; 2] {
        [
            reload::modified(&self.user_file),
            reload::modified(&self.project_file),
        ]
    }

    fn load(&self) -> Result<Vec<KnownServer>, ConfigError> {
        let servers = match self.harness {
            Harness::ClaudeCode => self.claude_code_servers()?,
            Harness::Pi => pi_config::merge(
                pi_config::read(&self.user_file)?,
                pi_config::read(&self.project_file)?,
            ),
        };

        Ok(servers
            .into_iter()
            .filter_map(|(name, entry)| self.known_server(name, entry))
            .collect())
    }

    fn claude_code_servers(&self) -> Result<McpServers, ConfigError> {
        let mut claude_config = config::read_json::<ClaudeConfig>(&self.user_file)?;
        let shared = if self.project_file.exists() {
            config::read_json::<Scope>(&self.project_file)?.mcp_servers
        } else {
            McpServers::new()
        };
        let local = claude_config
            .projects
            .remove(&self.project)
            .map(|scope| scope.mcp_servers)
            .unwrap_or_default();

        // Claude Code's precedence: a local entry replaces a project one, which replaces a user one.
        let mut servers = claude_config.mcp_servers;
        servers.extend(shared);
        servers.extend(local);

        Ok(servers)
    }

    fn known_server(&self, name: String, entry: Value) -> Option<KnownServer> {
        let entry = match Entry::deserialize(entry) {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!("skipping {name}: {error}");

                return None;
            }
        };

        match (entry.url, entry.command) {
            (Some(url), _) => {
                let uri = match url.parse::<Uri>() {
                    Ok(uri) if uri.authority().is_some() => uri,
                    // An unexpanded ${VAR} lands here; the hub would reject the whole list over it.
                    _ => {
                        eprintln!("skipping {name}: {url} is not an absolute URL");

                        return None;
                    }
                };

                (config::host(&uri) != self.hub_host).then_some(KnownServer {
                    name,
                    url: Some(url),
                })
            }
            (None, Some(command)) => {
                (!config::runs_this_program(&command)).then_some(KnownServer { name, url: None })
            }
            (None, None) => {
                eprintln!("skipping {name}: neither a url nor a command");

                None
            }
        }
    }
}
