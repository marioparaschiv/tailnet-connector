//! A windows-subsystem exe starts without stdout or stderr, so both are pointed at a log file,
//! which MCP servers started later inherit as their stderr too.

use std::ffi::c_void;
use std::fs::OpenOptions;
use std::io;
use std::os::windows::io::IntoRawHandle;
use std::path::Path;

const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const STD_ERROR_HANDLE: u32 = -12i32 as u32;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetStdHandle(std_handle: u32, handle: *mut c_void) -> i32;
}

pub fn append_output_to(path: &Path) -> io::Result<()> {
    // The handle stays open for the life of the process as its stdout and stderr.
    let handle = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .into_raw_handle();

    for std_handle in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `handle` is an open file handle that is never closed.
        if unsafe { SetStdHandle(std_handle, handle) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(())
}
