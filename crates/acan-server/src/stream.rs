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
    use std::str::FromStr;

    use acan_core::{Actor, CodeRev, Event, Seq, Stamped, Timestamp};

    use super::{Offer, Resume};

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
