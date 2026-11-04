//! The live event stream: the backlog from a seq, then each accepted event as it is persisted.
//!
//! A client authenticates with an `Authorization: Bearer` header on the upgrade, or, because
//! browsers cannot set headers on a WebSocket, with a first text frame `{"token": "..."}` sent
//! within [`AUTH_FRAME_TIMEOUT`]. Nothing is sent before the token is accepted. Tokens never go
//! in the URL, where proxies and access logs would keep them. A connection whose token is revoked
//! is closed with [`CLOSE_REVOKED`] and the reason [`REVOKED_REASON`].
//!
//! Between the events, a stream sends each live transition touch as a text frame,
//! `{"type": "transition_touched", ...}`. A touch is not in the log: it has no seq, a resume never
//! replays it, and a lagging stream skips the touches it missed without closing.
//!
//! A stream also sends the project's scan. On connect it sends one `scan_status` frame with the
//! scan's status, then each step of the scan as a `scan_progress` frame. Scan frames are live
//! only, as touches are: no seq, no replay, and a lagging stream sends the status again.

use std::time::Duration;

use acan_core::{Seq, Stamped};
use acan_llm::ModelClient;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::watch;

use crate::auth::{Principal, subscribe_revocations};
use crate::host::Host;
use crate::http::{ApiError, authenticate, bearer};
use crate::redact;
use crate::scan_feed::StatusFrame;
use crate::touches::TransitionTouch;

/// How long a connection without an `Authorization` header has to send its token frame, and the
/// multiplexed stream its subscription frame.
pub(crate) const AUTH_FRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// WebSocket close code for a client that must resume from a seq. The reason gives the seq.
pub(crate) const CLOSE_RESUME: u16 = 4001;
/// WebSocket close code for a server that is shutting down.
pub(crate) const CLOSE_SHUTDOWN: u16 = 1001;
/// WebSocket close code for a missing, late, or unknown token (policy violation).
pub(crate) const CLOSE_UNAUTHORIZED: u16 = 1008;
/// WebSocket close code for a server-side failure while reading the backlog.
pub(crate) const CLOSE_INTERNAL: u16 = 1011;
/// WebSocket close code for a connection whose token was revoked, such as by an owner's rotation.
/// It is the same number as [`CLOSE_RESUME`]; the reason, [`REVOKED_REASON`], tells them apart.
pub(crate) const CLOSE_REVOKED: u16 = 4001;
/// The close reason sent with [`CLOSE_REVOKED`].
pub(crate) const REVOKED_REASON: &str = "token revoked";

#[derive(Deserialize)]
pub struct StreamQuery {
    #[serde(default)]
    from: u64,
}

/// The first frame of a stream whose upgrade carried no `Authorization` header.
#[derive(Deserialize)]
pub(crate) struct AuthFrame {
    pub(crate) token: String,
}

/// How a stream proves its token.
enum Access {
    /// The upgrade's `Authorization` header carried a valid token, of this principal. `token` is
    /// the token's text, which the connection watches for revocation.
    Granted { principal: Principal, token: String },
    /// The token must arrive in the first frame.
    FirstFrame,
}

/// Upgrades `GET /api/stream?from=<seq>`.
///
/// # Errors
///
/// Returns [`ApiError::Unauthorized`] before the upgrade when an `Authorization` header carries
/// an unknown token. Without the header, the upgrade proceeds and the first frame must
/// authenticate.
pub async fn upgrade<C: ModelClient + 'static>(
    State(host): State<Host<C>>,
    headers: HeaderMap,
    Query(query): Query<StreamQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    // Subscribed before the token is checked, so a revocation that lands after the check still
    // reaches this connection.
    let revocations = subscribe_revocations();
    let access = match bearer(&headers) {
        Some(token) => Access::Granted {
            principal: authenticate(&host, Some(token))?,
            token: token.to_owned(),
        },
        None => Access::FirstFrame,
    };
    let from = Seq::new(query.from);
    Ok(ws.on_upgrade(move |socket| async move {
        track(&host, socket, from, access, revocations);
    }))
}

