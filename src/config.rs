use std::collections::BTreeMap;
use std::env::{self, VarError};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde::de::IgnoredAny;
use tokio_tungstenite::tungstenite::http::Uri;

#[derive(Debug, Clone)]
pub struct Config {
    /// TAILNET_URL as given, for logs.
    pub url: String,
    /// The hub's WebSocket endpoint derived from `url`.
    pub connect_url: String,
    pub machine: String,
    pub idle: Duration,
    pub claude_config: PathBuf,
    pub checks: PathBuf,
    pub check_interval: Duration,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerConfig {
    Stdio { command: String },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ServerEntry {
    Supported(ServerConfig),
    Unsupported(IgnoredAny),
}

pub type McpServers = BTreeMap<String, ServerEntry>;

#[derive(Debug)]
pub enum ConfigError {
    Missing(&'static str),
    NotUnicode(&'static str),
    Invalid {
        name: &'static str,
        value: String,
        reason: &'static str,
    },
    NoHomeDir(&'static str),
    ReadClaudeConfig {
        path: PathBuf,
        source: std::io::Error,
    },
    ParseClaudeConfig {
        path: PathBuf,
        source: serde_json::Error,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(name) => write!(f, "{name} is required"),
            Self::NotUnicode(name) => write!(f, "{name} is not valid unicode"),
            Self::Invalid {
                name,
                value,
                reason,
            } => write!(f, "{name}={value:?} is invalid: {reason}"),
            Self::NoHomeDir(name) => {
                write!(f, "{name} is not set and the home directory is unknown")
            }
            Self::ReadClaudeConfig { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            Self::ParseClaudeConfig { path, source } => {
                write!(f, "cannot parse {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadClaudeConfig { source, .. } => Some(source),
            Self::ParseClaudeConfig { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| env::var(name))
    }

    pub fn from_lookup(
        lookup: impl Fn(&str) -> Result<String, VarError>,
    ) -> Result<Self, ConfigError> {
        let var = |name: &'static str| match lookup(name) {
            Ok(value) => Ok(Some(value)),
            Err(VarError::NotPresent) => Ok(None),
            Err(VarError::NotUnicode(_)) => Err(ConfigError::NotUnicode(name)),
        };

        let url = var("TAILNET_URL")?.ok_or(ConfigError::Missing("TAILNET_URL"))?;
        let connect_url = connect_url(&url)?;

        let machine = match var("TAILNET_MACHINE")? {
            Some(machine) if machine.is_empty() => {
                return Err(invalid("TAILNET_MACHINE", machine, "must not be empty"));
            }
            Some(machine) => machine,
            None => default_machine(),
        };

        Ok(Self {
            url,
            connect_url,
            machine,
            idle: seconds("TAILNET_IDLE_SECONDS", var("TAILNET_IDLE_SECONDS")?, 300)?,
            claude_config: path_or_home("CLAUDE_CONFIG", var("CLAUDE_CONFIG")?, &[".claude.json"])?,
            checks: path_or_home(
                "TAILNET_CHECKS",
                var("TAILNET_CHECKS")?,
                &[".config", "tailnet", "checks.json"],
            )?,
            check_interval: seconds("TAILNET_CHECK_SECONDS", var("TAILNET_CHECK_SECONDS")?, 10)?,
        })
    }
}

pub fn read_mcp_servers(path: &Path) -> Result<McpServers, ConfigError> {
    #[derive(Deserialize)]
    struct ClaudeConfig {
        #[serde(default, rename = "mcpServers")]
        mcp_servers: McpServers,
    }

    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::ReadClaudeConfig {
        path: path.to_owned(),
        source,
    })?;
    let config: ClaudeConfig =
        serde_json::from_str(&text).map_err(|source| ConfigError::ParseClaudeConfig {
            path: path.to_owned(),
            source,
        })?;

    Ok(config.mcp_servers)
}

fn invalid(name: &'static str, value: String, reason: &'static str) -> ConfigError {
    ConfigError::Invalid {
        name,
        value,
        reason,
    }
}

// TLS is not compiled in, so https/wss hubs are rejected here rather than failing on every connect.
fn connect_url(url: &str) -> Result<String, ConfigError> {
    let not_a_url = || {
        invalid(
            "TAILNET_URL",
            url.to_owned(),
            "must be an http:// or ws:// URL",
        )
    };
    let uri: Uri = url.parse().map_err(|_| not_a_url())?;
    let authority = uri.authority().ok_or_else(not_a_url)?;

    match uri.scheme_str() {
        Some("http" | "ws") => Ok(format!("ws://{authority}/connect")),
        _ => Err(not_a_url()),
    }
}

fn default_machine() -> String {
    let hostname = gethostname::gethostname().to_string_lossy().into_owned();

    hostname
        .strip_suffix(".local")
        .unwrap_or(&hostname)
        .to_lowercase()
}

fn seconds(
    name: &'static str,
    value: Option<String>,
    default: u64,
) -> Result<Duration, ConfigError> {
    let Some(value) = value else {
        return Ok(Duration::from_secs(default));
    };

    match value.parse::<u64>() {
        Ok(seconds) if seconds >= 1 => Ok(Duration::from_secs(seconds)),
        _ => Err(invalid(
            name,
            value,
            "must be a whole number of seconds, at least 1",
        )),
    }
}

fn path_or_home(
    name: &'static str,
    value: Option<String>,
    default: &[&str],
) -> Result<PathBuf, ConfigError> {
    if let Some(value) = value {
        return Ok(PathBuf::from(value));
    }

    let home = env::home_dir().ok_or(ConfigError::NoHomeDir(name))?;

    Ok(default.iter().fold(home, |path, part| path.join(part)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        Config::from_lookup(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
                .ok_or(VarError::NotPresent)
        })
    }

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("tailnet-connector-{}-{name}", std::process::id()));
        std::fs::write(&path, contents).unwrap();

        path
    }

    #[test]
    fn applies_defaults() {
        let config = parse(&[("TAILNET_URL", "http://100.65.233.54:8790")]).unwrap();
        let home = env::home_dir().unwrap();

        assert_eq!(config.connect_url, "ws://100.65.233.54:8790/connect");
        assert_eq!(config.idle, Duration::from_secs(300));
        assert_eq!(config.check_interval, Duration::from_secs(10));
        assert_eq!(config.claude_config, home.join(".claude.json"));
        assert_eq!(config.checks, home.join(".config/tailnet/checks.json"));
        assert!(!config.machine.is_empty());
        assert!(!config.machine.ends_with(".local"));
        assert_eq!(config.machine, config.machine.to_lowercase());
    }

    #[test]
    fn reads_overrides() {
        let config = parse(&[
            ("TAILNET_URL", "ws://hub:1/ignored/path"),
            ("TAILNET_MACHINE", "laptop"),
            ("TAILNET_IDLE_SECONDS", "5"),
            ("CLAUDE_CONFIG", "/tmp/claude.json"),
            ("TAILNET_CHECKS", "/tmp/checks.json"),
            ("TAILNET_CHECK_SECONDS", "2"),
        ])
        .unwrap();

        assert_eq!(config.connect_url, "ws://hub:1/connect");
        assert_eq!(config.machine, "laptop");
        assert_eq!(config.idle, Duration::from_secs(5));
        assert_eq!(config.claude_config, PathBuf::from("/tmp/claude.json"));
        assert_eq!(config.checks, PathBuf::from("/tmp/checks.json"));
        assert_eq!(config.check_interval, Duration::from_secs(2));
    }

    #[test]
    fn rejects_bad_env() {
        let cases: &[(&[(&str, &str)], &str)] = &[
            (&[], "TAILNET_URL is required"),
            (&[("TAILNET_URL", "100.65.233.54:8790")], "TAILNET_URL="),
            (&[("TAILNET_URL", "https://hub")], "TAILNET_URL="),
            (
                &[("TAILNET_URL", "http://hub"), ("TAILNET_MACHINE", "")],
                "TAILNET_MACHINE=",
            ),
            (
                &[("TAILNET_URL", "http://hub"), ("TAILNET_IDLE_SECONDS", "0")],
                "TAILNET_IDLE_SECONDS=",
            ),
            (
                &[
                    ("TAILNET_URL", "http://hub"),
                    ("TAILNET_CHECK_SECONDS", "ten"),
                ],
                "TAILNET_CHECK_SECONDS=",
            ),
        ];

        for (vars, expected) in cases {
            let message = parse(vars).unwrap_err().to_string();

            assert!(message.starts_with(expected), "{vars:?}: {message}");
        }
    }

    #[test]
    fn parses_mcp_servers() {
        let path = write_temp(
            "mcp-servers.json",
            r#"{
                "numStartups": 3,
                "mcpServers": {
                    "echo": { "type": "stdio", "command": "echo-server", "args": [] },
                    "remote": { "type": "http", "url": "http://example.com/mcp" },
                    "legacy": { "command": "no-type" }
                }
            }"#,
        );

        let servers = read_mcp_servers(&path).unwrap();

        assert!(matches!(
            &servers["echo"],
            ServerEntry::Supported(ServerConfig::Stdio { command }) if command == "echo-server"
        ));
        assert!(matches!(servers["remote"], ServerEntry::Unsupported(_)));
        assert!(matches!(servers["legacy"], ServerEntry::Unsupported(_)));
    }

    #[test]
    fn defaults_to_no_mcp_servers() {
        let path = write_temp("no-mcp-servers.json", "{}");

        assert!(read_mcp_servers(&path).unwrap().is_empty());
    }

    #[test]
    fn rejects_malformed_claude_config() {
        let path = write_temp("malformed.json", r#"{ "mcpServers": [] }"#);

        let message = read_mcp_servers(&path).unwrap_err().to_string();

        assert!(
            message.starts_with(&format!("cannot parse {}", path.display())),
            "{message}"
        );
    }
}
