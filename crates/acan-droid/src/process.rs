use std::io;
use std::process::{ExitStatus, Output, Stdio};

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
#[cfg(unix)]
use tokio::signal::unix::{Signal, SignalKind, signal};

#[cfg(windows)]
mod job;

#[derive(Debug)]
pub(crate) struct Process {
    #[cfg(windows)]
    job: job::Job,
    #[cfg(unix)]
    exited: Signal,
    child: Child,
}

impl Process {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        command.kill_on_drop(true);
        #[cfg(windows)]
        {
            let (job, child) = job::Job::spawn(command)?;
            Ok(Self { job, child })
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;

            let exited = signal(SignalKind::child())?;
            command.as_std_mut().process_group(0);
            let child = command.spawn()?;
            Ok(Self { exited, child })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let child = command.spawn()?;
            Ok(Self { child })
        }
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub(crate) async fn output(command: &mut Command) -> io::Result<Output> {
        let mut process = Self::spawn(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )?;
        let mut stdout = process.take_stdout().expect("stdout is piped");
        let mut stderr = process.child.stderr.take().expect("stderr is piped");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (status, _, _) = tokio::try_join!(
            process.wait(),
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
        )?;
        Ok(Output {
            status,
            stdout: out,
            stderr: err,
        })
    }

    pub(crate) async fn kill(&mut self) -> io::Result<ExitStatus> {
        #[cfg(windows)]
        self.job.terminate()?;
        #[cfg(unix)]
        self.kill_group()?;
        #[cfg(not(windows))]
        self.child.start_kill()?;
        self.child.wait().await
    }

    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        #[cfg(unix)]
        if let Some(pid) = self.group_id() {
            // Detect exit without reaping the leader: its PID cannot be reused while we
            // still need to signal the group, even if a descendant holds stdout open.
            while !exited_without_reaping(pid)? {
                self.exited.recv().await;
            }
            self.kill_group()?;
        }
        let status = self.child.wait().await?;
        #[cfg(windows)]
        if let Err(error) = self.job.terminate() {
            tracing::debug!(%error, "could not explicitly terminate the droid job");
        }
        Ok(status)
    }

    #[cfg(unix)]
    fn group_id(&self) -> Option<libc::pid_t> {
        // Tokio returns None after wait has reaped the child.
        self.child
            .id()
            .and_then(|pid| libc::pid_t::try_from(pid).ok())
            .filter(|&pid| pid > 1)
    }

    #[cfg(unix)]
    fn kill_group(&self) -> io::Result<()> {
        if let Some(pid) = self.group_id() {
            // SAFETY: spawn created a group with this unreaped child's PID as its PGID.
            if unsafe { libc::kill(-pid, libc::SIGKILL) } == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for Process {
    fn drop(&mut self) {
        if let Err(error) = self.kill_group() {
            tracing::debug!(%error, "could not explicitly terminate the droid process group");
        }
        // Tokio's Child drop then takes care of reaping the leader.
    }
}

#[cfg(unix)]
fn exited_without_reaping(pid: libc::pid_t) -> io::Result<bool> {
    loop {
        // SAFETY: a zeroed siginfo_t is valid and waitid writes to it on an exit.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: pid names our child; WNOWAIT preserves its wait status for Tokio.
        if unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } == 0
        {
            return Ok(info.si_signo != 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
