use std::collections::VecDeque;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use serde::Serialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::time::{self, Instant};

use crate::process::Process;
use crate::protocol::SettingsSnapshot;
use crate::{
    AskUserReply, DroidError, DroidEvent, DroidVersion, InboundMessage, OutboundRequest,
    OutboundResponse, PermissionReply, RequestId, RpcError, SessionSettings, SpawnConfig,
};

const EXIT_GRACE: Duration = Duration::from_secs(1);
const MAX_STARTUP_EVENTS: usize = 256;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
struct FrameReader<R> {
    reader: BufReader<R>,
    partial: Vec<u8>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    // Keep the partial frame across awaits so selecting on child exit can't discard bytes.
    async fn next(&mut self) -> Result<Option<InboundMessage>, DroidError> {
        loop {
            let chunk = self.reader.fill_buf().await?;
            let newline = chunk.iter().position(|&byte| byte == b'\n');
            let end = newline.unwrap_or(chunk.len());
            let len = self.partial.len() + end;
            if len > MAX_FRAME_BYTES {
                return Err(DroidError::Protocol("frame exceeds size limit"));
            }
            if len > self.partial.capacity() {
                let capacity = self.partial.capacity().saturating_mul(2);
                let capacity = capacity.max(len).min(MAX_FRAME_BYTES);
                self.partial.reserve_exact(capacity - self.partial.len());
            }
            self.partial.extend_from_slice(&chunk[..end]);
            let eof = chunk.is_empty();
            self.reader.consume(end + usize::from(newline.is_some()));
            if newline.is_none() && !eof {
                continue;
            }
            if self.partial.iter().all(u8::is_ascii_whitespace) {
                self.partial.clear();
                if eof {
                    return Ok(None);
                }
                continue;
            }
            let message = InboundMessage::from_json(&self.partial);
            self.partial.clear();
            return message.map(Some);
        }
    }
}

/// Parameters for a new worktree session.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInit<'a> {
    /// A caller-chosen identifier for this host.
    pub machine_id: &'a str,
    /// The worktree in which this session will run.
    pub cwd: &'a Path,
}

#[derive(Debug, PartialEq)]
enum Startup {
    New,
    Aborted,
    Confirmed,
}

#[derive(Debug)]
enum Turn {
    Idle,
    Running(RequestId),
    Stopping(RequestId),
    Finished,
    Stopped,
}

impl Turn {
    fn request_id(&self) -> Option<&RequestId> {
        match self {
            Self::Running(id) | Self::Stopping(id) => Some(id),
            _ => None,
        }
    }
}

/// Drives one Droid JSON-RPC session over newline-delimited streams.
///
/// Drive events with [`Self::next_event`] and answer requests explicitly. Only one turn can
/// be in flight. Complete write operations before calling another method on the session.
#[derive(Debug)]
pub struct DroidSession<R = ChildStdout, W = ChildStdin> {
    stdout: FrameReader<R>,
    stdin: Option<W>,
    process: Option<Process>,
    version: Option<DroidVersion>,
    session_id: Option<String>,
    next_id: i64,
    startup: Startup,
    turn: Turn,
    queued: VecDeque<DroidEvent>,
    reaped: Option<(ExitStatus, Instant)>,
    closed: bool,
    exit_emitted: bool,
}

