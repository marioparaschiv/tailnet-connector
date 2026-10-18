//! A Task Scheduler task that starts the connector at the user's logon. Task Scheduler starts it
//! with the user's environment, so the connector's settings are user environment variables.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
};
use windows::Win32::System::TaskScheduler::{
    ITaskFolder, ITaskService, TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN, TaskScheduler,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{BSTR, HSTRING, w};

use super::task_xml::task_xml;
use super::{InstallError, LOG_DIR, Outcome, Role, Step, connector_environment, required_var};

/// Every variable that `install_connector` sets.
const VARIABLES: [&str; 2] = ["TAILNET_LOG", "TAILNET_URL"];

pub fn install_connector(url: &str) -> Result<Outcome, InstallError> {
    let role = Role::Connector;
    let log_dir = format!("{}\\{LOG_DIR}", required_var("LOCALAPPDATA")?);
    let log_file = format!("{log_dir}\\{}", role.log_file_name());
    let environment = connector_environment(url, vec![("TAILNET_LOG".to_owned(), log_file)])?;
    let user = format!(
        "{}\\{}",
        required_var("USERDOMAIN")?,
        required_var("USERNAME")?
    );

    let program = env::current_exe().map_err(InstallError::Program)?;
    let (Some(command), Some(working_dir)) =
        (program.to_str(), program.parent().and_then(Path::to_str))
    else {
        return Err(InstallError::Program(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not valid Unicode", program.display()),
        )));
    };
    let xml = task_xml(command, working_dir, &user);

    fs::create_dir_all(&log_dir).map_err(|source| InstallError::LogDir {
        path: PathBuf::from(&log_dir),
        source,
    })?;

    let folder = folder().map_err(InstallError::Manager)?;
    remove(&folder, role)?;

    set_environment(&environment).map_err(InstallError::step(Step::SetEnvironment, role))?;

    let none = VARIANT::default();
    // SAFETY: COM is initialized on this thread, and every argument outlives the call.
    let task = unsafe {
        folder.RegisterTask(
            &BSTR::from(role.label()),
            &BSTR::from(xml),
            TASK_CREATE_OR_UPDATE.0,
            &none,
            &none,
            TASK_LOGON_INTERACTIVE_TOKEN,
            &none,
        )
    }
    .map_err(InstallError::step(Step::Install, role))?;

    // SAFETY: COM is initialized on this thread, and `none` outlives the call.
    unsafe { task.Run(&none) }.map_err(InstallError::step(Step::Start, role))?;

    Ok(Outcome::Installed(role))
}

pub fn uninstall(role: Role) -> Result<Outcome, InstallError> {
    let folder = folder().map_err(InstallError::Manager)?;

    if remove(&folder, role)? {
        Ok(Outcome::Removed(role))
    } else {
        Ok(Outcome::NotInstalled(role))
    }
}

/// The root folder of the Task Scheduler, which holds the user's own tasks.
fn folder() -> windows::core::Result<ITaskFolder> {
    // SAFETY: COM stays initialized on this thread for the rest of the process, so it outlives
    // every interface taken from it.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;

        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
        let none = VARIANT::default();
        service.Connect(&none, &none, &none, &none)?;

        service.GetFolder(&BSTR::from("\\"))
    }
}

/// Returns whether there was a task to remove.
fn remove(folder: &ITaskFolder, role: Role) -> Result<bool, InstallError> {
    let name = BSTR::from(role.label());

    // SAFETY: COM is initialized on this thread, and `name` outlives the call.
    let task = match unsafe { folder.GetTask(&name) } {
        Ok(task) => task,
        Err(error)
            if error.code() == ERROR_FILE_NOT_FOUND.to_hresult()
                || error.code() == ERROR_PATH_NOT_FOUND.to_hresult() =>
        {
            return Ok(false);
        }
        Err(error) => return Err(InstallError::step(Step::Status, role)(error)),
    };

    // SAFETY: COM is initialized on this thread.
    unsafe { task.Stop(0) }.map_err(InstallError::step(Step::Stop, role))?;

    // SAFETY: COM is initialized on this thread, and `name` outlives the call.
    unsafe { folder.DeleteTask(&name, 0) }.map_err(InstallError::step(Step::Uninstall, role))?;

    clear_environment().map_err(InstallError::step(Step::ClearEnvironment, role))?;

    Ok(true)
}

fn set_environment(environment: &[(String, String)]) -> windows::core::Result<()> {
    for (name, value) in environment {
        let data: Vec<u16> = value.encode_utf16().chain([0]).collect();
        let size = u32::try_from(size_of_val(data.as_slice()))?;

        // SAFETY: `data` is a NUL-terminated UTF-16 string of `size` bytes, and the strings outlive
        // the call.
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                w!("Environment"),
                &HSTRING::from(name),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                size,
            )
        }
        .ok()?;
    }

    Ok(())
}

fn clear_environment() -> windows::core::Result<()> {
    for name in VARIABLES {
        // SAFETY: both strings are NUL-terminated and outlive the call.
        let result = unsafe {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, w!("Environment"), &HSTRING::from(name))
        };

        if result != ERROR_FILE_NOT_FOUND {
            result.ok()?;
        }
    }

    Ok(())
}
