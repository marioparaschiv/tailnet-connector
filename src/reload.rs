use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::{self, ConfigError, McpServers};
use crate::pi_config;

/// How often the config file's modification time is checked, as Node's `watchFile` did.
pub const POLL: Duration = Duration::from_secs(5);

/// Tracks the servers to share: `mcpServers` in the Claude config, which Claude Code rewrites
/// constantly for unrelated state, and in Pi's `mcp.json`, whose entries replace Claude's of the
/// same name.
pub struct Watcher {
    claude_config: PathBuf,
    pi_config: PathBuf,
    modified: [Option<SystemTime>; 2],
    mcp_servers: McpServers,
}

impl Watcher {
    pub fn new(claude_config: PathBuf, pi_config: PathBuf) -> Result<Self, ConfigError> {
        let mut watcher = Self {
            claude_config,
            pi_config,
            modified: [None, None],
            mcp_servers: McpServers::new(),
        };
        watcher.modified = watcher.modified();
        watcher.mcp_servers = watcher.read()?;

        Ok(watcher)
    }

    pub fn mcp_servers(&self) -> &McpServers {
        &self.mcp_servers
    }

    /// Returns the new servers once either file was rewritten with a different set.
    pub fn changed(&mut self) -> Result<Option<&McpServers>, ConfigError> {
        let modified = self.modified();

        if modified == self.modified {
            return Ok(None);
        }

        self.modified = modified;
        let mcp_servers = self.read()?;

        if mcp_servers == self.mcp_servers {
            return Ok(None);
        }

        self.mcp_servers = mcp_servers;

        Ok(Some(&self.mcp_servers))
    }

    fn modified(&self) -> [Option<SystemTime>; 2] {
        [modified(&self.claude_config), modified(&self.pi_config)]
    }

    fn read(&self) -> Result<McpServers, ConfigError> {
        let pi = pi_config::merge(pi_config::read(&self.pi_config)?, McpServers::new());
        let mut servers = config::read_mcp_servers(&self.claude_config)?;
        servers.extend(pi_config::as_claude_entries(pi));

        Ok(servers)
    }
}

pub(crate) fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}
