//! `install` and `uninstall`: run the connector or the hub as a background service of the user
//! who runs the command.

use std::env::{self, VarError};
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;

use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager, ServiceStartCtx,
    ServiceStatus, ServiceStatusCtx, ServiceStopCtx, ServiceUninstallCtx,
};

use crate::config::{Config, ConfigError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Connector,
    Hub,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Self::Connector => "tailnet-connector",
            Self::Hub => "tailnet-connector-hub",
        }
    }

    fn service_label(self) -> ServiceLabel {
        ServiceLabel {
            qualifier: None,
            organization: None,
            application: self.label().to_owned(),
        }
    }

    fn args(self) -> Vec<OsString> {
        match self {
            Self::Connector => Vec::new(),
            Self::Hub => vec!["hub".into()],
        }
    }
}

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
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Status => "read the status of",
            Self::Stop => "stop",
            Self::Uninstall => "uninstall",
            Self::Install => "install",
            Self::Start => "start",
        })
    }
}

#[derive(Debug)]
pub enum InstallError {
    Config(ConfigError),
    Path(VarError),
    Program(io::Error),
    Manager(io::Error),
    Step {
        step: Step,
        role: Role,
        source: io::Error,
    },
}

impl InstallError {
    fn step(step: Step, role: Role) -> impl FnOnce(io::Error) -> Self {
        move |source| Self::Step { step, role, source }
    }
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => write!(f, "{error}"),
            Self::Path(error) => write!(f, "cannot read PATH: {error}"),
            Self::Program(error) => write!(f, "cannot locate this executable: {error}"),
            Self::Manager(error) => write!(f, "cannot use the user service manager: {error}"),
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
            Self::Path(error) => Some(error),
            Self::Program(error) | Self::Manager(error) | Self::Step { source: error, .. } => {
                Some(error)
            }
        }
    }
}

pub fn install_connector(url: &str) -> Result<Outcome, InstallError> {
    let ctx = connector_ctx(url, program()?, |name| env::var(name))?;

    install(Role::Connector, ctx)
}

pub fn install_hub() -> Result<Outcome, InstallError> {
    let ctx = hub_ctx(program()?, |name| env::var(name))?;

    install(Role::Hub, ctx)
}

pub fn uninstall(role: Role) -> Result<Outcome, InstallError> {
    if remove(&*manager()?, role)? {
        Ok(Outcome::Removed(role))
    } else {
        Ok(Outcome::NotInstalled(role))
    }
}

/// Fails with the error the connector itself would give if `url` is not a usable `TAILNET_URL`.
fn connector_ctx(
    url: &str,
    program: PathBuf,
    lookup: impl Fn(&str) -> Result<String, VarError>,
) -> Result<ServiceInstallCtx, InstallError> {
    let lookup = |name: &str| match name {
        "TAILNET_URL" => Ok(url.to_owned()),
        _ => lookup(name),
    };
    Config::from_lookup(lookup).map_err(InstallError::Config)?;

    let path = lookup("PATH").map_err(InstallError::Path)?;

    Ok(ctx(
        Role::Connector,
        program,
        vec![
            ("PATH".to_owned(), path),
            ("TAILNET_URL".to_owned(), url.to_owned()),
        ],
    ))
}

fn hub_ctx(
    program: PathBuf,
    lookup: impl Fn(&str) -> Result<String, VarError>,
) -> Result<ServiceInstallCtx, InstallError> {
    let path = lookup("PATH").map_err(InstallError::Path)?;

    Ok(ctx(Role::Hub, program, vec![("PATH".to_owned(), path)]))
}

fn ctx(role: Role, program: PathBuf, environment: Vec<(String, String)>) -> ServiceInstallCtx {
    ServiceInstallCtx {
        label: role.service_label(),
        program,
        args: role.args(),
        contents: None,
        username: None,
        working_directory: None,
        environment: Some(environment),
        autostart: true,
        restart_policy: RestartPolicy::Always {
            delay_secs: Some(60),
        },
    }
}

fn program() -> Result<PathBuf, InstallError> {
    env::current_exe().map_err(InstallError::Program)
}

fn manager() -> Result<Box<dyn ServiceManager>, InstallError> {
    let mut manager = <dyn ServiceManager>::native().map_err(InstallError::Manager)?;
    manager
        .set_level(ServiceLevel::User)
        .map_err(InstallError::Manager)?;

    Ok(manager)
}

