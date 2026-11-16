use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::codec::{FrameError, read_frame_limited, write_frame, write_frame_limited};
use crate::types::{
    CLIENT_NAME, ConnectionState, DaemonEvent, EVENT_CHANNEL_CAPACITY, Frame, Hello, IPC_PROTOCOL,
    IpcError, MAX_FRAME_BYTES, MAX_HANDSHAKE_BYTES, Op, Reply, protocol_major,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_MIN: Duration = Duration::from_millis(200);
const RECONNECT_MAX: Duration = Duration::from_secs(5);
const MAX_PENDING_REQUESTS: usize = 256;

enum Command {
    Call {
        op: Op,
        reply: oneshot::Sender<std::result::Result<Reply, IpcError>>,
    },
    Shutdown,
}

enum Inbound {
    Response {
        id: u64,
        result: std::result::Result<Reply, IpcError>,
    },
    Event(DaemonEvent),
    Ping(u64),
    Closed(String),
}

enum SessionEnd {
    Shutdown,
    Disconnected { reason: String, was_connected: bool },
    /// The daemon refused this client for a reason another retry will not
    /// change, such as a rejected handshake or a protocol major mismatch.
    Rejected { reason: String },
}

/// A reconnecting client for the local daemon.
///
/// Dropping the handle stops the supervisor. Requests made while the daemon is
/// unreachable fail immediately with [`IpcError::not_connected`] rather than
/// hanging, which lets a frontend show "daemon offline" without a timeout.
pub struct DaemonClient {
    socket: PathBuf,
    commands: mpsc::UnboundedSender<Command>,
    events: broadcast::Sender<DaemonEvent>,
    state: watch::Receiver<ConnectionState>,
}

impl DaemonClient {
    /// Start a client for `socket` and connect in the background.
    pub fn new(socket: impl AsRef<Path>) -> Self {
        let socket = socket.as_ref().to_path_buf();
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (state_tx, state_rx) = watch::channel(ConnectionState::default());
        let supervisor_socket = socket.clone();
        tokio::spawn(supervisor(
            supervisor_socket,
            command_rx,
            events.clone(),
            state_tx,
        ));
        Self {
            socket,
            commands,
            events,
            state: state_rx,
        }
    }

    /// The socket this client was created for.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.events.subscribe()
    }

    pub fn state(&self) -> ConnectionState {
        self.state.borrow().clone()
    }

    pub fn is_connected(&self) -> bool {
        self.state.borrow().connected
    }

    /// Wait until the first handshake completes.
    ///
    /// A definitive rejection fails as soon as the supervisor records it,
    /// rather than waiting out the timeout for a reason it already knows.
    pub async fn wait_connected(&self, timeout: Duration) -> Result<ConnectionState, IpcError> {
        if self.state.borrow().connected {
            return Ok(self.state());
        }
        let mut state = self.state.clone();
        let wait = async {
            loop {
                {
                    let current = state.borrow_and_update();
                    if current.connected {
                        return Ok(current.clone());
                    }
                    if current.terminal {
                        return Err(IpcError::not_connected(
                            current
                                .last_error
                                .clone()
                                .unwrap_or_else(|| "daemon rejected the connection".to_string()),
                        ));
                    }
                }
                if state.changed().await.is_err() {
                    // The supervisor ended. Report the reason it recorded, if
                    // any, instead of a bare "client stopped".
                    let current = state.borrow();
                    return Err(IpcError::not_connected(
                        current
                            .last_error
                            .clone()
                            .unwrap_or_else(|| "daemon client stopped".to_string()),
                    ));
                }
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(result) => result,
            Err(_) => Err(IpcError::timeout(format!(
                "daemon did not become reachable within {timeout:?}"
            ))),
        }
    }

    /// Send one request and await its reply.
    pub async fn call(&self, op: Op) -> Result<Reply, IpcError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(Command::Call {
                op,
                reply: reply_tx,
            })
            .map_err(|_| IpcError::not_connected("daemon client stopped"))?;
        reply_rx
            .await
            .map_err(|_| IpcError::not_connected("daemon client stopped"))?
    }

    pub async fn call_timeout(&self, op: Op, timeout: Duration) -> Result<Reply, IpcError> {
        match tokio::time::timeout(timeout, self.call(op)).await {
            Ok(result) => result,
            Err(_) => Err(IpcError::timeout(format!(
                "daemon request timed out after {timeout:?}"
            ))),
        }
    }

    pub fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown);
    }
}

