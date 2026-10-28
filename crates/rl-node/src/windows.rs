//! Windows process-tree control: a kill-on-close job object per child, and Ctrl+Break for a
//! graceful stop.

use std::{ffi::c_void, io, mem, ptr};

use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::{
        Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent},
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
        Threading::CREATE_NEW_PROCESS_GROUP,
    },
};

/// Children start in their own process group: the user's Ctrl+C reaches only rayfin-local (Ctrl+C
/// is disabled in a new group), which then stops them with Ctrl+Break.
pub(crate) const CREATION_FLAGS: u32 = CREATE_NEW_PROCESS_GROUP;

/// A job object that terminates its processes when its last handle closes. The handle is not
/// inheritable and only rayfin-local holds it, so the processes also die when rayfin-local dies,
/// however it dies (the OS closes the handles of a terminated process).
pub(crate) struct Job(HANDLE);

// SAFETY: a job handle is a kernel object handle, usable from any thread.
unsafe impl Send for Job {}
// SAFETY: the job APIs used here are thread-safe.
unsafe impl Sync for Job {}

impl Job {
    fn kill_on_close() -> io::Result<Self> {
        // SAFETY: null security attributes (non-inheritable handle) and a null name are allowed.
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Job(handle);
        // SAFETY: all-zero is a valid value of this plain C struct.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `info` is a valid struct of the size passed, for this information class.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast::<c_void>(),
                mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn assign(&self, process: HANDLE) -> io::Result<()> {
        // SAFETY: both handles are valid for the duration of the call.
        if unsafe { AssignProcessToJobObject(self.0, process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Terminates every process in the job.
    pub(crate) fn terminate(&self) {
        // SAFETY: the handle is valid until drop.
        unsafe { TerminateJobObject(self.0, 1) };
    }

    /// Clears kill-on-close, so closing the handle leaves the job's remaining processes running
    /// (a browser a child opened, for example).
    pub(crate) fn release(&self) {
        // SAFETY: all-zero is a valid value of this plain C struct.
        let info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
        // SAFETY: `info` is a valid struct of the size passed, for this information class.
        unsafe {
            SetInformationJobObject(
                self.0,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast::<c_void>(),
                mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle is valid and closed exactly once; kill-on-close ends the processes.
        unsafe { CloseHandle(self.0) };
    }
}

/// Puts a freshly spawned child in its own kill-on-close job. `None` (logged) when that fails;
/// the caller then falls back to `taskkill /T`.
pub(crate) fn attach_job(child: &tokio::process::Child, label: &str) -> Option<Job> {
    let handle = child.raw_handle()?;
    match Job::kill_on_close().and_then(|job| job.assign(handle as HANDLE).map(|()| job)) {
        Ok(job) => Some(job),
        Err(error) => {
            tracing::debug!(
                label,
                %error,
                "could not put the process in a job object; its process tree is stopped with taskkill"
            );
            None
        }
    }
}

/// Sends Ctrl+Break to the process group `pid` leads. `false` when it could not be delivered
/// (for example when rayfin-local has no console).
pub(crate) fn send_ctrl_break(pid: u32) -> bool {
    // SAFETY: plain FFI call with integer arguments.
    unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) != 0 }
}

/// Force-kills the process tree rooted at `pid` without a job object.
pub(crate) fn taskkill_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