fn install(role: Role, ctx: ServiceInstallCtx) -> Result<Outcome, InstallError> {
    let manager = manager()?;
    remove(&*manager, role)?;

    manager
        .install(ctx)
        .map_err(InstallError::step(Step::Install, role))?;
    manager
        .start(ServiceStartCtx {
            label: role.service_label(),
        })
        .map_err(InstallError::step(Step::Start, role))?;

    Ok(Outcome::Installed(role))
}

/// Returns whether there was a service to remove.
fn remove(manager: &dyn ServiceManager, role: Role) -> Result<bool, InstallError> {
    let status = manager
        .status(ServiceStatusCtx {
            label: role.service_label(),
        })
        .map_err(InstallError::step(Step::Status, role))?;

    match status {
        ServiceStatus::NotInstalled => return Ok(false),
        ServiceStatus::Running => manager
            .stop(ServiceStopCtx {
                label: role.service_label(),
            })
            .map_err(InstallError::step(Step::Stop, role))?,
        ServiceStatus::Stopped(_) => {}
    }

    manager
        .uninstall(ServiceUninstallCtx {
            label: role.service_label(),
        })
        .map_err(InstallError::step(Step::Uninstall, role))?;

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(name: &str) -> Result<String, VarError> {
        match name {
            "PATH" => Ok("/usr/local/bin:/usr/bin".to_owned()),
            "HOME" => Ok("/home/user".to_owned()),
            _ => Err(VarError::NotPresent),
        }
    }

    fn program() -> PathBuf {
        PathBuf::from("/opt/tailnet/tailnet-connector")
    }

    #[test]
    fn connector_runs_without_arguments_with_the_url_and_path() {
        let ctx = connector_ctx("http://100.64.0.1:8790", program(), lookup).unwrap();

        assert_eq!(ctx.label.to_script_name(), "tailnet-connector");
        assert_eq!(ctx.program, program());
        assert!(ctx.args.is_empty());
        assert_eq!(
            ctx.environment,
            Some(vec![
                ("PATH".to_owned(), "/usr/local/bin:/usr/bin".to_owned()),
                (
                    "TAILNET_URL".to_owned(),
                    "http://100.64.0.1:8790".to_owned()
                ),
            ])
        );
        assert!(ctx.autostart);
        assert_eq!(
            ctx.restart_policy,
            RestartPolicy::Always {
                delay_secs: Some(60)
            }
        );
    }

    #[test]
    fn connector_url_overrides_the_environment() {
        let lookup = |name: &str| match name {
            "TAILNET_URL" => Ok("not-a-url".to_owned()),
            _ => lookup(name),
        };

        let ctx = connector_ctx("http://100.64.0.1:8790", program(), lookup).unwrap();

        assert_eq!(
            ctx.environment.unwrap()[1],
            (
                "TAILNET_URL".to_owned(),
                "http://100.64.0.1:8790".to_owned()
            )
        );
    }

    #[test]
    fn connector_rejects_the_urls_the_connector_rejects() {
        for url in ["https://hub", "not-a-url"] {
            let error = connector_ctx(url, program(), lookup).unwrap_err();
            let expected = Config::from_lookup(|name| match name {
                "TAILNET_URL" => Ok(url.to_owned()),
                _ => lookup(name),
            })
            .unwrap_err();

            assert_eq!(error.to_string(), expected.to_string());
        }
    }

    #[test]
    fn connector_rejects_other_invalid_settings() {
        let lookup = |name: &str| match name {
            "TAILNET_IDLE_SECONDS" => Ok("soon".to_owned()),
            _ => lookup(name),
        };

        let error = connector_ctx("http://100.64.0.1:8790", program(), lookup).unwrap_err();

        assert!(matches!(
            error,
            InstallError::Config(ConfigError::Invalid {
                name: "TAILNET_IDLE_SECONDS",
                ..
            })
        ));
    }

    #[test]
    fn hub_runs_the_hub_command_with_path_only() {
        let ctx = hub_ctx(program(), lookup).unwrap();

        assert_eq!(ctx.label.to_script_name(), "tailnet-connector-hub");
        assert_eq!(ctx.program, program());
        assert_eq!(ctx.args, vec![OsString::from("hub")]);
        assert_eq!(
            ctx.environment,
            Some(vec![(
                "PATH".to_owned(),
                "/usr/local/bin:/usr/bin".to_owned()
            )])
        );
        assert!(ctx.autostart);
        assert_eq!(
            ctx.restart_policy,
            RestartPolicy::Always {
                delay_secs: Some(60)
            }
        );
    }

    #[test]
    fn missing_path_is_reported() {
        let error = hub_ctx(program(), |_| Err(VarError::NotPresent)).unwrap_err();

        assert_eq!(
            error.to_string(),
            "cannot read PATH: environment variable not found"
        );
    }

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
}
