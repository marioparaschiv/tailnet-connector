use std::env::{self, VarError};
use std::process::Command;
use std::time::Duration;

use crate::config::{ConfigError, var};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubConfig {
    /// The address to bind, which defaults to this machine's Tailscale IPv4 address.
    pub host: String,
    pub port: u16,
    /// How often each agent session is told if its tools changed.
    pub notify: Duration,
}

impl HubConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| env::var(name), Self::tailscale_ip)
    }

    /// `tailscale_ip` is only called when TAILNET_MCP_HOST is not set.
    pub fn from_lookup(
        lookup: impl Fn(&str) -> Result<String, VarError>,
        tailscale_ip: impl FnOnce() -> Result<String, String>,
    ) -> Result<Self, ConfigError> {
        let var = |name| var(&lookup, name);

        let host = match var("TAILNET_MCP_HOST")? {
            Some(host) => host,
            None => tailscale_ip().map_err(|reason| ConfigError::Default {
                name: "TAILNET_MCP_HOST",
                command: "tailscale ip -4",
                reason,
            })?,
        };

        Ok(Self {
            host,
            port: Self::port(var("TAILNET_MCP_PORT")?)?,
            notify: Self::notify(var("TAILNET_MCP_NOTIFY_SECONDS")?)?,
        })
    }

    // Binding the Tailscale address keeps the hub off the public interface.
    fn tailscale_ip() -> Result<String, String> {
        let output = Command::new("tailscale")
            .args(["ip", "-4"])
            .output()
            .map_err(|error| error.to_string())?;

        if !output.status.success() {
            return Err(format!(
                "{}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let ip = String::from_utf8_lossy(&output.stdout).trim().to_owned();

        if ip.is_empty() {
            return Err("it printed no address".to_owned());
        }

        Ok(ip)
    }

    fn port(value: Option<String>) -> Result<u16, ConfigError> {
        let Some(value) = value else {
            return Ok(8790);
        };

        match value.parse::<u16>() {
            Ok(port) if port >= 1 => Ok(port),
            _ => Err(ConfigError::Invalid {
                name: "TAILNET_MCP_PORT",
                value,
                reason: "must be a port number from 1 to 65535",
            }),
        }
    }

    fn notify(value: Option<String>) -> Result<Duration, ConfigError> {
        let Some(value) = value else {
            return Ok(Duration::from_secs(300));
        };

        match value.parse::<u64>() {
            Ok(seconds) if seconds >= 5 => Ok(Duration::from_secs(seconds)),
            _ => Err(ConfigError::Invalid {
                name: "TAILNET_MCP_NOTIFY_SECONDS",
                value,
                reason: "must be a whole number of seconds, at least 5",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(vars: &[(&str, &str)]) -> Result<HubConfig, ConfigError> {
        HubConfig::from_lookup(
            |name| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
                    .ok_or(VarError::NotPresent)
            },
            || Ok("100.65.233.54".to_owned()),
        )
    }

    #[test]
    fn applies_defaults() {
        assert_eq!(
            parse(&[]).unwrap(),
            HubConfig {
                host: "100.65.233.54".to_owned(),
                port: 8790,
                notify: Duration::from_secs(300),
            }
        );
    }

    #[test]
    fn reads_overrides() {
        let config = HubConfig::from_lookup(
            |name| match name {
                "TAILNET_MCP_HOST" => Ok("127.0.0.1".to_owned()),
                "TAILNET_MCP_PORT" => Ok("65535".to_owned()),
                "TAILNET_MCP_NOTIFY_SECONDS" => Ok("5".to_owned()),
                _ => Err(VarError::NotPresent),
            },
            || panic!("tailscale is not asked when TAILNET_MCP_HOST is set"),
        )
        .unwrap();

        assert_eq!(
            config,
            HubConfig {
                host: "127.0.0.1".to_owned(),
                port: 65535,
                notify: Duration::from_secs(5),
            }
        );
    }

    #[test]
    fn rejects_bad_env() {
        let cases: &[(&str, &str)] = &[
            ("TAILNET_MCP_PORT", "0"),
            ("TAILNET_MCP_PORT", "65536"),
            ("TAILNET_MCP_PORT", "http"),
            ("TAILNET_MCP_NOTIFY_SECONDS", "4"),
            ("TAILNET_MCP_NOTIFY_SECONDS", "5.5"),
            ("TAILNET_MCP_NOTIFY_SECONDS", "-10"),
        ];

        for (name, value) in cases {
            let message = parse(&[(name, value)]).unwrap_err().to_string();

            assert!(
                message.starts_with(&format!("{name}={value:?} is invalid")),
                "{message}"
            );
        }
    }

    #[test]
    fn names_the_host_variable_when_tailscale_fails() {
        let error = HubConfig::from_lookup(
            |_| Err(VarError::NotPresent),
            || Err("No such file or directory (os error 2)".to_owned()),
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "TAILNET_MCP_HOST is not set and `tailscale ip -4` failed: No such file or directory (os error 2)"
        );
    }
}
