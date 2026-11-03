//! The multiplexed live stream: one WebSocket for every project the token is a member of.
//!
//! A client connects to `GET /api/stream/all` and authenticates as `/api/stream` does: an
//! `Authorization` header, or a first text frame `{"token": "..."}`. The next text frame is the
//! subscription, `{"from": {"<project>": <seq>}}`. A project missing from the map starts at seq 0,
//! and a project the token is not a member of is absent. Each event arrives as
//! `{"project": "<project>", "event": <the event as /api/p/<project>/events shows it>}`.
//!
//! Each project keeps its own cursor. A lag or gap in any project sends a control frame that
//! names it, `{"control": "resume", "project": "<project>", "from": <seq>}`, then closes with
//! 4001. The socket closes for every project, and the client reconnects with its cursors. Other
//! projects' streams are not affected until then.
//!
//! Ids repeat across projects, so a client that merges events keys them by project and id.

use std::collections::BTreeMap;
use std::sync::Arc;

use acan_core::{Seq, Stamped};
use acan_llm::ModelClient;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::auth::Principal;
use crate::host::Host;
use crate::http::bearer;
use crate::redact;
use crate::registry::{Entry, Registry};
use crate::stream::{
    AuthFrame, CLOSE_INTERNAL, CLOSE_RESUME, CLOSE_SHUTDOWN, CLOSE_UNAUTHORIZED, Offer, Resume,
    close, next_text, shutting_down,
};

/// WebSocket close code for a subscription frame the protocol does not allow.
const CLOSE_POLICY: u16 = 1008;

/// How many live items may wait between a project's forwarder and the socket's loop. Past this a
/// forwarder waits, and its project's broadcast buffer then fills and reports a lag, as a single
/// project's stream does.
const FAN_IN_BUFFER: usize = 16;

/// The subscription frame: the seq each project's stream resumes from.
#[derive(Deserialize)]
struct Subscription {
    from: BTreeMap<String, u64>,
}

/// One project a multiplexed stream serves: what its token may read, and where its client is.
struct Member<C> {
    id: String,
    host: Host<C>,
    principal: Principal,
    resume: Resume,
}

/// One item a project's forwarder hands to the socket's loop.
enum Live {
    Event(Box<Stamped>),
    Lagged,
    Closed,
}

/// Upgrades `GET /api/stream/all`.
///
/// The token is checked after the upgrade, so a token that is a member of no project gets a
/// 1008 close frame rather than a refused upgrade, as the protocol describes.
pub(crate) async fn upgrade<C: ModelClient + 'static>(
    State(projects): State<Arc<Registry<C>>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let header_token = bearer(&headers).map(str::to_owned);
    ws.on_upgrade(move |socket| async move { track(&projects, socket, header_token) })
}

/// Registers the stream so shutdown can wait for it to close, then runs it.
fn track<C: ModelClient + 'static>(
    projects: &Arc<Registry<C>>,
    socket: WebSocket,
    header_token: Option<String>,
) {
    let pump = pump(Arc::clone(projects), socket, header_token);
    let mut streams = projects.streams().lock().expect("stream set lock poisoned");
    // Reap closed streams so a long-running server does not keep one entry per connection.
    while streams.try_join_next().is_some() {}
    streams.spawn(pump);
}