impl Drop for DaemonClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn supervisor(
    socket: PathBuf,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: broadcast::Sender<DaemonEvent>,
    state_tx: watch::Sender<ConnectionState>,
) {
    let mut backoff = RECONNECT_MIN;
    let mut attempts = 0u32;
    loop {
        // While there is no connection, answer queued calls instead of leaving
        // the caller waiting for a reply that can never arrive.
        loop {
            match commands.try_recv() {
                Ok(Command::Call { reply, .. }) => {
                    let _ = reply.send(Err(IpcError::not_connected(
                        "daemon is not reachable; is `ovvc-daemon` running?",
                    )));
                }
                Ok(Command::Shutdown) => return,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return,
            }
        }

        match connect_once(&socket, &mut commands, &events, &state_tx, attempts).await {
            SessionEnd::Shutdown => return,
            SessionEnd::Rejected { reason } => {
                let _ = state_tx.send(ConnectionState {
                    connected: false,
                    daemon: None,
                    capabilities: Vec::new(),
                    max_frame_bytes: 0,
                    last_error: Some(reason.clone()),
                    reconnect_attempts: attempts,
                    terminal: true,
                });
                let _ = events.send(DaemonEvent::Disconnected { reason });
                // The daemon's refusal will not change until one side is
                // upgraded or reconfigured, so stop retrying for this client.
                return;
            }
            SessionEnd::Disconnected {
                reason,
                was_connected,
            } => {
                if was_connected {
                    attempts = 0;
                    backoff = RECONNECT_MIN;
                } else {
                    attempts = attempts.saturating_add(1);
                }
                let _ = state_tx.send(ConnectionState {
                    connected: false,
                    daemon: None,
                    capabilities: Vec::new(),
                    max_frame_bytes: 0,
                    last_error: Some(reason.clone()),
                    reconnect_attempts: attempts,
                    terminal: false,
                });
                let _ = events.send(DaemonEvent::Disconnected { reason });
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RECONNECT_MAX);
            }
        }
    }
}

fn is_unexpected_eof(error: &FrameError) -> bool {
    matches!(
        error,
        FrameError::Io(io) if io.kind() == std::io::ErrorKind::UnexpectedEof
    )
}

