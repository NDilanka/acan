use std::io;
use std::process::{ExitStatus, Output, Stdio};

use tokio::process::{Child, Command};

#[cfg(windows)]
mod job;

#[derive(Debug)]
pub(crate) struct Process {
    #[cfg(windows)]
    job: job::Job,
    pub(crate) child: Child,
}

impl Process {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        command.kill_on_drop(true);
        #[cfg(windows)]
        {
            let (job, child) = job::Job::spawn(command)?;
            Ok(Self { job, child })
        }
        #[cfg(not(windows))]
        {
            let child = command.spawn()?;
            Ok(Self { child })
        }
    }

    pub(crate) async fn output(command: &mut Command) -> io::Result<Output> {
        let Self {
            #[cfg(windows)]
                job: _job,
            child,
        } = Self::spawn(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )?;
        // Keep the job alive across the wait, including when the wait is cancelled.
        child.wait_with_output().await
    }

    pub(crate) async fn kill(&mut self) -> io::Result<ExitStatus> {
        #[cfg(windows)]
        self.job.terminate()?;
        #[cfg(not(windows))]
        self.child.start_kill()?;
        self.child.wait().await
    }

    pub(crate) fn root_exited(&mut self) {
        #[cfg(windows)]
        if let Err(error) = self.job.terminate() {
            tracing::debug!(%error, "could not explicitly terminate the droid job");
        }
    }
}