async fn pump<C: ModelClient + 'static>(
    projects: Arc<Registry<C>>,
    mut socket: WebSocket,
    header_token: Option<String>,
) {
    let mut shutdown = projects.shutdown_signal();
    let token = match header_token {
        Some(token) => token,
        None => {
            let frame = tokio::select! {
                () = shutting_down(&mut shutdown) => {
                    close(&mut socket, CLOSE_SHUTDOWN, "server shutting down".to_owned()).await;
                    return;
                }
                text = next_text(&mut socket) => text,
            };
            match frame.and_then(|text| serde_json::from_str::<AuthFrame>(&text).ok()) {
                Some(frame) => frame.token,
                None => {
                    close(&mut socket, CLOSE_UNAUTHORIZED, "unauthorized".to_owned()).await;
                    return;
                }
            }
        }
    };

    let mut members = members_of(projects.entries(), &token);
    if members.is_empty() {
        close(&mut socket, CLOSE_UNAUTHORIZED, "unauthorized".to_owned()).await;
        return;
    }

    let subscription = tokio::select! {
        () = shutting_down(&mut shutdown) => {
            close(&mut socket, CLOSE_SHUTDOWN, "server shutting down".to_owned()).await;
            return;
        }
        text = next_text(&mut socket) => text,
    };
    let Some(subscription) =
        subscription.and_then(|text| serde_json::from_str::<Subscription>(&text).ok())
    else {
        close(
            &mut socket,
            CLOSE_POLICY,
            "subscription expected: {\"from\": {...}}".to_owned(),
        )
        .await;
        return;
    };
    for member in &mut members {
        let from = subscription.from.get(&member.id).copied().unwrap_or(0);
        member.resume = Resume::new(Seq::new(from));
    }

    // Subscribe to every project before reading any backlog, as the single-project stream does,
    // so an event committed in between is in the backlog and the subscription, and `Resume` drops
    // the copy that arrives second. Each project's forwarder feeds one channel.
    let (tx, mut live) = mpsc::channel::<(usize, Live)>(FAN_IN_BUFFER);
    let mut forwarders = JoinSet::new();
    for (index, member) in members.iter().enumerate() {
        forwarders.spawn(forward(index, member.host.subscribe(), tx.clone()));
    }
    drop(tx);

    for member in &mut members {
        let backlog = match member.host.store().load_from(member.resume.next()).await {
            Ok(events) => events,
            Err(err) => {
                tracing::error!(project = %member.id, error = ?err, "loading the event backlog failed");
                close(
                    &mut socket,
                    CLOSE_INTERNAL,
                    "backlog unavailable".to_owned(),
                )
                .await;
                return;
            }
        };
        for event in &backlog {
            match member.resume.offer(event) {
                Offer::Send => {
                    if !send(&mut socket, &member.id, &member.principal, event).await {
                        return;
                    }
                }
                Offer::Duplicate => {}
                Offer::Gap => {
                    close_for_resume(&mut socket, &member.id, member.resume.next(), "gap").await;
                    return;
                }
            }
        }
    }

    #[cfg(test)]
    for member in &members {
        member.host.stream_hold().wait().await;
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
            item = live.recv() => match item {
                Some((index, Live::Event(event))) => {
                    let member = &mut members[index];
                    match member.resume.offer(&event) {
                        Offer::Send => {
                            if !send(&mut socket, &member.id, &member.principal, &event).await {
                                return;
                            }
                        }
                        Offer::Duplicate => {}
                        Offer::Gap => {
                            close_for_resume(&mut socket, &member.id, member.resume.next(), "gap").await;
                            return;
                        }
                    }
                }
                Some((index, Live::Lagged)) => {
                    // The client fell behind this project's live buffer, so its events were
                    // skipped. The client resumes that project from its last seq.
                    let member = &members[index];
                    close_for_resume(&mut socket, &member.id, member.resume.next(), "lagged").await;
                    return;
                }
                // The project's writer has stopped, so its events end. The other projects keep
                // streaming until every project has ended.
                Some((_, Live::Closed)) => {}
                None => return,
            },
        }
    }
}

/// The projects `token` is a member of, each with what that token may read. The subscription
/// fills in where each one starts.
fn members_of<C: ModelClient>(entries: &[Entry<C>], token: &str) -> Vec<Member<C>> {
    entries
        .iter()
        .filter_map(|entry| {
            let principal = entry.host.tokens().principal(token)?.clone();
            Some(Member {
                id: entry.id.clone(),
                host: entry.host.clone(),
                principal,
                resume: Resume::new(Seq::new(0)),
            })
        })
        .collect()
}

/// Passes one project's live events to the socket's loop, tagged with the project's position.
/// A lag or a closed channel is the last item it sends.
async fn forward(
    index: usize,
    mut live: broadcast::Receiver<Stamped>,
    out: mpsc::Sender<(usize, Live)>,
) {
    loop {
        let item = match live.recv().await {
            Ok(event) => Live::Event(Box::new(event)),
            Err(RecvError::Lagged(_)) => Live::Lagged,
            Err(RecvError::Closed) => Live::Closed,
        };
        let last = !matches!(item, Live::Event(_));
        if out.send((index, item)).await.is_err() || last {
            return;
        }
    }
}

/// Sends one event as the stream frames it. `false` means the socket is gone.
async fn send(
    socket: &mut WebSocket,
    project: &str,
    principal: &Principal,
    event: &Stamped,
) -> bool {
    let encoded = redact::view(principal, event).and_then(|view| {
        serde_json::to_string(&json!({ "project": project, "event": view }))
            .map_err(anyhow::Error::from)
    });
    let text = match encoded {
        Ok(text) => text,
        Err(err) => {
            tracing::error!(error = %err, "encoding an event for the stream failed");
            return false;
        }
    };
    socket.send(Message::Text(text.into())).await.is_ok()
}