/// Registers the stream so shutdown can wait for it to close, then runs it.
fn track<C: ModelClient + 'static>(
    host: &Host<C>,
    socket: WebSocket,
    from: Seq,
    access: Access,
    revocations: broadcast::Receiver<String>,
) {
    let pump = pump(host.clone(), socket, from, access, revocations);
    let mut streams = host.streams().lock().expect("stream set lock poisoned");
    // Reap closed streams so a long-running server does not keep one entry per connection.
    while streams.try_join_next().is_some() {}
    streams.spawn(pump);
}

/// Resolves once the server is shutting down, even when the signal was sent before the call.
pub(crate) async fn shutting_down(shutdown: &mut watch::Receiver<bool>) {
    // An error means the sender is gone, which is a shutdown too.
    let _ = shutdown.wait_for(|off| *off).await;
}

/// The next text frame the client sends, within [`AUTH_FRAME_TIMEOUT`]. `None` means the client
/// sent nothing in time, or left.
pub(crate) async fn next_text(socket: &mut WebSocket) -> Option<String> {
    let first_text = async {
        loop {
            match socket.recv().await {
                Some(Ok(Message::Text(text))) => return Some(text),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return None,
            }
        }
    };
    tokio::time::timeout(AUTH_FRAME_TIMEOUT, first_text)
        .await
        .ok()
        .flatten()
        .map(|text| text.to_string())
}

/// Waits for a valid token frame and returns the principal it names, with the token's text. `None`
/// means the client sent anything else, sent nothing in time, or left.
async fn first_frame_authenticates<C: ModelClient>(
    host: &Host<C>,
    socket: &mut WebSocket,
) -> Option<(Principal, String)> {
    let text = next_text(socket).await?;
    let frame = serde_json::from_str::<AuthFrame>(&text).ok()?;
    let principal = authenticate(host, Some(&frame.token)).ok()?;
    Some((principal, frame.token))
}

