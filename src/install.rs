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

    #[cfg(target_os = "macos")]
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
    #[cfg(target_os = "macos")]
    Home(VarError),
    #[cfg(target_os = "macos")]
    LogDir {
        path: PathBuf,
        source: io::Error,
    },
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
            #[cfg(target_os = "macos")]
            Self::Home(error) => write!(f, "cannot read HOME: {error}"),
            #[cfg(target_os = "macos")]
            Self::LogDir { path, source } => {
                write!(
                    f,
                    "cannot create the log directory {}: {source}",
                    path.display()
                )
            }
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
            #[cfg(target_os = "macos")]
            Self::Home(error) => Some(error),
            #[cfg(target_os = "macos")]
            Self::LogDir { source, .. } => Some(source),
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

/// service-manager's own plist has no `StandardOutPath` or `StandardErrorPath`, so the output
/// would be lost.
#[cfg(target_os = "macos")]
fn with_log_file(role: Role, ctx: ServiceInstallCtx) -> Result<ServiceInstallCtx, InstallError> {
    let home = env::var("HOME").map_err(InstallError::Home)?;
    let dir = PathBuf::from(home).join(LOG_DIR);
    std::fs::create_dir_all(&dir).map_err(|source| InstallError::LogDir {
        path: dir.clone(),
        source,
    })?;

    let contents = launchd_plist(&ctx, &dir.join(role.log_file_name()));

    Ok(ServiceInstallCtx {
        contents: Some(contents),
        ..ctx
    })
}

/// Without `Disabled`, so that service-manager's `start` runs a plain `launchctl start`.
#[cfg(target_os = "macos")]
fn launchd_plist(ctx: &ServiceInstallCtx, log_file: &std::path::Path) -> String {
    use plist::{Dictionary, Value};

    let string = |value: &std::ffi::OsStr| Value::String(value.to_string_lossy().into_owned());
    let log_file = string(log_file.as_os_str());
    let environment = ctx
        .environment
        .iter()
        .flatten()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<Dictionary>();

    let plist = Value::Dictionary(Dictionary::from_iter([
        ("Label", Value::String(ctx.label.to_qualified_name())),
        (
            "ProgramArguments",
            Value::Array(ctx.cmd_iter().map(string).collect()),
        ),
        ("EnvironmentVariables", Value::Dictionary(environment)),
        ("RunAtLoad", Value::Boolean(true)),
        ("KeepAlive", Value::Boolean(true)),
        ("StandardOutPath", log_file.clone()),
        ("StandardErrorPath", log_file),
    ]));

    let mut xml = Vec::new();
    plist
        .to_writer_xml(&mut xml)
        .expect("a plist of strings and booleans serializes");

    String::from_utf8(xml).expect("plist XML is UTF-8")
}

fn install(role: Role, ctx: ServiceInstallCtx) -> Result<Outcome, InstallError> {
    #[cfg(target_os = "macos")]
    let ctx = with_log_file(role, ctx)?;

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

    if let ServiceStatus::NotInstalled = status {
        return Ok(false);
    }

    // systemd reports a unit waiting out its restart delay as stopped, and it would still restart
    // after the uninstall.
    manager
        .stop(ServiceStopCtx {
            label: role.service_label(),
        })
        .map_err(InstallError::step(Step::Stop, role))?;

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

    #[cfg(target_os = "macos")]
    #[test]
    fn installed_outcome_says_where_the_logs_are() {
        assert_eq!(
            Outcome::Installed(Role::Hub).to_string(),
            "installed tailnet-connector-hub as a user service; its logs are in ~/Library/Logs/tailnet-connector/hub.log"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_plist_runs_the_service_and_writes_its_output_to_the_log_file() {
        let ctx = connector_ctx("http://100.64.0.1:8790", program(), lookup).unwrap();
        let log_file = PathBuf::from("/Users/user/Library/Logs/tailnet-connector/connector.log");

        let xml = launchd_plist(&ctx, &log_file);
        let plist = plist::Value::from_reader_xml(xml.as_bytes()).unwrap();

        let expected = plist::Value::Dictionary(plist::Dictionary::from_iter([
            ("Label", plist::Value::from("tailnet-connector")),
            (
                "ProgramArguments",
                plist::Value::Array(vec!["/opt/tailnet/tailnet-connector".into()]),
            ),
            (
                "EnvironmentVariables",
                plist::Value::Dictionary(plist::Dictionary::from_iter([
                    ("PATH", "/usr/local/bin:/usr/bin"),
                    ("TAILNET_URL", "http://100.64.0.1:8790"),
                ])),
            ),
            ("RunAtLoad", true.into()),
            ("KeepAlive", true.into()),
            (
                "StandardOutPath",
                "/Users/user/Library/Logs/tailnet-connector/connector.log".into(),
            ),
            (
                "StandardErrorPath",
                "/Users/user/Library/Logs/tailnet-connector/connector.log".into(),
            ),
        ]));
        assert_eq!(plist, expected);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchd_plist_passes_the_hub_command() {
        let ctx = hub_ctx(program(), lookup).unwrap();
        let log_file = PathBuf::from("/Users/user/Library/Logs/tailnet-connector/hub.log");

        let xml = launchd_plist(&ctx, &log_file);
        let plist = plist::Value::from_reader_xml(xml.as_bytes()).unwrap();
        let plist = plist.as_dictionary().unwrap();

        assert_eq!(plist["Label"], "tailnet-connector-hub".into());
        assert_eq!(
            plist["ProgramArguments"],
            plist::Value::Array(vec!["/opt/tailnet/tailnet-connector".into(), "hub".into()])
        );
        assert_eq!(
            plist["StandardErrorPath"],
            "/Users/user/Library/Logs/tailnet-connector/hub.log".into()
        );
    }
}
