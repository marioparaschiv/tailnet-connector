//! A systemd user service on Linux, a LaunchAgent on macOS.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager, ServiceStartCtx,
    ServiceStatus, ServiceStatusCtx, ServiceStopCtx, ServiceUninstallCtx,
};

#[cfg(target_os = "macos")]
use super::LOG_DIR;
use super::{InstallError, Outcome, Role, Step, connector_environment, required_var};
#[cfg(target_os = "macos")]
use crate::config::ConfigError;

impl Role {
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

pub fn install_connector(url: &str) -> Result<Outcome, InstallError> {
    let environment = connector_environment(url, vec![("PATH".to_owned(), required_var("PATH")?)])?;

    install(Role::Connector, environment)
}

pub fn install_hub() -> Result<Outcome, InstallError> {
    let path = required_var("PATH")?;

    install(Role::Hub, vec![("PATH".to_owned(), path)])
}

pub fn uninstall(role: Role) -> Result<Outcome, InstallError> {
    if remove(&*manager()?, role)? {
        Ok(Outcome::Removed(role))
    } else {
        Ok(Outcome::NotInstalled(role))
    }
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
    let home = env::home_dir().ok_or(InstallError::Config(ConfigError::NoHomeDir("HOME")))?;
    let dir = home.join(LOG_DIR);
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

fn install(role: Role, environment: Vec<(String, String)>) -> Result<Outcome, InstallError> {
    let program = env::current_exe().map_err(InstallError::Program)?;
    let ctx = ctx(role, program, environment);

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

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    const PATH: &str = "/usr/local/bin:/usr/bin";

    #[test]
    fn launchd_plist_runs_the_service_and_writes_its_output_to_the_log_file() {
        let ctx = ctx(
            Role::Connector,
            PathBuf::from("/opt/tailnet/tailnet-connector"),
            connector_environment(
                "http://100.64.0.1:8790",
                vec![("PATH".to_owned(), PATH.to_owned())],
            )
            .unwrap(),
        );
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

    #[test]
    fn launchd_plist_passes_the_hub_command() {
        let ctx = ctx(
            Role::Hub,
            PathBuf::from("/opt/tailnet/tailnet-connector"),
            vec![("PATH".to_owned(), PATH.to_owned())],
        );
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
