use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::future::join_all;
use serde::Deserialize;
use tokio::net::TcpStream;

use crate::config::{self, ConfigError};

const TIMEOUT: Duration = Duration::from_secs(1);

/// What must accept a connection for a service to count as available.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Check {
    Tcp(TcpAddress),
    UnixSocket(String),
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "String")]
pub struct TcpAddress {
    host: String,
    port: u16,
}

impl TryFrom<String> for TcpAddress {
    type Error = &'static str;

    fn try_from(address: String) -> Result<Self, Self::Error> {
        let (host, port) = address.split_once(':').ok_or("expected host:port")?;
        let digits = port.bytes().all(|byte| byte.is_ascii_digit());

        match port.parse() {
            Ok(port) if digits && !host.is_empty() => Ok(Self {
                host: host.to_owned(),
                port,
            }),
            _ => Err("expected host:port"),
        }
    }
}

pub type Checks = BTreeMap<String, Check>;

/// Reads the checks file; a missing file means no service has a check.
pub fn read(path: &Path) -> Result<Checks, ConfigError> {
    match config::read_json(path) {
        Err(ConfigError::Read { source, .. }) if source.kind() == ErrorKind::NotFound => {
            Ok(Checks::new())
        }
        checks => checks,
    }
}

/// The names that have no check or whose check passes.
pub async fn available<'a>(
    names: impl Iterator<Item = &'a str>,
    checks: &Checks,
) -> BTreeSet<String> {
    let results = join_all(names.map(|name| async move {
        let available = match checks.get(name) {
            Some(check) => passes(check).await,
            None => true,
        };

        available.then(|| name.to_owned())
    }))
    .await;

    results.into_iter().flatten().collect()
}

// A socket file can outlive the app that made it, so only an accepted connection counts.
pub async fn passes(check: &Check) -> bool {
    let connect = async {
        match check {
            Check::Tcp(TcpAddress { host, port }) => {
                TcpStream::connect((host.as_str(), *port)).await.is_ok()
            }
            Check::UnixSocket(path) => connect_unix(&expand_home(path)).await,
        }
    };

    tokio::time::timeout(TIMEOUT, connect)
        .await
        .unwrap_or(false)
}

#[cfg(unix)]
async fn connect_unix(path: &Path) -> bool {
    tokio::net::UnixStream::connect(path).await.is_ok()
}

#[cfg(not(unix))]
async fn connect_unix(_: &Path) -> bool {
    false
}

fn expand_home(path: &str) -> PathBuf {
    let rest = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest.trim_start_matches('/'),
        _ => return PathBuf::from(path),
    };

    match std::env::home_dir() {
        Some(home) => home.join(rest),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tcp_addresses() {
        let address = TcpAddress::try_from("vm.local:8080".to_owned()).unwrap();
        assert_eq!((address.host.as_str(), address.port), ("vm.local", 8080));

        for invalid in [
            "no-port",
            ":80",
            "host:",
            "host:+80",
            "host:99999",
            "::1:80",
        ] {
            assert!(
                TcpAddress::try_from(invalid.to_owned()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn expands_a_leading_tilde() {
        let home = std::env::home_dir().unwrap();

        assert_eq!(expand_home("~/vm.sock"), home.join("vm.sock"));
        assert_eq!(expand_home("~"), home);
        assert_eq!(
            expand_home("~other/vm.sock"),
            PathBuf::from("~other/vm.sock")
        );
        assert_eq!(expand_home("/tmp/vm.sock"), PathBuf::from("/tmp/vm.sock"));
    }
}