async fn pump<C: ModelClient>(
    host: Host<C>,
    mut socket: WebSocket,
    from: Seq,
    access: Access,
    mut revocations: broadcast::Receiver<String>,
) {
    let mut shutdown = host.shutdown_signal();
    let (principal, token) = match access {
        Access::Granted { principal, token } => (principal, token),
        Access::FirstFrame => {
            let authenticated = tokio::select! {
                () = shutting_down(&mut shutdown) => {
                    close(&mut socket, CLOSE_SHUTDOWN, "server shutting down".to_owned()).await;
                    return;
                }
                authenticated = first_frame_authenticates(&host, &mut socket) => authenticated,
            };
            let Some(authenticated) = authenticated else {
                close(&mut socket, CLOSE_UNAUTHORIZED, "unauthorized".to_owned()).await;
                return;
            };
            authenticated
        }
    };
    // A page on an authenticated stream starts the codebase's scan when the server maps on first view.
    host.note_view();
    // Cleared when the revocation channel closes, so the loop stops polling it.
    let mut watching = true;

    // Subscribe before reading the backlog. An event committed in between is then both in the
    // backlog and in the subscription, and `Resume` drops the copy that arrives second.
    let mut live = host.subscribe();
    // Touches are not in the log, so they bypass `Resume` and are not part of the backlog.
    let mut touches = host.subscribe_touches();
    let mut touches_open = true;
    // The scan is live too, and joined with its status, so the status sent below and the frames
    // after it cover the scan exactly.
    let feed = host.scan_feed();
    let (scan_status, mut scan) = feed.join();
    let mut scan_open = true;
    let backlog = match host.store().load_from(from).await {
        Ok(events) => events,
        Err(err) => {
            tracing::error!(error = ?err, "loading the event backlog failed");
            close(
                &mut socket,
                CLOSE_INTERNAL,
                "backlog unavailable".to_owned(),
            )
            .await;
            return;
        }
    };

    let mut resume = Resume::new(from);
    for event in &backlog {
        match resume.offer(event) {
            Offer::Send => {
                if !send(&mut socket, &principal, event).await {
                    return;
                }
            }
            Offer::Duplicate => {}
            Offer::Gap => {
                close_for_gap(&mut socket, &resume).await;
                return;
            }
        }
    }
    if !send_scan(&mut socket, &StatusFrame::of(&scan_status)).await {
        return;
    }

    #[cfg(test)]
    host.stream_hold().wait().await;

    loop {
        tokio::select! {
            () = shutting_down(&mut shutdown) => {
                close(&mut socket, CLOSE_SHUTDOWN, "server shutting down".to_owned()).await;
                return;
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
            event = live.recv() => match event {
                Ok(event) => match resume.offer(&event) {
                    Offer::Send => {
                        if !send(&mut socket, &principal, &event).await {
                            return;
                        }
                    }
                    Offer::Duplicate => {}
                    Offer::Gap => {
                        close_for_gap(&mut socket, &resume).await;
                        return;
                    }
                },
                Err(RecvError::Lagged(_)) => {
                    // The client fell behind the live buffer. Events were skipped on this
                    // connection, so the client must resume from its last seq, not continue.
                    let reason = format!("lagged; resume from seq {}", resume.next().get());
                    close(&mut socket, CLOSE_RESUME, reason).await;
                    return;
                }
                Err(RecvError::Closed) => return,
            },
            revoked = revocations.recv(), if watching => match revoked {
                Ok(revoked) if revoked == token => {
                    close(&mut socket, CLOSE_REVOKED, REVOKED_REASON.to_owned()).await;
                    return;
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {
                    // Revocations were skipped, so this connection's own token is checked
                    // directly rather than trusting the channel to have said everything.
                    if host.tokens().principal(&token).is_none() {
                        close(&mut socket, CLOSE_REVOKED, REVOKED_REASON.to_owned()).await;
                        return;
                    }
                }
                Err(RecvError::Closed) => watching = false,
            },
            touch = touches.recv(), if touches_open => match touch {
                Ok(touch) => {
                    if !send_touch(&mut socket, &touch).await {
                        return;
                    }
                }
                // A touch is live only, so a stream that lags skips the ones it missed and goes on.
                Err(RecvError::Lagged(missed)) => {
                    tracing::debug!(missed, "a stream skipped live transition touches");
                }
                // No sender is left, so no more touches come. The events still stream.
                Err(RecvError::Closed) => touches_open = false,
            },
            frame = scan.recv(), if scan_open => match frame {
                Ok(frame) => {
                    if !send_scan(&mut socket, &frame).await {
                        return;
                    }
                }
                // The scan's frames are live only, so the frames this stream missed are not sent
                // again. The status is, and the stream joins the scan again from it.
                Err(RecvError::Lagged(missed)) => {
                    tracing::debug!(missed, "a stream skipped live scan frames");
                    let (status, joined) = feed.join();
                    scan = joined;
                    if !send_scan(&mut socket, &StatusFrame::of(&status)).await {
                        return;
                    }
                }
                Err(RecvError::Closed) => scan_open = false,
            },
        }
    }
}

/// Sends one touch frame as text. A touch is not in the log, so it has no seq and no resume
/// bookkeeping. `false` means the socket is gone.
async fn send_touch(socket: &mut WebSocket, touch: &TransitionTouch) -> bool {
    send_frame(socket, touch, "a transition touch").await
}

/// Sends one scan frame as text. A scan frame is not in the log either. `false` means the socket
/// is gone.
async fn send_scan<T: Serialize>(socket: &mut WebSocket, frame: &T) -> bool {
    send_frame(socket, frame, "a scan frame").await
}

/// Encodes one live frame as text and sends it. A frame that does not encode is logged and
/// skipped, and the stream goes on.
async fn send_frame<T: Serialize>(socket: &mut WebSocket, frame: &T, what: &str) -> bool {
    let json = match serde_json::to_string(frame) {
        Ok(json) => json,
        Err(err) => {
            tracing::error!(error = %err, "encoding {what} for the stream failed");
            return true;
        }
    };
    socket.send(Message::Text(json.into())).await.is_ok()
}

async fn close_for_gap(socket: &mut WebSocket, resume: &Resume) {
    let reason = format!("gap; resume from seq {}", resume.next().get());
    close(socket, CLOSE_RESUME, reason).await;
}

async fn send(socket: &mut WebSocket, principal: &Principal, event: &Stamped) -> bool {
    let encoded = redact::view(principal, event)
        .and_then(|view| serde_json::to_string(&view).map_err(anyhow::Error::from));
    let json = match encoded {
        Ok(json) => json,
        Err(err) => {
            tracing::error!(error = %err, "encoding an event for the stream failed");
            return false;
        }
    };
    socket.send(Message::Text(json.into())).await.is_ok()
}

pub(crate) async fn close(socket: &mut WebSocket, code: u16, reason: String) {
    let frame = CloseFrame {
        code,
        reason: reason.into(),
    };
    // The socket may already be gone, and there is nothing useful to do about that.
    let _ = socket.send(Message::Close(Some(frame))).await;
}

/// A gate a test can hold at one point of the server: each stream after its backlog, so a test
/// can fall a stream behind the live buffer without depending on socket buffer sizes, and each
/// ruling after its checks and before the writer decides it.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct Hold {
    held: std::sync::Arc<watch::Sender<bool>>,
    parked: std::sync::Arc<watch::Sender<usize>>,
}

#[cfg(test)]
impl Default for Hold {
    fn default() -> Self {
        Self {
            held: std::sync::Arc::new(watch::Sender::new(false)),
            parked: std::sync::Arc::new(watch::Sender::new(0)),
        }
    }
}

#[cfg(test)]
impl Hold {
    /// Makes every request that reaches the gate wait there.
    pub(crate) fn hold(&self) {
        self.held.send_replace(true);
    }

    /// Lets waiting and later streams through.
    pub(crate) fn release(&self) {
        self.held.send_replace(false);
    }

    /// Resolves once `count` requests have reached the gate since the server started.
    pub(crate) async fn parked(&self, count: usize) {
        let _ = self
            .parked
            .subscribe()
            .wait_for(|parked| *parked >= count)
            .await;
    }

    pub(crate) async fn wait(&self) {
        self.parked.send_modify(|parked| *parked += 1);
        let _ = self.held.subscribe().wait_for(|held| !*held).await;
    }
}

/// What to do with one event on a stream.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Offer {
    /// The next event in order. Send it.
    Send,
    /// An event the client already has.
    Duplicate,
    /// An event past the next one. Something was missed, so the client must resume.
    Gap,
}