async fn connect_once(
    socket: &Path,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    events: &broadcast::Sender<DaemonEvent>,
    state_tx: &watch::Sender<ConnectionState>,
    attempts: u32,
) -> SessionEnd {
    let stream = match UnixStream::connect(socket).await {
        Ok(stream) => stream,
        Err(error) => {
            return SessionEnd::Disconnected {
                reason: error.to_string(),
                was_connected: false,
            };
        }
    };
    let (mut reader, mut writer) = stream.into_split();

    // `Hello::new` advertises `CLIENT_CAPABILITIES`; the daemon replies within
    // the small pre-auth cap, so an unauthenticated peer cannot make the
    // client allocate a full-size frame before it has proved anything.
    if let Err(error) = write_frame(&mut writer, &Frame::Hello(Hello::new(CLIENT_NAME))).await {
        return SessionEnd::Disconnected {
            reason: error.to_string(),
            was_connected: false,
        };
    }
    let welcome = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        read_frame_limited(&mut reader, MAX_HANDSHAKE_BYTES),
    )
    .await
    {
        Ok(Ok(Frame::Welcome(welcome))) => welcome,
        Ok(Ok(Frame::Rejected { protocol, reason })) => {
            // The daemon understood us well enough to say why it will not
            // continue. Surface that instead of retrying blindly; the
            // connection closes when this session returns.
            return SessionEnd::Rejected {
                reason: format!("daemon rejected the handshake for protocol {protocol}: {reason}"),
            };
        }
        Ok(Ok(_)) => {
            return SessionEnd::Disconnected {
                reason: "daemon sent an unexpected handshake frame".to_string(),
                was_connected: false,
            };
        }
        Ok(Err(error)) => {
            // A daemon with a different IPC protocol closes the connection
            // instead of completing the handshake, which surfaces here as an
            // unexpected EOF. Say what that usually means instead of leaving
            // the user with "early eof".
            let reason = if is_unexpected_eof(&error) {
                format!(
                    "{} at {}; the daemon closed the handshake, likely an older build. Restart ovvc-daemon from the same build.",
                    error,
                    socket.display()
                )
            } else {
                error.to_string()
            };
            return SessionEnd::Disconnected {
                reason,
                was_connected: false,
            };
        }
        Err(_) => {
            return SessionEnd::Disconnected {
                reason: "daemon handshake timed out".to_string(),
                was_connected: false,
            };
        }
    };
    if protocol_major(&welcome.protocol) != protocol_major(IPC_PROTOCOL) {
        return SessionEnd::Rejected {
            reason: format!(
                "daemon speaks IPC protocol {:?}, this client speaks {IPC_PROTOCOL}",
                welcome.protocol
            ),
        };
    }
    if welcome.max_frame_bytes == 0 || welcome.max_frame_bytes as usize > MAX_FRAME_BYTES {
        return SessionEnd::Rejected {
            reason: format!(
                "daemon advertised an invalid max_frame_bytes {} (this client accepts 1..={MAX_FRAME_BYTES})",
                welcome.max_frame_bytes
            ),
        };
    }

    // The daemon may advertise a smaller cap than this crate's maximum. The
    // session must honour the negotiated value in both directions, or a request
    // the daemon will refuse is sent and the whole connection drops.
    let frame_cap = (welcome.max_frame_bytes as usize).min(MAX_FRAME_BYTES);

    let _ = state_tx.send(ConnectionState {
        connected: true,
        daemon: Some(welcome.clone()),
        capabilities: welcome.capabilities.clone(),
        max_frame_bytes: welcome.max_frame_bytes,
        last_error: None,
        reconnect_attempts: attempts,
        terminal: false,
    });
    let _ = events.send(DaemonEvent::Connected {
        daemon_version: welcome.daemon_version.clone(),
        author_id: welcome.author_id,
    });

    let (inbound_tx, mut inbound_rx) = mpsc::channel(256);
    tokio::spawn(reader_loop(reader, inbound_tx, frame_cap));

    let mut pending: HashMap<u64, oneshot::Sender<std::result::Result<Reply, IpcError>>> =
        HashMap::new();
    let mut next_id = 1u64;

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    None => return SessionEnd::Shutdown,
                    Some(Command::Shutdown) => {
                        let _ = write_frame(&mut writer, &Frame::Goodbye).await;
                        return SessionEnd::Shutdown;
                    }
                    Some(Command::Call { op, reply }) => {
                        if pending.len() >= MAX_PENDING_REQUESTS {
                            let _ = reply.send(Err(IpcError::internal(
                                "too many in-flight daemon requests",
                            )));
                            continue;
                        }
                        let id = next_id;
                        next_id = next_id.wrapping_add(1);
                        if let Err(error) = write_frame_limited(&mut writer, &Frame::Request { id, op }, frame_cap).await {
                            let _ = reply.send(Err(IpcError::not_connected(error.to_string())));
                            fail_pending(&mut pending, "connection lost");
                            return SessionEnd::Disconnected {
                                reason: error.to_string(),
                                was_connected: true,
                            };
                        }
                        pending.insert(id, reply);
                    }
                }
            }
            inbound = inbound_rx.recv() => {
                match inbound {
                    None => {
                        fail_pending(&mut pending, "connection lost");
                        return SessionEnd::Disconnected {
                            reason: "daemon closed the connection".to_string(),
                            was_connected: true,
                        };
                    }
                    Some(Inbound::Response { id, result }) => {
                        if let Some(reply) = pending.remove(&id) {
                            let _ = reply.send(result);
                        }
                    }
                    Some(Inbound::Event(event)) => {
                        let _ = events.send(event);
                    }
                    Some(Inbound::Ping(nonce)) => {
                        if let Err(error) = write_frame_limited(&mut writer, &Frame::Pong(nonce), frame_cap).await {
                            fail_pending(&mut pending, "connection lost");
                            return SessionEnd::Disconnected {
                                reason: error.to_string(),
                                was_connected: true,
                            };
                        }
                    }
                    Some(Inbound::Closed(reason)) => {
                        fail_pending(&mut pending, &reason);
                        return SessionEnd::Disconnected {
                            reason,
                            was_connected: true,
                        };
                    }
                }
            }
        }
    }
}