/// Tells the client to resume `project` from `from`, then closes the socket with the resume code.
async fn close_for_resume(socket: &mut WebSocket, project: &str, from: Seq, cause: &str) {
    let control = json!({ "control": "resume", "project": project, "from": from.get() });
    let _ = socket.send(Message::Text(control.to_string().into())).await;
    let reason = format!("{cause} in {project}; resume from seq {}", from.get());
    close(socket, CLOSE_RESUME, reason).await;
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::Duration;

    use acan_llm::{Stub, UsageLedger};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::net::TcpStream;
    use tokio::time::timeout;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

    use super::FAN_IN_BUFFER;
    use crate::projects::ProjectSet;
    use crate::stream::CLOSE_RESUME;
    use crate::{DEFAULT_WITNESS_CONCURRENCY, LIVE_BUFFER, Running, ServeConfig, start_projects};

    type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

    /// How long a test waits for the server before it gives up.
    const WAIT: Duration = Duration::from_secs(10);

    /// Starts a server over two small repositories. Returns the server and its owner token.
    async fn boot(root: &std::path::Path) -> (Running, String) {
        let repos: Vec<_> = ["alpha", "beta"]
            .iter()
            .map(|name| {
                let dir = root.join(name);
                std::fs::create_dir_all(dir.join("model")).expect("model dir");
                std::fs::write(
                    dir.join("model/store.ts"),
                    "export function load() {\n  return 1;\n}\n",
                )
                .expect("write fixture");
                dir
            })
            .collect();
        let config = ServeConfig {
            projects: ProjectSet::from_repos(&repos),
            addr: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            team: false,
            web: None,
            witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
            vision: None,
            local_workers: 0,
        };
        let clients = repos
            .iter()
            .map(|_| (Stub::default(), UsageLedger::default()))
            .collect();
        let running = start_projects(config, clients)
            .await
            .expect("the server starts");
        let token = running
            .owner_token
            .as_ref()
            .expect("the first start mints the owner token")
            .as_str()
            .to_owned();
        (running, token)
    }

    async fn connect(running: &Running, token: &str) -> Socket {
        let mut request = format!("ws://{}/api/stream/all", running.local_addr)
            .into_client_request()
            .expect("stream URL");
        let value = HeaderValue::from_str(&format!("Bearer {token}")).expect("header value");
        request.headers_mut().insert(AUTHORIZATION, value);
        let (socket, _response) = tokio_tungstenite::connect_async(request)
            .await
            .expect("the stream connects");
        socket
    }

    /// Reads frames until the server closes the stream, and returns the event frames and the
    /// close code and reason.
    async fn drain(socket: &mut Socket) -> (Vec<Value>, u16, String) {
        let mut frames = Vec::new();
        loop {
            let message = timeout(WAIT, socket.next())
                .await
                .expect("the stream went quiet without closing");
            match message {
                Some(Ok(WsMessage::Text(text))) => {
                    frames.push(serde_json::from_str(&text).expect("a JSON frame"));
                }
                Some(Ok(WsMessage::Close(Some(frame)))) => {
                    return (frames, u16::from(frame.code), frame.reason.to_string());
                }
                Some(Ok(_)) => {}
                Some(Err(err)) => panic!("the stream failed before a close frame: {err}"),
                None => panic!("the stream ended without a close frame"),
            }
        }
    }

    #[tokio::test]
    async fn a_lagging_project_closes_the_socket_with_a_resume_naming_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (running, owner) = boot(dir.path()).await;
        // Alpha is the first project, whose hold the test controls.
        let hold = running.stream_hold.clone();
        hold.hold();
        let mut socket = connect(&running, &owner).await;
        let subscription = json!({ "from": { "alpha": 0, "beta": 0 } });
        socket
            .send(WsMessage::Text(subscription.to_string().into()))
            .await
            .expect("the subscription is sent");
        // Parked means every project is subscribed and done with its backlog, so every command
        // below is live.
        timeout(WAIT, hold.parked(1))
            .await
            .expect("the stream reaches the hold");

        let http = reqwest::Client::new();
        for n in 0..(LIVE_BUFFER + FAN_IN_BUFFER + 2) {
            let cmd = acan_core::HumanCmd::AddMember {
                member: acan_core::MemberId::from_str(&format!("member-{n}"))
                    .expect("valid member id"),
                role: acan_core::Role::Member,
            };
            let response = http
                .post(format!(
                    "http://{}/api/p/alpha/commands",
                    running.local_addr
                ))
                .bearer_auth(&owner)
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

        let (frames, code, reason) = drain(&mut socket).await;
        assert_eq!(
            code, CLOSE_RESUME,
            "a lagging project closes with the resume code"
        );
        assert!(
            reason.starts_with("lagged in alpha; resume from seq "),
            "the reason names the project: {reason}"
        );
        let control = frames
            .iter()
            .find(|frame| frame.get("control").is_some())
            .expect("a resume control frame arrives before the close");
        assert_eq!(control["control"], "resume");
        assert_eq!(control["project"], "alpha", "the control frame names alpha");
        running.stop().await.expect("the server stops");
    }
}