impl DroidSession {
    /// Checks the CLI version, then starts a child with a cleaned environment.
    ///
    /// Call [`Self::initialize`] before sending messages. Dropping the session kills the owned
    /// process and, on Windows, its descendants.
    ///
    /// # Errors
    ///
    /// Returns an error if the executable cannot run or is outside the supported version range.
    pub async fn spawn(config: &SpawnConfig) -> Result<Self, DroidError> {
        let (mut command, version) = config.session_command().await?;
        let mut process = Process::spawn(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )?;
        let stdout = process
            .child
            .stdout
            .take()
            .ok_or(DroidError::Protocol("missing child stdout"))?;
        let stdin = process
            .child
            .stdin
            .take()
            .ok_or(DroidError::Protocol("missing child stdin"))?;
        let mut session = Self::from_streams(stdout, stdin);
        session.process = Some(process);
        session.version = Some(version);
        Ok(session)
    }
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> DroidSession<R, W> {
    /// Attaches to existing streams without starting a process or running preflight commands.
    pub fn from_streams(stdout: R, stdin: W) -> Self {
        Self {
            stdout: FrameReader {
                reader: BufReader::new(stdout),
                partial: Vec::new(),
            },
            stdin: Some(stdin),
            process: None,
            version: None,
            session_id: None,
            next_id: 1,
            startup: Startup::New,
            turn: Turn::Idle,
            queued: VecDeque::new(),
            reaped: None,
            closed: false,
            exit_emitted: false,
        }
    }

    /// The CLI version detected before spawning, if the client owns a child.
    #[must_use]
    pub fn version(&self) -> Option<&DroidVersion> {
        self.version.as_ref()
    }

    /// The identifier Droid returned when initializing the session, when supplied.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Whether Droid explicitly reported successful completion of the latest turn.
    #[must_use]
    pub fn finished(&self) -> bool {
        matches!(self.turn, Turn::Finished)
    }

    /// Initializes once, then waits for Droid to acknowledge and confirm the requested settings.
    ///
    /// # Errors
    ///
    /// Aborts on missing or mismatched settings, timeout, malformed frames, or stream failure.
    /// A failed or cancelled handshake can never be reused to send a turn.
    pub async fn initialize(
        &mut self,
        init: SessionInit<'_>,
        settings: &SessionSettings,
        timeout: Duration,
    ) -> Result<(), DroidError> {
        if self.startup != Startup::New {
            return Err(DroidError::AlreadyInitialized);
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(DroidError::Protocol("invalid startup timeout"))?;
        self.startup = Startup::Aborted;
        let result = self.handshake(init, settings, deadline).await;
        if result.is_ok() {
            self.startup = Startup::Confirmed;
        } else {
            // Preserve the cause of startup failure even if the peer already broke the pipe.
            let _ = self.kill().await;
        }
        result
    }

    async fn handshake(
        &mut self,
        init: SessionInit<'_>,
        settings: &SessionSettings,
        deadline: Instant,
    ) -> Result<(), DroidError> {
        let result = time::timeout_at(deadline, async {
            let init_id = self.request("droid.initialize_session", init).await?;
            loop {
                match self.read_inbound().await? {
                    Some(InboundMessage::Error { error, .. }) => return Err(rpc_failure(error)),
                    Some(InboundMessage::Response { id, result, error })
                        if id.as_ref() == Some(&init_id) =>
                    {
                        break response_result(result, error);
                    }
                    Some(message) => self.queue_startup(message)?,
                    None => break Err(DroidError::Exited("initialization")),
                }
            }
        })
        .await
        .map_err(|_| DroidError::Timeout("initialization"))??;
        self.session_id = result
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);

        time::timeout_at(deadline, async {
            let update_id = self
                .request("droid.update_session_settings", settings)
                .await?;
            let mut acknowledged = false;
            let mut confirmed = false;
            let mut matching_broadcast = false;
            while !(acknowledged && (confirmed || matching_broadcast)) {
                match self.read_inbound().await? {
                    Some(InboundMessage::Error { error, .. }) => return Err(rpc_failure(error)),
                    Some(InboundMessage::Response { id, result, error })
                        if id.as_ref() == Some(&update_id) =>
                    {
                        acknowledged = true;
                        let result = response_result(result, error)?;
                        if let Some(snapshot) = SettingsSnapshot::from_value(&result)? {
                            confirmed |= snapshot.confirms(settings)?;
                        }
                    }
                    Some(message) => {
                        if let Some(event) = Self::to_event(message)? {
                            if let DroidEvent::SettingsUpdated { request_id, raw } = &event {
                                if request_id.as_ref() == Some(&update_id) {
                                    if let Some(snapshot) = SettingsSnapshot::from_value(raw)? {
                                        confirmed |= snapshot.confirms(settings)?;
                                    }
                                } else if raw
                                    .get("requestId")
                                    .is_none_or(serde_json::Value::is_null)
                                {
                                    // Droid may broadcast the previous settings before the tagged
                                    // snapshot. A broadcast can confirm a match, not a rejection.
                                    matching_broadcast = SettingsSnapshot::from_value(raw)
                                        .ok()
                                        .flatten()
                                        .and_then(|snapshot| snapshot.confirms(settings).ok())
                                        .unwrap_or(false);
                                }
                            }
                            self.push_startup(event)?;
                        }
                    }
                    None => return Err(DroidError::Exited("settings confirmation")),
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| DroidError::Timeout("settings confirmation"))?
    }

    /// Sends a user message after initialization. The acknowledgement is handled by the event loop.
    ///
    /// # Errors
    ///
    /// Returns an error if the session is unconfirmed, a turn is active, or writing fails.
    pub async fn send_user_message(&mut self, text: &str) -> Result<(), DroidError> {
        self.require_confirmed()?;
        if self.turn.request_id().is_some() {
            return Err(DroidError::TurnInProgress);
        }
        let id = self.new_id()?;
        self.turn = Turn::Running(id.clone());
        // The stream API calls this add_user_message, not send_user_message.
        let written = self
            .write(&OutboundRequest::new(
                &id,
                "droid.add_user_message",
                json!({ "text": text }),
            ))
            .await;
        if written.is_err() {
            self.turn = Turn::Stopped;
        }
        written
    }

    /// Answers a permission request using its original ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the session is unconfirmed or writing the reply fails.
    pub async fn reply_permission(
        &mut self,
        id: &RequestId,
        reply: &PermissionReply,
    ) -> Result<(), DroidError> {
        self.require_confirmed()?;
        self.write(&OutboundResponse::new(id, reply)).await
    }

    /// Answers or declines a question request using its original ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the session is unconfirmed or writing the reply fails.
    pub async fn reply_ask_user(
        &mut self,
        id: &RequestId,
        reply: &AskUserReply,
    ) -> Result<(), DroidError> {
        self.require_confirmed()?;
        self.write(&OutboundResponse::new(id, reply)).await
    }

    /// Requests that Droid interrupt the current turn.
    ///
    /// # Errors
    ///
    /// Returns an error if the session is unconfirmed or writing the request fails.
    pub async fn interrupt(&mut self) -> Result<(), DroidError> {
        self.require_confirmed()?;
        self.mark_stopping();
        self.request("droid.interrupt_session", json!({})).await?;
        Ok(())
    }

    /// Reads an event, then reports `Exited` once when the child or stream closes.
    ///
    /// Cancelling this read does not discard partial frames.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid frames and transport failure, not for unknown notifications.
    pub async fn next_event(&mut self) -> Result<Option<DroidEvent>, DroidError> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(Some(event));
        }
        while let Some(message) = self.read_inbound().await? {
            if let Some(event) = self.handle_event(message)? {
                return Ok(Some(event));
            }
        }
        if self.exit_emitted {
            return Ok(None);
        }
        self.exit_emitted = true;
        Ok(Some(DroidEvent::Exited {
            status: self.reaped.map(|(status, _)| status),
        }))
    }

    /// Asks Droid to close, closes stdin, and waits for exit, killing the child on timeout.
    ///
    /// # Errors
    ///
    /// Returns an error if graceful shutdown fails, even if forced termination succeeds.
    pub async fn shutdown(&mut self, timeout: Duration) -> Result<(), DroidError> {
        self.mark_stopping();
        let shutdown = time::timeout(timeout, async {
            if self.startup == Startup::Confirmed && !self.closed {
                self.request("droid.close_session", json!({})).await?;
            }
            if let Some(mut stdin) = self.stdin.take() {
                stdin.shutdown().await?;
            }
            while let Some(message) = self.read_inbound().await? {
                if let Some(event) = self.handle_event(message)? {
                    self.push_startup(event)?;
                }
            }
            Ok(())
        })
        .await
        .unwrap_or(Err(DroidError::Timeout("shutdown")));
        if shutdown.is_err() {
            self.kill().await?;
        }
        shutdown
    }

    /// Stops the child (and its descendants on Windows), or closes an attached writer.
    ///
    /// # Errors
    ///
    /// Returns an error if the process or writer cannot be stopped.
    pub async fn kill(&mut self) -> Result<(), DroidError> {
        let stdin = self.stdin.take();
        if let Some(process) = &mut self.process {
            drop(stdin);
            if self.reaped.is_none() {
                self.reaped = Some((process.kill().await?, Instant::now()));
            } else {
                process.root_exited();
            }
        } else if let Some(mut stdin) = stdin {
            time::timeout(EXIT_GRACE, stdin.shutdown())
                .await
                .map_err(|_| DroidError::Timeout("writer shutdown"))??;
        }
        self.mark_closed();
        Ok(())
    }

    fn require_confirmed(&self) -> Result<(), DroidError> {
        if self.startup == Startup::Confirmed && !self.closed && self.stdin.is_some() {
            Ok(())
        } else {
            Err(DroidError::NotConfirmed)
        }
    }

    fn mark_stopping(&mut self) {
        if let Turn::Running(id) = &self.turn {
            self.turn = Turn::Stopping(id.clone());
        }
    }

    fn mark_closed(&mut self) {
        self.closed = true;
        self.stdin.take();
        if self.turn.request_id().is_some() {
            self.turn = Turn::Stopped;
        }
    }

    fn new_id(&mut self) -> Result<RequestId, DroidError> {
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or(DroidError::Protocol("request IDs exhausted"))?;
        Ok(RequestId::Number(id))
    }

    async fn request<P: Serialize>(
        &mut self,
        method: &str,
        params: P,
    ) -> Result<RequestId, DroidError> {
        let id = self.new_id()?;
        self.write(&OutboundRequest::new(&id, method, params))
            .await?;
        Ok(id)
    }

    async fn write(&mut self, value: &impl Serialize) -> Result<(), DroidError> {
        let mut frame = serde_json::to_vec(value)?;
        frame.push(b'\n');
        let stdin = self.stdin.as_mut().ok_or(DroidError::Exited("write"))?;
        stdin.write_all(&frame).await?;
        stdin.flush().await?;
        Ok(())
    }

    fn queue_startup(&mut self, message: InboundMessage) -> Result<(), DroidError> {
        if let Some(event) = Self::to_event(message)? {
            self.push_startup(event)?;
        }
        Ok(())
    }

    fn push_startup(&mut self, event: DroidEvent) -> Result<(), DroidError> {
        if self.queued.len() >= MAX_STARTUP_EVENTS {
            return Err(DroidError::Protocol("too many queued events"));
        }
        self.queued.push_back(event);
        Ok(())
    }

    fn handle_event(&mut self, message: InboundMessage) -> Result<Option<DroidEvent>, DroidError> {
        let failed_turn_request = self.turn.request_id().is_some_and(|turn| match &message {
            InboundMessage::Error { id, .. } => id.as_ref().is_none_or(|id| id == turn),
            InboundMessage::Response {
                id, error: Some(_), ..
            } => id.as_ref() == Some(turn),
            _ => false,
        });
        let event = Self::to_event(message)?;
        if failed_turn_request {
            self.turn = Turn::Stopped;
        } else {
            match &event {
                Some(DroidEvent::TurnCompleted { reason, .. })
                    if self.turn.request_id().is_some() =>
                {
                    self.turn = if matches!(self.turn, Turn::Running(_))
                        && reason.as_deref().is_none_or(|reason| reason == "completed")
                    {
                        Turn::Finished
                    } else {
                        Turn::Stopped
                    };
                }
                Some(DroidEvent::RateLimited { .. }) => self.mark_stopping(),
                _ => {}
            }
        }
        Ok(event)
    }

    fn to_event(message: InboundMessage) -> Result<Option<DroidEvent>, DroidError> {
        Ok(match message {
            InboundMessage::Request { id, method, params } => {
                Some(DroidEvent::request(id, method, params)?)
            }
            InboundMessage::Notification { method, params } => {
                Some(DroidEvent::notification(method, params))
            }
            InboundMessage::Response { id, error, .. } => {
                error.map(|error| DroidEvent::response_error(id, error))
            }
            InboundMessage::Error { id, error } => Some(DroidEvent::response_error(id, error)),
        })
    }

    async fn read_inbound(&mut self) -> Result<Option<InboundMessage>, DroidError> {
        while !self.closed {
            let frame = if let Some((_, deadline)) = self.reaped {
                // Drain final frames, but don't wait forever on an inherited stdout handle.
                time::timeout_at(deadline, self.stdout.next())
                    .await
                    .unwrap_or(Ok(None))?
            } else if let Some(process) = &mut self.process {
                tokio::select! {
                    biased;
                    frame = self.stdout.next() => frame?,
                    status = process.child.wait() => {
                        self.reaped = Some((status?, Instant::now() + EXIT_GRACE));
                        process.root_exited();
                        continue;
                    }
                }
            } else {
                self.stdout.next().await?
            };
            if frame.is_some() {
                return Ok(frame);
            }
            if let Some(process) = &mut self.process
                && self.reaped.is_none()
            {
                // A closed stdout can't carry any further events, even if the child lingers.
                self.stdin.take();
                let status =
                    if let Ok(status) = time::timeout(EXIT_GRACE, process.child.wait()).await {
                        status?
                    } else {
                        process.kill().await?
                    };
                self.reaped = Some((status, Instant::now()));
                process.root_exited();
            }
            self.mark_closed();
        }
        Ok(None)
    }
}

fn response_result(
    result: serde_json::Value,
    error: Option<RpcError>,
) -> Result<serde_json::Value, DroidError> {
    match error {
        Some(error) => Err(rpc_failure(error)),
        None => Ok(result),
    }
}

fn rpc_failure(error: RpcError) -> DroidError {
    match DroidEvent::response_error(None, error) {
        DroidEvent::RateLimited {
            message,
            retry_after,
            headroom,
        } => DroidError::RateLimited {
            message,
            retry_after,
            headroom,
        },
        DroidEvent::RequestFailed { error, .. } => DroidError::Rpc(error.code, error.message),
        _ => unreachable!("an RPC error is either a rate limit or a failed request"),
    }
}
