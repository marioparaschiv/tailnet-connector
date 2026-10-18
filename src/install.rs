//! `install` and `uninstall`: run the connector or the hub as a background service of the user
//! who runs the command. On Windows only the connector installs, as a scheduled task.

use std::env::{self, VarError};
use std::fmt;
use std::io;
#[cfg(any(target_os = "macos", windows))]
use std::path::PathBuf;

use crate::config::{Config, ConfigError, var};

#[cfg(not(windows))]
mod service;
#[cfg(windows)]
mod task;
#[cfg(any(windows, test))]
mod task_xml;

#[cfg(not(windows))]
pub use service::{install_connector, install_hub, uninstall};
#[cfg(windows)]
pub use task::{install_connector, uninstall};

#[cfg(not(windows))]
type ManagerError = io::Error;
#[cfg(windows)]
type ManagerError = windows::core::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Connector,
    Hub,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Self::Connector => "tailnet-connector",
            Self::Hub => "tailnet-connector-hub",
        }
    }

    #[cfg(any(target_os = "macos", windows))]
    fn log_file_name(self) -> &'static str {
        match self {
            Self::Connector => "connector.log",
            Self::Hub => "hub.log",
        }
    }
}

/// Relative to the home directory.
#[cfg(target_os = "macos")]
const LOG_DIR: &str = "Library/Logs/tailnet-connector";

/// Relative to `%LOCALAPPDATA%`.
#[cfg(windows)]
const LOG_DIR: &str = "tailnet-connector";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Installed(Role),
    Removed(Role),
    NotInstalled(Role),
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Installed(role) => {
                let label = role.label();
                write!(f, "installed {label} as a user service")?;

                if cfg!(target_os = "linux") {
                    write!(f, "; read its logs with `journalctl --user -u {label}`")?;
                }

                #[cfg(target_os = "macos")]
                write!(f, "; its logs are in ~/{LOG_DIR}/{}", role.log_file_name())?;

                #[cfg(windows)]
                write!(
                    f,
                    "; its log is in %LOCALAPPDATA%\\{LOG_DIR}\\{}",
                    role.log_file_name()
                )?;

                Ok(())
            }
            Self::Removed(role) => write!(f, "removed {}", role.label()),
            Self::NotInstalled(role) => write!(f, "{} is not installed", role.label()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Step {
    Status,
    Stop,
    Uninstall,
    Install,
    Start,
    #[cfg(windows)]
    SetEnvironment,
    #[cfg(windows)]
    ClearEnvironment,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Status => "read the status of",
            Self::Stop => "stop",
            Self::Uninstall => "uninstall",
            Self::Install => "install",
            Self::Start => "start",
            #[cfg(windows)]
            Self::SetEnvironment => "set the user environment variables of",
            #[cfg(windows)]
            Self::ClearEnvironment => "clear the user environment variables of",
        })
    }
}

#[derive(Debug)]
pub enum InstallError {
    Config(ConfigError),
    Program(io::Error),
    #[cfg(any(target_os = "macos", windows))]
    LogDir {
        path: PathBuf,
        source: io::Error,
    },
    Manager(ManagerError),
    Step {
        step: Step,
        role: Role,
        source: ManagerError,
    },
}

impl InstallError {
    fn step(step: Step, role: Role) -> impl FnOnce(ManagerError) -> Self {
        move |source| Self::Step { step, role, source }
    }
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => write!(f, "{error}"),
            Self::Program(error) => write!(f, "cannot locate this executable: {error}"),
            #[cfg(any(target_os = "macos", windows))]
            Self::LogDir { path, source } => {
                write!(
                    f,
                    "cannot create the log directory {}: {source}",
                    path.display()
                )
            }
            #[cfg(not(windows))]
            Self::Manager(error) => write!(f, "cannot use the user service manager: {error}"),
            #[cfg(windows)]
            Self::Manager(error) => write!(f, "cannot use the task scheduler: {error}"),
            Self::Step { step, role, source } => {
                write!(f, "cannot {step} {}: {source}", role.label())
            }
        }
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            #[cfg(any(target_os = "macos", windows))]
            Self::LogDir { source, .. } => Some(source),
            Self::Program(error) => Some(error),
            Self::Manager(error) | Self::Step { source: error, .. } => Some(error),
        }
    }
}

/// `environment` with `TAILNET_URL` added. Fails with the error the connector itself would give
/// when started with it.
fn connector_environment(
    url: &str,
    mut environment: Vec<(String, String)>,
) -> Result<Vec<(String, String)>, InstallError> {
    environment.push(("TAILNET_URL".to_owned(), url.to_owned()));

    Config::from_lookup(|name| {
        environment
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .ok_or(VarError::NotPresent)
    })
    .map_err(InstallError::Config)?;

    Ok(environment)
}

fn required_var(name: &'static str) -> Result<String, InstallError> {
    var(|name| env::var(name), name)
        .and_then(|value| value.ok_or(ConfigError::Missing(name)))
        .map_err(InstallError::Config)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "/usr/local/bin:/usr/bin";

    fn path() -> Vec<(String, String)> {
        vec![("PATH".to_owned(), PATH.to_owned())]
    }

    #[test]
    fn connector_environment_adds_the_url() {
        let environment = connector_environment("http://100.64.0.1:8790", path()).unwrap();

        assert_eq!(
            environment,
            vec![
                ("PATH".to_owned(), PATH.to_owned()),
                (
                    "TAILNET_URL".to_owned(),
                    "http://100.64.0.1:8790".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn connector_rejects_the_urls_the_connector_rejects() {
        for url in ["https://hub", "not-a-url"] {
            let error = connector_environment(url, path()).unwrap_err();
            let expected = Config::from_lookup(|name| match name {
                "PATH" => Ok(PATH.to_owned()),
                "TAILNET_URL" => Ok(url.to_owned()),
                _ => Err(VarError::NotPresent),
            })
            .unwrap_err();

            assert_eq!(error.to_string(), expected.to_string());
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn step_errors_name_the_step_and_the_service() {
        let error = InstallError::step(Step::Start, Role::Connector)(io::Error::other("boom"));

        assert_eq!(error.to_string(), "cannot start tailnet-connector: boom");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn installed_outcome_says_where_the_logs_are() {
        assert_eq!(
            Outcome::Installed(Role::Hub).to_string(),
            "installed tailnet-connector-hub as a user service; read its logs with `journalctl --user -u tailnet-connector-hub`"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn installed_outcome_says_where_the_logs_are() {
        assert_eq!(
            Outcome::Installed(Role::Hub).to_string(),
            "installed tailnet-connector-hub as a user service; its logs are in ~/Library/Logs/tailnet-connector/hub.log"
        );
    }

    #[cfg(windows)]
    #[test]
    fn installed_outcome_says_where_the_log_is() {
        assert_eq!(
            Outcome::Installed(Role::Connector).to_string(),
            r"installed tailnet-connector as a user service; its log is in %LOCALAPPDATA%\tailnet-connector\connector.log"
        );
    }
}
