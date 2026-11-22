use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{self, from_ref};

use tokio::process::{Child, Command};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, GetProcessIdOfThread, OpenThread, ResumeThread,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
};

#[derive(Debug)]
pub(super) struct Job(Option<OwnedHandle>);

impl Job {
    pub(super) fn spawn(command: &mut Command) -> io::Result<(Self, Child)> {
        let handle = new_job()?;
        // A shim can launch a descendant immediately, so it must not run before assignment.
        let child = command.creation_flags(CREATE_SUSPENDED).spawn()?;
        let process_id = child
            .id()
            .ok_or_else(|| io::Error::other("missing child ID"))?;
        let process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("missing child handle"))?;
        // SAFETY: both handles remain open and name the new job and the new suspended process.
        if unsafe { AssignProcessToJobObject(handle.as_raw_handle(), process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let job = Self(Some(handle));
        resume_initial_thread(process_id)?;
        Ok((job, child))
    }

    pub(super) fn terminate(&mut self) -> io::Result<()> {
        if let Some(handle) = self.0.take() {
            // SAFETY: the owned handle identifies this job and has termination rights.
            if unsafe { TerminateJobObject(handle.as_raw_handle(), 1) } == 0 {
                // Closing the handle still applies the kill-on-close limit.
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

fn new_job() -> io::Result<OwnedHandle> {
    // SAFETY: null attributes create a non-inheritable handle; a null name creates a private job.
    let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateJobObjectW returned a new handle that we now own.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let bytes = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
        .expect("job limits struct fits in a DWORD");
    // SAFETY: the handle is live and the initialized buffer has the size and type for this class.
    if unsafe {
        SetInformationJobObject(
            handle.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            from_ref(&limits).cast(),
            bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(handle)
}

fn resume_initial_thread(process_id: u32) -> io::Result<()> {
    // std does not expose the initial thread on stable Rust. The suspended process has only
    // that thread, which can be found by its process ID in a thread snapshot.
    // SAFETY: TH32CS_SNAPTHREAD takes no process ID, so zero is appropriate.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the snapshot call returned a new handle that we now own.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry = THREADENTRY32::default();
    let bytes = u32::try_from(size_of::<THREADENTRY32>()).expect("thread entry fits in a DWORD");
    entry.dwSize = bytes;
    // SAFETY: snapshot is a valid thread snapshot and entry is writable with dwSize initialized.
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &raw mut entry) };
    while found != 0 {
        if entry.dwSize == bytes && entry.th32OwnerProcessID == process_id {
            // SAFETY: the ID came from the snapshot; OpenThread returns a new handle or null.
            let raw = unsafe {
                OpenThread(
                    THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                    0,
                    entry.th32ThreadID,
                )
            };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: OpenThread returned a new handle that we now own.
            let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
            // SAFETY: the owned handle has query access. Check that the ID was not recycled
            // between taking the snapshot and opening the handle.
            if unsafe { GetProcessIdOfThread(thread.as_raw_handle()) } != process_id {
                return Err(io::Error::other(
                    "droid thread no longer belongs to the child",
                ));
            }
            // SAFETY: the thread handle is live and was opened with suspend/resume access.
            return match unsafe { ResumeThread(thread.as_raw_handle()) } {
                1 => Ok(()),
                u32::MAX => Err(io::Error::last_os_error()),
                _ => Err(io::Error::other(
                    "unexpected suspend count for droid process",
                )),
            };
        }
        entry.dwSize = bytes;
        // SAFETY: snapshot and entry have the same validity as for Thread32First.
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &raw mut entry) };
    }
    Err(io::Error::last_os_error())
}
