//! Pi's MCP config: `mcp.json` in Pi's agent directory for the user, `.pi/mcp.json` in a project.

use std::env;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::config::{self, ConfigError, McpServers};

#[derive(Default, Deserialize)]
struct PiConfig {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: McpServers,
}

/// The `mcpServers` of a Pi config file, empty when the file does not exist.
pub fn read(path: &Path) -> Result<McpServers, ConfigError> {
    Ok(config::read_json_if_exists::<PiConfig>(path)?
        .unwrap_or_default()
        .mcp_servers)
}

/// The servers Pi connects in a project, by Pi's precedence: a project entry replaces a user one,
/// except that an entry without `command`, `url` or `type` only overrides `enabled` and the
/// exposure settings. Disabled servers are left out.
pub fn merge(user: McpServers, project: McpServers) -> McpServers {
    let mut servers = user;

    for (name, entry) in project {
        match (servers.get_mut(&name), entry) {
            (Some(Value::Object(user)), Value::Object(overrides))
                if !defines_server(&overrides) =>
            {
                user.extend(overrides);
            }
            (_, entry) => {
                servers.insert(name, entry);
            }
        }
    }

    servers.retain(|_, entry| entry.get("enabled") != Some(&Value::Bool(false)));

    servers
}

/// Rewrites Pi's entries into the shape of Claude Code's: `type` is optional in Pi, which also
/// accepts `streamable-http` for `http` and expands a leading `~/` in a stdio server's paths.
/// An http entry without an `Authorization` header is marked to use Pi's OAuth sign-in.
pub fn as_claude_entries(servers: McpServers) -> McpServers {
    servers
        .into_iter()
        .map(|(name, mut entry)| {
            if let Value::Object(fields) = &mut entry {
                let kind = match fields.get("type").and_then(Value::as_str) {
                    None if fields.contains_key("command") => Some("stdio"),
                    None if fields.contains_key("url") => Some("http"),
                    Some("streamable-http") => Some("http"),
                    _ => None,
                };

                if let Some(kind) = kind {
                    fields.insert("type".to_owned(), kind.into());
                }

                if fields.get("type").and_then(Value::as_str) == Some("http")
                    && !has_authorization(fields)
                {
                    fields.insert("piOAuth".to_owned(), true.into());
                }

                for key in ["command", "args", "cwd"] {
                    if let Some(value) = fields.get_mut(key) {
                        expand_home(value);
                    }
                }
            }

            (name, entry)
        })
        .collect()
}

fn has_authorization(entry: &serde_json::Map<String, Value>) -> bool {
    entry
        .get("headers")
        .and_then(Value::as_object)
        .is_some_and(|headers| {
            headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("authorization"))
        })
}

fn defines_server(entry: &serde_json::Map<String, Value>) -> bool {
    ["command", "url", "type"]
        .iter()
        .any(|key| entry.contains_key(*key))
}

fn expand_home(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let (Some(rest), Some(home)) = (text.strip_prefix("~/"), env::home_dir()) {
                *text = home.join(rest).to_string_lossy().into_owned();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(expand_home),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::config::ServerConfig;

    fn servers(value: Value) -> McpServers {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn merges_project_entries_like_pi() {
        let merged = merge(
            servers(json!({
                "docs": { "url": "https://docs.example.com/mcp", "headers": { "X-Key": "k" } },
                "internal": { "command": "internal-mcp" },
                "off": { "command": "off-mcp", "enabled": false },
                "kept": { "command": "kept-mcp" },
            })),
            servers(json!({
                "docs": { "url": "https://project.example.com/mcp" },
                "internal": { "enabled": false },
                "off": { "enabled": true, "exposure": "direct" },
                "local": { "command": "local-mcp" },
            })),
        );

        assert_eq!(
            Value::Object(merged),
            json!({
                "docs": { "url": "https://project.example.com/mcp" },
                "off": { "command": "off-mcp", "enabled": true, "exposure": "direct" },
                "kept": { "command": "kept-mcp" },
                "local": { "command": "local-mcp" },
            })
        );
    }

    #[test]
    fn rewrites_entries_into_claude_server_configs() {
        let home = env::home_dir().unwrap();
        let entries = as_claude_entries(servers(json!({
            "stdio": { "command": "~/bin/mcp", "args": ["~/data", "-v"], "cwd": "~/work", "exposure": "direct" },
            "http": { "url": "https://example.com/mcp", "description": "Docs" },
            "bearer": { "url": "https://example.com/mcp", "headers": { "authorization": "Bearer k" } },
            "streamable": { "type": "streamable-http", "url": "https://example.com/mcp" },
            "sse": { "type": "sse", "url": "https://example.com/sse" },
        })));
        let parse = |name: &str| ServerConfig::deserialize(&entries[name]);

        assert!(matches!(
            parse("stdio").unwrap(),
            ServerConfig::Stdio { command, args, cwd, .. }
                if command == home.join("bin/mcp").to_str().unwrap()
                    && args == [home.join("data").to_str().unwrap(), "-v"]
                    && cwd == Some(home.join("work"))
        ));
        assert!(matches!(
            parse("http").unwrap(),
            ServerConfig::Http { pi_oauth: true, .. }
        ));
        assert!(matches!(
            parse("bearer").unwrap(),
            ServerConfig::Http {
                pi_oauth: false,
                ..
            }
        ));
        assert!(matches!(
            parse("streamable").unwrap(),
            ServerConfig::Http { .. }
        ));
        assert!(parse("sse").is_err());
    }

    #[test]
    fn reads_a_missing_file_as_no_servers() {
        let path = env::temp_dir().join("tailnet-connector-no-such-pi-mcp.json");

        assert!(read(&path).unwrap().is_empty());
    }
}
