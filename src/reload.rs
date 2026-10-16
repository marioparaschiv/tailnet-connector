use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::{self, ConfigError, McpServers};

/// How often the config file's modification time is checked, as Node's `watchFile` did.
pub const POLL: Duration = Duration::from_secs(5);

/// Tracks `mcpServers` in the Claude config, which Claude Code rewrites constantly for
/// unrelated state.
pub struct Watcher {
    path: PathBuf,
    modified: Option<SystemTime>,
    mcp_servers: McpServers,
}

impl Watcher {
    pub fn new(path: PathBuf) -> Result<Self, ConfigError> {
        let modified = modified(&path);
        let mcp_servers = config::read_mcp_servers(&path)?;

        Ok(Self {
            path,
            modified,
            mcp_servers,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn mcp_servers(&self) -> &McpServers {
        &self.mcp_servers
    }

    /// Returns the new `mcpServers` once the file was rewritten with a different value.
    pub fn changed(&mut self) -> Result<Option<&McpServers>, ConfigError> {
        let modified = modified(&self.path);

        if modified == self.modified {
            return Ok(None);
        }

        self.modified = modified;
        let mcp_servers = config::read_mcp_servers(&self.path)?;

        if mcp_servers == self.mcp_servers {
            return Ok(None);
        }

        self.mcp_servers = mcp_servers;

        Ok(Some(&self.mcp_servers))
    }
}

pub(crate) fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}