/// The next seq a stream owes its client.
#[derive(Debug)]
pub(crate) struct Resume {
    next: Seq,
}

impl Resume {
    pub(crate) fn new(from: Seq) -> Self {
        Self { next: from }
    }

    pub(crate) fn next(&self) -> Seq {
        self.next
    }

    /// Classifies `event`. On `Send` the cursor moves past it.
    pub(crate) fn offer(&mut self, event: &Stamped) -> Offer {
        match event.seq.cmp(&self.next) {
            std::cmp::Ordering::Less => Offer::Duplicate,
            std::cmp::Ordering::Greater => Offer::Gap,
            std::cmp::Ordering::Equal => {
                self.next = Seq::new(self.next.get() + 1);
                Offer::Send
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::time::Duration;

    use acan_core::{
        Actor, CodeRev, Event, HumanCmd, MemberId, Role as MemberRole, Seq, Stamped, Timestamp,
    };
    use acan_llm::{Stub, UsageLedger};
    use futures_util::StreamExt;
    use serde_json::Value;
    use tokio::net::TcpStream;
    use tokio::time::timeout;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

    use super::{CLOSE_RESUME, Offer, Resume};
    use crate::{DEFAULT_WITNESS_CONCURRENCY, LIVE_BUFFER, Running, ServerConfig, start};

    type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

    /// How long the stream test waits for the server before it gives up.
    const WAIT: Duration = Duration::from_secs(10);

    async fn boot(repo: &std::path::Path) -> Running {
        std::fs::create_dir_all(repo.join("model")).expect("model dir");
        std::fs::write(
            repo.join("model/store.ts"),
            "export function load() {\n  return 1;\n}\n",
        )
        .expect("write fixture");
        let config = ServerConfig {
            repo: repo.to_path_buf(),
            addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            team: false,
            web: None,
            protected_branch: None,
            witness_http_base_url: None,
            witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
            vision: None,
        };
        start(config, Stub::default(), UsageLedger::default())
            .await
            .expect("the server starts")
    }

    async fn connect(running: &Running, token: &str) -> Socket {
        let mut request = format!("ws://{}/api/stream?from=0", running.local_addr)
            .into_client_request()
            .expect("stream URL");
        let value = HeaderValue::from_str(&format!("Bearer {token}")).expect("header value");
        request.headers_mut().insert(AUTHORIZATION, value);
        let (socket, _response) = tokio_tungstenite::connect_async(request)
            .await
            .expect("the stream connects");
        socket
    }

    /// Reads events until the server closes the stream. Returns the events and the close frame.
    /// A live frame, such as a scan frame, has a `type` and is not an event, so it is skipped.
    async fn drain(socket: &mut Socket) -> (Vec<Stamped>, u16, String) {
        let mut events = Vec::new();
        loop {
            let message = timeout(WAIT, socket.next())
                .await
                .expect("the stream went quiet without closing");
            match message {
                Some(Ok(WsMessage::Text(text))) => {
                    let value: Value = serde_json::from_str(&text).expect("a JSON frame");
                    if value.get("type").is_none() {
                        events.push(
                            serde_json::from_value::<Stamped>(value).expect("a stream event"),
                        );
                    }
                }
                Some(Ok(WsMessage::Close(Some(frame)))) => {
                    return (events, u16::from(frame.code), frame.reason.to_string());
                }
                Some(Ok(_)) => {}
                Some(Err(err)) => panic!("the stream failed before a close frame: {err}"),
                None => panic!(
                    "the stream ended without a close frame after {} events",
                    events.len()
                ),
            }
        }
    }

    #[tokio::test]
    async fn a_client_that_falls_behind_is_closed_with_a_resume_reason() {
        let dir = tempfile::tempdir().expect("temp dir");
        let running = boot(dir.path()).await;
        let token = running
            .owner_token
            .as_ref()
            .expect("the first start mints the owner token")
            .as_str()
            .to_owned();
        let hold = running.stream_hold.clone();
        hold.hold();
        let mut socket = connect(&running, &token).await;
        // Parked means subscribed and done with the backlog, so every command below is live.
        timeout(WAIT, hold.parked(1))
            .await
            .expect("the stream reaches the hold");

        let http = reqwest::Client::new();
        for n in 0..=LIVE_BUFFER {
            let cmd = HumanCmd::AddMember {
                member: MemberId::from_str(&format!("member-{n}")).expect("valid member id"),
                role: MemberRole::Member,
            };
            let response = http
                .post(format!("http://{}/api/commands", running.local_addr))
                .bearer_auth(&token)
                .json(&cmd)
                .send()
                .await
                .expect("the command request is sent");
            assert!(
                response.status().is_success(),
                "flood command {n} is accepted"
            );
        }
        hold.release();

        let (events, code, reason) = drain(&mut socket).await;
        assert_eq!(
            code, CLOSE_RESUME,
            "a lagging client is closed with the resume code"
        );
        let next = events.last().map_or(0, |event| event.seq.get() + 1);
        assert_eq!(
            reason,
            format!("lagged; resume from seq {next}"),
            "the reason names the seq the client must resume from"
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.seq.get())
                .collect::<Vec<_>>(),
            (0..next).collect::<Vec<u64>>(),
            "every event before the close arrived in order, with none dropped"
        );
        running.stop().await.expect("the server stops");
    }

    /// The owner's token, which the first start minted.
    fn owner_token(running: &Running) -> String {
        running
            .owner_token
            .as_ref()
            .expect("the first start mints the owner token")
            .as_str()
            .to_owned()
    }

    /// Reads text frames, parsed as JSON, until `done` says the frames read so far end the wait.
    async fn read_until(socket: &mut Socket, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let mut frames = Vec::new();
        while !done(&frames) {
            let message = timeout(WAIT, socket.next())
                .await
                .expect("the stream went quiet before the wait ended")
                .expect("the stream stayed open")
                .expect("the stream did not fail");
            if let WsMessage::Text(text) = message {
                frames.push(serde_json::from_str(&text).expect("a JSON frame"));
            }
        }
        frames
    }

    /// Whether a frame carries the live `type` tag `kind`.
    fn is_type(frame: &Value, kind: &str) -> bool {
        frame.get("type").and_then(Value::as_str) == Some(kind)
    }

    /// Whether a frame is an `IndexUpdated` event.
    fn is_index_updated(frame: &Value) -> bool {
        frame.pointer("/event/type").and_then(Value::as_str) == Some("index_updated")
    }

    /// Adds a file to the repository at `dir`, so the next rescan finds a new revision and commits
    /// it. A rescan to the same revision commits nothing.
    fn add_file(dir: &std::path::Path) {
        std::fs::write(
            dir.join("model/save.ts"),
            "export function save() {\n  return 2;\n}\n",
        )
        .expect("write the added file");
    }

    /// Runs a rescan over HTTP and returns the events it committed, as the response shows them.
    async fn rescan(running: &Running, token: &str) -> Vec<Value> {
        reqwest::Client::new()
            .post(format!("http://{}/api/scan", running.local_addr))
            .bearer_auth(token)
            .send()
            .await
            .expect("the rescan request is sent")
            .error_for_status()
            .expect("the rescan is accepted")
            .json()
            .await
            .expect("the rescan returns its events")
    }

    /// The log from seq 0, as the owner reads it over HTTP.
    async fn log_of(running: &Running, token: &str) -> Vec<Value> {
        reqwest::Client::new()
            .get(format!("http://{}/api/events?from=0", running.local_addr))
            .bearer_auth(token)
            .send()
            .await
            .expect("the log request is sent")
            .json()
            .await
            .expect("the log is a JSON array")
    }

    /// A rescan's steps reach a connected client as live `scan_progress` frames, in order, and
    /// none of them is in the log. The log gains only the events the scan commits.
    #[tokio::test]
    async fn a_rescan_streams_its_steps_live_and_logs_only_its_commit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let running = boot(dir.path()).await;
        let token = owner_token(&running);
        let mut socket = connect(&running, &token).await;
        let backlog = read_until(&mut socket, |frames| {
            frames.iter().any(|frame| is_type(frame, "scan_status"))
        })
        .await;
        let status = backlog.last().expect("the status frame ends the backlog");
        assert_eq!(status["stage"], "done", "a mapped project's scan is done");
        let logged_before = backlog.iter().filter(|f| f.get("seq").is_some()).count();
        let indexed_before = backlog.iter().filter(|f| is_index_updated(f)).count();

        add_file(dir.path());
        let committed = rescan(&running, &token).await;
        assert!(!committed.is_empty(), "the rescan commits the added file");
        let live = read_until(&mut socket, |frames| {
            let done = frames
                .iter()
                .any(|f| is_type(f, "scan_progress") && f["stage"] == "done");
            let events = frames.iter().filter(|f| f.get("type").is_none()).count();
            done && events >= committed.len()
        })
        .await;

        let steps: Vec<&str> = live
            .iter()
            .filter(|f| is_type(f, "scan_progress"))
            .map(|f| f["stage"].as_str().expect("a step has a stage"))
            .collect();
        assert_eq!(
            steps.first(),
            Some(&"skeleton"),
            "the scan opens on the skeleton"
        );
        assert_eq!(steps.last(), Some(&"done"), "the scan ends done");
        assert!(
            steps.contains(&"states"),
            "the state machines resolve after the modules"
        );
        assert!(
            live.iter()
                .filter(|f| is_type(f, "scan_progress"))
                .any(|f| f.get("module").is_some()),
            "a module frame names the module that filled"
        );
        assert!(
            live.iter()
                .filter(|f| is_type(f, "scan_progress"))
                .all(|f| f.get("seq").is_none()),
            "a scan frame has no seq"
        );
        let streamed: Vec<&Value> = live.iter().filter(|f| f.get("type").is_none()).collect();
        let streamed_seqs: Vec<&Value> = streamed.iter().map(|e| &e["seq"]).collect();
        let committed_seqs: Vec<&Value> = committed.iter().map(|e| &e["seq"]).collect();
        assert_eq!(
            streamed_seqs, committed_seqs,
            "the stream sends the committed events in the order the rescan returned them"
        );

        let log = log_of(&running, &token).await;
        assert_eq!(
            log.len(),
            logged_before + committed.len(),
            "the log gains the committed events and no scan frame"
        );
        assert!(
            log.iter()
                .all(|e| e.get("seq").is_some() && e.get("type").is_none()),
            "every logged entry is an event"
        );
        assert_eq!(
            log.iter().filter(|e| is_index_updated(e)).count(),
            indexed_before + committed.iter().filter(|e| is_index_updated(e)).count(),
            "each scan commits its IndexUpdated once"
        );
        running.stop().await.expect("the server stops");
    }

    /// A stream that connects after a scan replays the events and sends the status. The steps of
    /// the scan are live only, so they are not replayed.
    #[tokio::test]
    async fn a_resumed_stream_gets_the_status_and_not_the_steps_of_a_past_scan() {
        let dir = tempfile::tempdir().expect("temp dir");
        let running = boot(dir.path()).await;
        let token = owner_token(&running);
        add_file(dir.path());
        let committed = rescan(&running, &token).await;
        assert!(!committed.is_empty(), "the rescan commits the added file");

        let mut socket = connect(&running, &token).await;
        let frames = read_until(&mut socket, |frames| {
            frames.iter().any(|frame| is_type(frame, "scan_status"))
        })
        .await;
        assert!(
            frames.iter().all(|frame| !is_type(frame, "scan_progress")),
            "no step of the past scan is replayed"
        );
        let status = frames.last().expect("the status frame ends the backlog");
        assert_eq!(
            status["stage"], "done",
            "the status reads done after the scan"
        );
        let events = frames.iter().filter(|f| f.get("type").is_none()).count();
        assert_eq!(
            events,
            log_of(&running, &token).await.len(),
            "the backlog holds every logged event"
        );
        running.stop().await.expect("the server stops");
    }

    fn stamped(seq: u64) -> Stamped {
        Stamped {
            seq: Seq::new(seq),
            at: Timestamp::from_millis(seq),
            actor: Actor::System,
            event: Event::IndexUpdated {
                rev: CodeRev::from_str("rev-test").expect("valid revision"),
            },
        }
    }

    #[test]
    fn resume_sends_in_order_skips_copies_and_flags_a_gap() {
        let mut resume = Resume::new(Seq::new(2));
        assert_eq!(
            resume.offer(&stamped(0)),
            Offer::Duplicate,
            "before the cursor"
        );
        assert_eq!(resume.offer(&stamped(2)), Offer::Send);
        assert_eq!(
            resume.offer(&stamped(2)),
            Offer::Duplicate,
            "the same event again"
        );
        assert_eq!(resume.offer(&stamped(3)), Offer::Send);
        assert_eq!(
            resume.offer(&stamped(5)),
            Offer::Gap,
            "seq 4 was never sent"
        );
        assert_eq!(
            resume.next(),
            Seq::new(4),
            "the gap does not move the cursor"
        );
    }
}