fn fail_pending(
    pending: &mut HashMap<u64, oneshot::Sender<std::result::Result<Reply, IpcError>>>,
    reason: &str,
) {
    for (_, reply) in pending.drain() {
        let _ = reply.send(Err(IpcError::not_connected(reason.to_string())));
    }
}

async fn reader_loop(mut reader: OwnedReadHalf, inbound: mpsc::Sender<Inbound>, max_frame_bytes: usize) {
    loop {
        match read_frame_limited(&mut reader, max_frame_bytes).await {
            Ok(Frame::Response { id, result }) => {
                if inbound
                    .send(Inbound::Response { id, result })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Ok(Frame::Event(event)) => {
                if inbound.send(Inbound::Event(event)).await.is_err() {
                    return;
                }
            }
            Ok(Frame::Ping(nonce)) => {
                if inbound.send(Inbound::Ping(nonce)).await.is_err() {
                    return;
                }
            }
            Ok(Frame::Pong(_)) => {}
            Ok(Frame::Goodbye) => {
                let _ = inbound
                    .send(Inbound::Closed("daemon closed the connection".to_string()))
                    .await;
                return;
            }
            Ok(_) => {
                // Requests/welcome are not valid server-to-client frames.
                let _ = inbound
                    .send(Inbound::Closed(
                        "daemon sent an unexpected frame".to_string(),
                    ))
                    .await;
                return;
            }
            Err(error) => {
                let _ = inbound.send(Inbound::Closed(error.to_string())).await;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{read_frame, write_frame};
    use crate::types::{
        DaemonEvent, ErrorKind, Frame, IPC_PROTOCOL, MAX_FRAME_BYTES, Op, Reply, Welcome,
    };
    use ovvc_core::AuthorId;
    use tokio::net::UnixListener;

    fn welcome(protocol: &str) -> Welcome {
        let mut welcome = Welcome::for_daemon(
            "0.1.0",
            AuthorId([7u8; 32]),
            "00",
            None,
            "/tmp/ovvc",
            "/tmp/ovvc/daemon.sock",
        );
        welcome.protocol = protocol.to_string();
        welcome
    }

    #[tokio::test]
    async fn requests_fail_immediately_while_disconnected() {
        let dir = tempfile::tempdir().unwrap();
        let client = DaemonClient::new(dir.path().join("missing.sock"));
        let error = tokio::time::timeout(Duration::from_secs(2), client.call(Op::Identity))
            .await
            .expect("call must not hang while disconnected")
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::NotConnected);
        assert!(!client.is_connected());
        assert!(
            tokio::time::timeout(
                Duration::from_millis(300),
                client.wait_connected(Duration::from_millis(100))
            )
            .await
            .unwrap()
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_version_mismatch_is_reported_instead_of_connecting() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = stream.into_split();
            // Read the client's Hello, then answer as a newer daemon.
            let _ = read_frame(&mut reader).await.unwrap();
            write_frame(&mut writer, &Frame::Welcome(welcome("6.0")))
                .await
                .unwrap();
            // Hold the connection open until the client gives up.
            let _ = read_frame(&mut reader).await;
        });

        let client = DaemonClient::new(&socket);
        // The supervisor retries quickly; poll for the mismatch before a later
        // "connection refused" overwrites it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut state = client.state();
        while tokio::time::Instant::now() < deadline
            && !state
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("IPC protocol"))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
            state = client.state();
        }
        assert!(!state.connected);
        assert!(
            state
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("IPC protocol")),
            "unexpected state: {state:?}"
        );
        drop(client);
        server.abort();
    }

    #[tokio::test]
    async fn a_rejected_handshake_is_surfaced() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            // Answer several retries so a later "connection refused" does not
            // overwrite the rejection before the test sees it.
            for _ in 0..8 {
                let (stream, _) = listener.accept().await.unwrap();
                let (mut reader, mut writer) = stream.into_split();
                let _ = read_frame(&mut reader).await;
                let _ = write_frame(
                    &mut writer,
                    &Frame::Rejected {
                        protocol: IPC_PROTOCOL.to_string(),
                        reason: "missing capability: policy".to_string(),
                    },
                )
                .await;
            }
        });

        let client = DaemonClient::new(&socket);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut state = client.state();
        while tokio::time::Instant::now() < deadline
            && !state
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("rejected the handshake"))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
            state = client.state();
        }
        assert!(!state.connected);
        let error = state.last_error.expect("a rejection error");
        assert!(
            error.contains("missing capability: policy"),
            "the daemon's reason must reach the user: {error}"
        );
        drop(client);
        server.abort();
    }

    #[tokio::test]
    async fn an_invalid_frame_cap_is_reported_instead_of_connecting() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = stream.into_split();
            let _ = read_frame(&mut reader).await.unwrap();
            let mut welcome = welcome(IPC_PROTOCOL);
            welcome.max_frame_bytes = 0;
            write_frame(&mut writer, &Frame::Welcome(welcome))
                .await
                .unwrap();
            let _ = read_frame(&mut reader).await;
        });

        let client = DaemonClient::new(&socket);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut state = client.state();
        while tokio::time::Instant::now() < deadline
            && !state
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("max_frame_bytes"))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
            state = client.state();
        }
        assert!(!state.connected);
        assert!(
            state
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("max_frame_bytes")),
            "unexpected state: {state:?}"
        );
        drop(client);
        server.abort();
    }

    #[tokio::test]
    async fn a_rejection_fails_wait_connected_without_waiting_for_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = stream.into_split();
            let _ = read_frame(&mut reader).await;
            let _ = write_frame(
                &mut writer,
                &Frame::Rejected {
                    protocol: IPC_PROTOCOL.to_string(),
                    reason: "missing capability: policy".to_string(),
                },
            )
            .await;
            let _ = read_frame(&mut reader).await;
        });

        let client = DaemonClient::new(&socket);
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            client.wait_connected(Duration::from_secs(10)),
        )
        .await
        .expect("a rejection must not wait for the 10 second default timeout")
        .unwrap_err();
        assert!(
            error.message.contains("rejected the handshake"),
            "the daemon's reason must reach the caller: {error:?}"
        );
        assert!(client.state().terminal, "a rejection is terminal");
        drop(client);
        server.abort();
    }

    #[tokio::test]
    async fn a_smaller_advertised_frame_cap_limits_requests() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = stream.into_split();
            let _ = read_frame(&mut reader).await.unwrap();
            let mut welcome = welcome(IPC_PROTOCOL);
            welcome.max_frame_bytes = 256;
            write_frame(&mut writer, &Frame::Welcome(welcome))
                .await
                .unwrap();
            let id = match read_frame(&mut reader).await.unwrap() {
                Frame::Request { id, op } => {
                    assert_eq!(op, Op::Identity);
                    id
                }
                other => panic!("expected Request, got {other:?}"),
            };
            write_frame(
                &mut writer,
                &Frame::Response {
                    id,
                    result: Ok(Reply::Unit),
                },
            )
            .await
            .unwrap();
            // The oversize PutBlob must never arrive: the client refuses it at
            // the negotiated cap rather than writing a frame the daemon would
            // reject, which would drop every request in flight.
            let next = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut reader)).await;
            assert!(
                matches!(next, Err(_) | Ok(Err(_))),
                "the client sent a frame above the negotiated cap"
            );
        });

        let client = DaemonClient::new(&socket);
        client
            .wait_connected(Duration::from_secs(2))
            .await
            .expect("connected");
        assert_eq!(client.state().max_frame_bytes, 256);
        assert_eq!(client.call(Op::Identity).await.unwrap(), Reply::Unit);

        let error = tokio::time::timeout(
            Duration::from_secs(1),
            client.call(Op::PutBlob {
                bytes: vec![0u8; 4096],
            }),
        )
        .await
        .expect("the oversize request must fail locally, not wait for a reply")
        .unwrap_err();
        assert!(
            error.message.contains("too large"),
            "expected a local frame-size refusal, got {error:?}"
        );
        drop(client);
        server.abort();
    }

    #[tokio::test]
    async fn requests_and_events_flow_and_slow_calls_time_out() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (event_gate, wait_for_event) = tokio::sync::oneshot::channel();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, mut writer) = stream.into_split();
            match read_frame(&mut reader).await.unwrap() {
                Frame::Hello(_) => {}
                other => panic!("expected Hello, got {other:?}"),
            }
            write_frame(&mut writer, &Frame::Welcome(welcome(IPC_PROTOCOL)))
                .await
                .unwrap();
            // One successful request.
            let id = match read_frame(&mut reader).await.unwrap() {
                Frame::Request { id, op } => {
                    assert_eq!(op, Op::Identity);
                    id
                }
                other => panic!("expected Request, got {other:?}"),
            };
            write_frame(
                &mut writer,
                &Frame::Response {
                    id,
                    result: Ok(Reply::Unit),
                },
            )
            .await
            .unwrap();
            let _ = wait_for_event.await;
            write_frame(
                &mut writer,
                &Frame::Event(DaemonEvent::Disconnected {
                    reason: "test event".to_string(),
                }),
            )
            .await
            .unwrap();
            // Consume the next request but never answer it, so the client's
            // call timeout is what ends the wait.
            let _ = read_frame(&mut reader).await;
            let _ = read_frame(&mut reader).await;
        });

        let client = DaemonClient::new(&socket);
        let mut events = client.subscribe();
        client
            .wait_connected(Duration::from_secs(2))
            .await
            .expect("connected");
        assert!(client.is_connected());
        let state = client.state();
        let welcome = state.daemon.expect("welcome");
        assert_eq!(welcome.protocol, IPC_PROTOCOL);
        assert_eq!(state.max_frame_bytes, MAX_FRAME_BYTES as u32);
        assert_eq!(state.capabilities, welcome.capabilities);
        assert!(!state.capabilities.is_empty());

        assert_eq!(client.call(Op::Identity).await.unwrap(), Reply::Unit);
        event_gate.send(()).unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.expect("event channel open");
                if matches!(event, DaemonEvent::Disconnected { .. }) {
                    break event;
                }
            }
        })
        .await
        .expect("a disconnect event");
        assert!(matches!(event, DaemonEvent::Disconnected { .. }));

        let error = client
            .call_timeout(Op::Identity, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Timeout);

        drop(client);
        server.abort();
    }
}
