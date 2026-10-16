use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_tungstenite::tungstenite::http::Uri;

use crate::config::{self, Config, ConfigError, McpServers};
use crate::reload;

/// An MCP server an agent already has, which the hub then leaves out of that agent's tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KnownServer {
    pub name: String,
    /// Set for http servers, which the hub matches by URL; stdio servers match by name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Tracks the MCP servers Claude Code gives an agent working in `project`.
pub struct KnownServers {
    claude_config: PathBuf,
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
        let mut known = Self {
            claude_config: config.claude_config.clone(),
            project_file: project.join(".mcp.json"),
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
            reload::modified(&self.claude_config),
            reload::modified(&self.project_file),
        ]
    }

    fn load(&self) -> Result<Vec<KnownServer>, ConfigError> {
        let mut claude_config = config::read_json::<ClaudeConfig>(&self.claude_config)?;
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
        let mut servers = BTreeMap::new();
        servers.extend(claude_config.mcp_servers);
        servers.extend(shared);
        servers.extend(local);

        Ok(servers
            .into_iter()
            .filter_map(|(name, entry)| self.known_server(name, entry))
            .collect())
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
