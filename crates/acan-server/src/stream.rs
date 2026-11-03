//! The live event stream: the backlog from a seq, then each accepted event as it is persisted.
//!
//! A client authenticates with an `Authorization: Bearer` header on the upgrade, or, because
//! browsers cannot set headers on a WebSocket, with a first text frame `{"token": "..."}` sent
//! within [`AUTH_FRAME_TIMEOUT`]. Nothing is sent before the token is accepted. Tokens never go
//! in the URL, where proxies and access logs would keep them.

use std::time::Duration;

use acan_core::{Seq, Stamped};
use acan_llm::ModelClient;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;

use crate::auth::Principal;
use crate::host::Host;
use crate::http::{ApiError, authenticate, bearer};
use crate::redact;

/// How long a connection without an `Authorization` header has to send its token frame.
const AUTH_FRAME_TIMEOUT: Duration = Duration::from_secs(3);

/// WebSocket close code for a client that must resume from a seq. The reason gives the seq.
const CLOSE_RESUME: u16 = 4001;
/// WebSocket close code for a server that is shutting down.
const CLOSE_SHUTDOWN: u16 = 1001;
/// WebSocket close code for a missing, late, or unknown token (policy violation).
const CLOSE_UNAUTHORIZED: u16 = 1008;
/// WebSocket close code for a server-side failure while reading the backlog.
const CLOSE_INTERNAL: u16 = 1011;

#[derive(Deserialize)]
pub struct StreamQuery {
    #[serde(default)]
    from: u64,
}

/// The first frame of a stream whose upgrade carried no `Authorization` header.
#[derive(Deserialize)]
struct AuthFrame {
    token: String,
}

/// How a stream proves its token.
enum Access {
    /// The upgrade's `Authorization` header carried a valid token, of this principal.
    Granted(Principal),
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
    let access = match bearer(&headers) {
        Some(token) => Access::Granted(authenticate(&host, Some(token))?),
        None => Access::FirstFrame,
    };
    let from = Seq::new(query.from);
    Ok(ws.on_upgrade(move |socket| async move { track(&host, socket, from, access) }))
}

/// Registers the stream so shutdown can wait for it to close, then runs it.
fn track<C: ModelClient + 'static>(host: &Host<C>, socket: WebSocket, from: Seq, access: Access) {
    let pump = pump(host.clone(), socket, from, access);
    let mut streams = host.streams().lock().expect("stream set lock poisoned");
    // Reap closed streams so a long-running server does not keep one entry per connection.
    while streams.try_join_next().is_some() {}
    streams.spawn(pump);
}

/// Resolves once the server is shutting down, even when the signal was sent before the call.
async fn shutting_down(shutdown: &mut watch::Receiver<bool>) {
    // An error means the sender is gone, which is a shutdown too.
    let _ = shutdown.wait_for(|off| *off).await;
}

/// Waits for a valid token frame and returns the principal it names. `None` means the client sent
/// anything else, sent nothing in time, or left.
async fn first_frame_authenticates<C: ModelClient>(
    host: &Host<C>,
    socket: &mut WebSocket,
) -> Option<Principal> {
    let first_text = async {
        loop {
            match socket.recv().await {
                Some(Ok(Message::Text(text))) => return Some(text),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return None,
            }
        }
    };
    let Ok(Some(text)) = tokio::time::timeout(AUTH_FRAME_TIMEOUT, first_text).await else {
        return None;
    };
    let frame = serde_json::from_str::<AuthFrame>(&text).ok()?;
    authenticate(host, Some(&frame.token)).ok()
}

async fn pump<C: ModelClient>(host: Host<C>, mut socket: WebSocket, from: Seq, access: Access) {
    let mut shutdown = host.shutdown_signal();
    let principal = match access {
        Access::Granted(principal) => principal,
        Access::FirstFrame => {
            let authenticated = tokio::select! {
                () = shutting_down(&mut shutdown) => {
                    close(&mut socket, CLOSE_SHUTDOWN, "server shutting down".to_owned()).await;
                    return;
                }
                authenticated = first_frame_authenticates(&host, &mut socket) => authenticated,
            };
            let Some(principal) = authenticated else {
                close(&mut socket, CLOSE_UNAUTHORIZED, "unauthorized".to_owned()).await;
                return;
            };
            principal
        }
    };

    // Subscribe before reading the backlog. An event committed in between is then both in the
    // backlog and in the subscription, and `Resume` drops the copy that arrives second.
    let mut live = host.subscribe();
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
        }
    }
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

async fn close(socket: &mut WebSocket, code: u16, reason: String) {
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
    async fn drain(socket: &mut Socket) -> (Vec<Stamped>, u16, String) {
        let mut events = Vec::new();
        loop {
            let message = timeout(WAIT, socket.next())
                .await
                .expect("the stream went quiet without closing");
            match message {
                Some(Ok(WsMessage::Text(text))) => {
                    events.push(serde_json::from_str::<Stamped>(&text).expect("a stream event"));
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
