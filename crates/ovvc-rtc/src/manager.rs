//! One WebRTC peer connection per remote author, plus the signalling and media
//! events the daemon routes.
//!
//! The manager follows the offerer rule the rest of ovvc uses: the lower
//! [`AuthorId`] offers, the higher one answers. Every peer gets its own
//! `PeerConnection` with Opus registered and one sendrecv audio track, so the
//! same payload the interim [`MediaPacket`] path carries can be written as an
//! RTP sample and read back from the remote track.
//!
//! Nothing here is panic-driven. A peer that fails to apply a description, or
//! whose connection enters `Failed`, is reported as [`RtcEvent::Error`] and
//! removed from the map. The remaining peers keep running; `send_audio` and
//! `stats` skip any peer already marked failed while the connection state
//! settles.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use ovvc_core::AuthorId;
use ovvc_crypto::{CryptoError, MediaKeyring};
use ovvc_media::call::CallId;
use ovvc_media::packet::{MAX_MEDIA_PAYLOAD, MediaKind, MediaPacket};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::MIME_TYPE_OPUS;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::Sender;

use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceCandidateType, RTCIceServer,
    RTCIceTransportPolicy, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
    RTCSessionDescription, RTCStatsReportEntry, SettingEngineBuilder, StatsSelector,
};
use webrtc::rtp_transceiver::RtpSender;

use crate::config::RtcConfig;
use crate::error::{Result, RtcError};
use crate::signal::RtcSignal;

/// Audio is always 48 kHz in the codec the default MediaEngine registers.
const CLOCK_RATE: u32 = 48_000;

/// The Opus `fmtp` the default MediaEngine registers. Matching it exactly keeps
/// the local track codec discoverable during negotiation.
const OPUS_FMTP: &str = "minptime=10;useinbandfec=1";

/// Bound on candidates buffered for a peer that has not applied a remote
/// description yet. A normal session trickles one or two; the cap stops a
/// broken peer from growing the map without limit.
const MAX_PENDING_CANDIDATES: usize = 32;

/// UDP addresses bound by every peer connection.
///
/// The wildcard produces the LAN/interface candidates a real deployment needs.
/// Loopback is only added when [`RtcConfig::allow_loopback_candidates`] is set,
/// because advertising `127.0.0.1` in a production call lets a remote peer
/// direct the local ICE agent at services bound to this machine. Both bind to
/// an ephemeral port, so they never collide.
const UDP_BIND_ADDRS: [&str; 1] = ["0.0.0.0:0"];
const UDP_BIND_ADDRS_WITH_LOOPBACK: [&str; 2] = ["0.0.0.0:0", "127.0.0.1:0"];

/// Attempts after which the negotiation retry stops doubling.
const NEGOTIATION_MAX_DOUBLINGS: u32 = 3;

type PeerMap = HashMap<AuthorId, Arc<Peer>>;
type SharedPeers = Arc<Mutex<PeerMap>>;

/// What the manager reports to the daemon.
#[derive(Debug, Clone)]
pub enum RtcEvent {
    /// A signalling message to deliver to `to`.
    Signal { to: AuthorId, signal: RtcSignal },
    /// An Opus payload received from `from`.
    Media { from: AuthorId, packet: MediaPacket },
    /// A snapshot requested by the audio service.
    Stats(RtcStats),
    /// A peer-level failure. The affected peer is removed; other peers are
    /// unaffected.
    Error(String),
}

/// Per-peer counters and state read from the connection.
#[derive(Debug, Clone)]
pub struct RtcPeerStats {
    /// The remote author this peer connects to.
    pub author: AuthorId,
    /// The connection state, as WebRTC names it ("new", "connecting",
    /// "connected", "failed", ...).
    pub state: String,
    /// Current round-trip time in milliseconds, or 0 when ICE has not measured
    /// one yet.
    pub rtt_ms: u32,
    /// RTP packets reported lost on the inbound stream.
    pub packets_lost: i64,
    /// Whether the selected candidate pair uses a TURN relay.
    pub relayed: bool,
    /// RTP payload bytes sent.
    pub bytes_sent: u64,
    /// RTP payload bytes received.
    pub bytes_received: u64,
    /// Whether a media key is installed for this peer, so a frame to or from
    /// it is sealed end to end.
    pub e2ee: bool,
    /// The key epoch of the peer's installed media cipher, if any.
    pub key_epoch: Option<u32>,
    /// Frames from this peer dropped because they failed to decrypt, were
    /// replayed, or were from the wrong epoch.
    pub decrypt_errors: u64,
}

/// A snapshot of every live peer.
#[derive(Debug, Clone, Default)]
pub struct RtcStats {
    /// One entry per live peer, ordered by author id.
    pub peers: Vec<RtcPeerStats>,
    /// Frames dropped for decryption failure across the call, including peers
    /// that have since been removed.
    pub decrypt_errors: u64,
}

/// The state shared with a peer's event handler.
struct PeerShared {
    state: Mutex<RTCPeerConnectionState>,
    failed: AtomicBool,
    /// Frames from this peer that failed to authenticate or decrypt.
    decrypt_errors: AtomicU64,
    /// The most recent RTP payload read from this peer before decryption.
    /// When E2EE is active this is ciphertext, never plaintext.
    last_wire_payload: Mutex<Option<Vec<u8>>>,
}

impl PeerShared {
    fn new() -> Self {
        Self {
            state: Mutex::new(RTCPeerConnectionState::New),
            failed: AtomicBool::new(false),
            decrypt_errors: AtomicU64::new(0),
            last_wire_payload: Mutex::new(None),
        }
    }

    fn state(&self) -> RTCPeerConnectionState {
        self.state
            .lock()
            .map(|state| *state)
            .unwrap_or(RTCPeerConnectionState::Unspecified)
    }
}

/// The end-to-end media encryption state shared by a manager and every peer
/// connection it builds.
#[derive(Clone)]
struct MediaCrypto {
    keys: Arc<RwLock<MediaKeyring>>,
    /// Call-wide count of frames dropped for decryption failure.
    decrypt_errors: Arc<AtomicU64>,
}

/// Everything one remote author needs.
struct Peer {
    pc: Arc<dyn PeerConnection>,
    track: Arc<TrackLocalStaticSample>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    shared: Arc<PeerShared>,
    /// Negotiated Opus payload type, resolved on first use.
    payload_type: AsyncMutex<Option<u8>>,
    /// Whether a remote description has been applied. Candidates that arrive
    /// before this point are buffered.
    has_remote: AtomicBool,
}

impl Peer {
    async fn payload_type(&self) -> Option<u8> {
        let mut cached = self.payload_type.lock().await;
        if let Some(payload_type) = *cached {
            return Some(payload_type);
        }
        let parameters = self.sender.get_parameters().await.ok()?;
        let payload_type = parameters
            .rtp_parameters
            .codecs
            .iter()
            .find(|codec| {
                codec
                    .rtp_codec
                    .mime_type
                    .eq_ignore_ascii_case(MIME_TYPE_OPUS)
            })
            .map(|codec| codec.payload_type)?;
        *cached = Some(payload_type);
        Some(payload_type)
    }
}

/// Events from one `PeerConnection`.
struct PeerHandler {
    remote: AuthorId,
    call_id: CallId,
    event_tx: Sender<RtcEvent>,
    shared: Arc<PeerShared>,
    peers: Weak<Mutex<PeerMap>>,
    crypto: MediaCrypto,
}

impl PeerHandler {
    /// Remove this peer from the manager's map and close its connection.
    ///
    /// Runs from a connection-driver callback, so it spawns the close instead of
    /// awaiting it: awaiting a command to the same driver from inside one of its
    /// callbacks would deadlock.
    fn remove_and_close(&self) {
        let Some(peers) = self.peers.upgrade() else {
            return;
        };
        let removed = peers
            .lock()
            .ok()
            .and_then(|mut peers| peers.remove(&self.remote));
        if let Some(peer) = removed {
            tokio::spawn(async move {
                let _ = peer.pc.close().await;
            });
        }
    }
}

#[async_trait]
impl PeerConnectionEventHandler for PeerHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if self.shared.failed.load(Ordering::SeqCst) {
            return;
        }
        let Ok(candidate) = event.candidate.to_json() else {
            return;
        };
        if candidate.candidate.is_empty() {
            // End-of-gathering sentinel emitted by the driver.
            return;
        }
        let mid = candidate.sdp_mid.filter(|mid| !mid.is_empty());
        // Signalling must not be lost, so it waits for capacity rather than
        // using try_send. The pump drains this channel continuously.
        let _ = self
            .event_tx
            .send(RtcEvent::Signal {
                to: self.remote,
                signal: RtcSignal::Candidate {
                    candidate: candidate.candidate,
                    mid,
                },
            })
            .await;
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if let Ok(mut current) = self.shared.state.lock() {
            *current = state;
        }
        if state == RTCPeerConnectionState::Failed
            && !self.shared.failed.swap(true, Ordering::SeqCst)
        {
            let _ = self.event_tx.try_send(RtcEvent::Error(format!(
                "peer {} connection failed",
                self.remote.short()
            )));
            self.remove_and_close();
        }
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let reader = TrackReader {
            call_id: self.call_id,
            from: self.remote,
            event_tx: self.event_tx.clone(),
            shared: Arc::clone(&self.shared),
            crypto: self.crypto.clone(),
        };
        spawn_track_reader(track, reader);
    }
}

/// What became of one received RTP payload.
enum DeliverOutcome {
    /// The frame decrypted and was handed to the event channel (or dropped
    /// only because the channel was full, which is normal backpressure).
    Delivered,
    /// The frame was dropped and counted: no key, wrong epoch, replay,
    /// tamper or a payload that cannot become a `MediaPacket`.
    Failed,
    /// The event channel is closed; the reader should stop.
    Closed,
}

/// The receive path for one remote peer's media.
///
/// It owns no connection state beyond the shared peer counters, so the same
/// logic serves the live remote track and [`RtcManager::ingest_rtp_payload`].
#[derive(Clone)]
struct TrackReader {
    call_id: CallId,
    from: AuthorId,
    event_tx: Sender<RtcEvent>,
    shared: Arc<PeerShared>,
    crypto: MediaCrypto,
}

impl TrackReader {
    /// Decrypt one RTP payload and, when it authenticates, emit it.
    ///
    /// A missing key, a wrong epoch, a replay counter or a failed tag is a
    /// frame drop, not a manager failure: it is counted and nothing is
    /// emitted. Plaintext is never produced from a frame that failed to
    /// authenticate.
    fn deliver(&self, seq: u16, timestamp_ms: u64, payload: &[u8]) -> DeliverOutcome {
        if let Ok(mut last) = self.shared.last_wire_payload.lock() {
            *last = Some(payload.to_vec());
        }
        let plaintext = match self.crypto.keys.read() {
            Ok(keyring) => keyring.decrypt_from(self.from, payload),
            Err(_) => return self.drop_frame(),
        };
        let plaintext = match plaintext {
            Ok(plaintext) => plaintext,
            Err(_) => return self.drop_frame(),
        };
        let packet = match MediaPacket::new(
            MediaKind::Audio,
            self.call_id,
            self.from,
            seq,
            timestamp_ms,
            plaintext,
        ) {
            Ok(packet) => packet,
            Err(_) => return self.drop_frame(),
        };
        // Media is lossy and replaces itself: drop the frame when the daemon
        // is behind rather than growing an unbounded queue.
        match self.event_tx.try_send(RtcEvent::Media {
            from: self.from,
            packet,
        }) {
            Ok(()) | Err(TrySendError::Full(_)) => DeliverOutcome::Delivered,
            Err(TrySendError::Closed(_)) => DeliverOutcome::Closed,
        }
    }

    fn drop_frame(&self) -> DeliverOutcome {
        self.shared.decrypt_errors.fetch_add(1, Ordering::Relaxed);
        self.crypto.decrypt_errors.fetch_add(1, Ordering::Relaxed);
        DeliverOutcome::Failed
    }

    async fn run(self, track: Arc<dyn TrackRemote>) {
        while let Some(event) = track.poll().await {
            match event {
                TrackRemoteEvent::OnRtpPacket(packet) => {
                    if packet.payload.is_empty() {
                        continue;
                    }
                    // MediaPacket carries milliseconds; RTP carries 48 kHz
                    // ticks. The conversion is lossy but monotonic within the
                    // 32-bit RTP timestamp wrap.
                    let timestamp_ms =
                        u64::from(packet.header.timestamp) * 1_000 / u64::from(CLOCK_RATE);
                    match self.deliver(packet.header.sequence_number, timestamp_ms, &packet.payload)
                    {
                        DeliverOutcome::Closed => break,
                        DeliverOutcome::Delivered | DeliverOutcome::Failed => {}
                    }
                }
                TrackRemoteEvent::OnEnded => break,
                _ => {}
            }
        }
    }
}

/// Read RTP from a remote track and turn each authenticated payload into a
/// [`MediaPacket`].
fn spawn_track_reader(track: Arc<dyn TrackRemote>, reader: TrackReader) {
    tokio::spawn(reader.run(track));
}

/// How far one peer's negotiation has got, for the reconcile tick to re-drive.
struct Negotiation {
    started: Instant,
    attempts: u32,
}

impl Negotiation {
    fn timeout(&self, base: Duration) -> Duration {
        let factor = 1u32 << self.attempts.min(NEGOTIATION_MAX_DOUBLINGS);
        base.saturating_mul(factor)
    }
}

/// The WebRTC transport for one call.
pub struct RtcManager {
    local_author: AuthorId,
    call_id: CallId,
    config: RtcConfig,
    event_tx: Sender<RtcEvent>,
    peers: SharedPeers,
    /// Per-call media keys. A frame is sealed for each remote author before it
    /// is written as an RTP sample and opened again after it is read.
    keys: Arc<RwLock<MediaKeyring>>,
    /// Frames dropped for decryption failure across the whole call.
    decrypt_errors: Arc<AtomicU64>,
    /// The epoch in which a missing-key error was last reported for a peer, so
    /// one broken peer cannot flood the daemon every frame.
    missing_key_reported: Mutex<HashMap<AuthorId, u32>>,
    /// Candidates that arrived before the peer or its remote description
    /// existed. Drained when the description is applied.
    pending_candidates: HashMap<AuthorId, Vec<RTCIceCandidateInit>>,
    /// When each peer's current negotiation started and how many attempts have
    /// already been made, so a lost offer or answer is retried with backoff.
    negotiation: HashMap<AuthorId, Negotiation>,
}

impl RtcManager {
    /// Create a manager for `call_id`.
    ///
    /// The configuration is validated here, so a bad frame size or ICE URL
    /// fails before any socket is opened. A fresh media keyring is generated
    /// at epoch one; publish [`public_key`](Self::public_key) and install each
    /// peer's authenticated key with [`install_peer_key`](Self::install_peer_key)
    /// before media can flow.
    pub fn new(
        local_author: AuthorId,
        call_id: CallId,
        config: RtcConfig,
        event_tx: Sender<RtcEvent>,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            local_author,
            call_id,
            config,
            event_tx,
            peers: Arc::new(Mutex::new(PeerMap::new())),
            keys: Arc::new(RwLock::new(MediaKeyring::generate(local_author, call_id))),
            decrypt_errors: Arc::new(AtomicU64::new(0)),
            missing_key_reported: Mutex::new(HashMap::new()),
            pending_candidates: HashMap::new(),
            negotiation: HashMap::new(),
        })
    }

    /// The keyring shared with the daemon, so it can install peer keys and
    /// rotate keys without rebuilding the manager.
    pub fn keyring(&self) -> Arc<RwLock<MediaKeyring>> {
        Arc::clone(&self.keys)
    }

    /// Replace the keyring, for a daemon that constructs and owns one itself.
    ///
    /// Call this before the first peer is ensured: a receive task already
    /// spawned keeps the keyring it was built with.
    pub fn set_keyring(&mut self, keyring: Arc<RwLock<MediaKeyring>>) {
        self.keys = keyring;
    }

    /// The local public key to publish in a signed control frame.
    pub fn public_key(&self) -> Result<[u8; 32]> {
        self.keys
            .read()
            .map(|keys| keys.public_key())
            .map_err(|_| RtcError::Poisoned)
    }

    /// The current media key epoch.
    pub fn key_epoch(&self) -> Result<u32> {
        self.keys
            .read()
            .map(|keys| keys.epoch())
            .map_err(|_| RtcError::Poisoned)
    }

    /// Install a peer's public key, authenticated by its signed control frame,
    /// for the given epoch. Only the local epoch is accepted.
    pub fn install_peer_key(
        &mut self,
        author: AuthorId,
        epoch: u32,
        public: [u8; 32],
    ) -> Result<()> {
        self.keys
            .write()
            .map_err(|_| RtcError::Poisoned)?
            .install_peer(author, epoch, public)
            .map_err(|err| RtcError::Crypto(err.to_string()))
    }

    /// Forget a peer's key. Media to and from that peer stops until a new key
    /// is installed.
    pub fn remove_peer_key(&mut self, author: AuthorId) -> Result<()> {
        self.keys
            .write()
            .map_err(|_| RtcError::Poisoned)?
            .remove_peer(author);
        Ok(())
    }

    /// Move to the next epoch with a fresh local keypair. Every peer key is
    /// cleared, so publish the new [`public_key`](Self::public_key) and install
    /// each peer's new key before expecting media. Returns the new epoch.
    pub fn rotate_keyring(&mut self) -> Result<u32> {
        self.keys
            .write()
            .map(|mut keys| keys.rotate())
            .map_err(|_| RtcError::Poisoned)
    }

    /// The crypto context handed to every peer connection this manager builds.
    fn crypto(&self) -> MediaCrypto {
        MediaCrypto {
            keys: Arc::clone(&self.keys),
            decrypt_errors: Arc::clone(&self.decrypt_errors),
        }
    }

    /// Ensure a peer connection exists for `remote`.
    ///
    /// When `local_author < remote` this creates and emits an offer. When the
    /// local author is higher, it creates the connection and waits for the
    /// lower author's offer, as the offerer rule requires.
    pub async fn ensure_peer(&mut self, remote: AuthorId) -> Result<()> {
        if remote == self.local_author {
            return Err(RtcError::SelfPeer);
        }
        self.reap_failed(remote).await;
        if self.peer_opt(remote).is_some() {
            let connected = self
                .peer_opt(remote)
                .is_some_and(|peer| peer.shared.state() == RTCPeerConnectionState::Connected);
            if connected {
                // Keep a fresh, zero-attempt schedule so a later drop out of
                // `connected` is noticed and renegotiated rather than being
                // mistaken for a live peer forever.
                self.negotiation.insert(
                    remote,
                    Negotiation {
                        started: Instant::now(),
                        attempts: 0,
                    },
                );
                return Ok(());
            }
            if !self.negotiation_expired(remote) {
                return Ok(());
            }
            // The offer or answer was lost, or the peer stalled in
            // new/connecting. Drop it and build a fresh connection so a new
            // offer is sent; the attempts count backs the next retry off.
            self.recycle_peer(remote).await;
        }

        let attempts = self
            .negotiation
            .get(&remote)
            .map(|negotiation| negotiation.attempts + 1)
            .unwrap_or(0);
        let peer = build_peer(
            self.local_author,
            remote,
            self.call_id,
            &self.config,
            self.event_tx.clone(),
            Arc::downgrade(&self.peers),
            self.crypto(),
        )
        .await?;
        self.insert_peer(remote, peer)?;
        self.negotiation.insert(
            remote,
            Negotiation {
                started: Instant::now(),
                attempts,
            },
        );

        if self.local_author < remote
            && let Err(err) = self.emit_offer(remote).await
        {
            self.drop_peer(remote).await;
            return Err(err);
        }
        Ok(())
    }

    /// Whether the current negotiation has burned its timeout. A peer with no
    /// schedule is treated as expired, so a connection that lost its entry is
    /// still re-driven rather than left alone.
    fn negotiation_expired(&self, remote: AuthorId) -> bool {
        match self.negotiation.get(&remote) {
            Some(negotiation) => {
                negotiation.started.elapsed() >= negotiation.timeout(self.retry_base())
            }
            None => true,
        }
    }

    fn retry_base(&self) -> Duration {
        Duration::from_millis(self.config.negotiation_timeout_ms)
    }

    /// Restart a stalled negotiation without resetting its backoff score.
    async fn recycle_peer(&mut self, remote: AuthorId) {
        let removed = self
            .peers
            .lock()
            .ok()
            .and_then(|mut peers| peers.remove(&remote));
        self.pending_candidates.remove(&remote);
        if let Some(peer) = removed {
            peer.shared.failed.store(true, Ordering::SeqCst);
            let _ = peer.pc.close().await;
        }
    }

    /// Apply a signalling message received from `from`.
    ///
    /// Malformed signals and messages for an author this manager has no peer
    /// for are returned as errors. A message that is well-formed but cannot be
    /// applied to the peer (a corrupt SDP, for example) is reported as
    /// [`RtcEvent::Error`] and the peer is removed; the remaining peers are
    /// untouched.
    pub async fn on_signal(&mut self, from: AuthorId, signal: RtcSignal) -> Result<()> {
        signal.validate()?;
        if from == self.local_author {
            return Err(RtcError::SelfPeer);
        }
        self.reap_failed(from).await;

        match signal {
            RtcSignal::Offer { sdp } => self.on_offer(from, sdp).await,
            RtcSignal::Answer { sdp } => self.on_answer(from, sdp).await,
            RtcSignal::Candidate { candidate, mid } => {
                let init = RTCIceCandidateInit {
                    candidate,
                    sdp_mid: mid,
                    sdp_mline_index: Some(0),
                    ..Default::default()
                };
                self.on_candidate(from, init).await;
                Ok(())
            }
        }
    }

    /// Write the Opus payload to every connected peer that has negotiated a
    /// payload type.
    ///
    /// The payload is sealed for each remote author first and the ciphertext is
    /// what goes on the wire; plaintext is never written when a key is missing.
    /// A peer without a usable key is skipped and reported at most once per
    /// peer per epoch. A write failure marks that peer failed, reports it and
    /// closes it; the loop continues with the next peer, so one bad connection
    /// cannot stop the call for everyone else.
    pub async fn send_audio(&self, packet: &MediaPacket) -> Result<()> {
        let targets: Vec<AuthorId> = self
            .peer_snapshot()
            .into_iter()
            .map(|(author, _)| author)
            .collect();
        self.send_audio_to(packet, &targets).await
    }

    /// Write the Opus payload to `targets` only.
    ///
    /// A star hub uses this for each listener's own mixed frame: the payload
    /// has already been mixed for that listener, so sending it to every
    /// connected peer would deliver the wrong mix to everyone else. The
    /// payload is sealed separately for each target with that target's
    /// pairwise key. A target with no live peer is skipped.
    pub async fn send_audio_to(&self, packet: &MediaPacket, targets: &[AuthorId]) -> Result<()> {
        if packet.kind != MediaKind::Audio {
            return Err(RtcError::Packet(
                "the RTC transport carries audio packets only".to_owned(),
            ));
        }
        if packet.payload.is_empty() {
            return Err(RtcError::Packet("empty Opus payload".to_owned()));
        }
        if packet.payload.len() > MAX_MEDIA_PAYLOAD {
            return Err(RtcError::Packet(format!(
                "payload is {} bytes, at most {MAX_MEDIA_PAYLOAD} are accepted",
                packet.payload.len()
            )));
        }

        let duration = Duration::from_millis(u64::from(self.config.audio_frame_ms));
        for remote in targets {
            let Some(peer) = self.peer_opt(*remote) else {
                continue;
            };
            if peer.shared.failed.load(Ordering::SeqCst)
                || peer.shared.state() != RTCPeerConnectionState::Connected
            {
                continue;
            }
            let Some(payload_type) = peer.payload_type().await else {
                // Negotiation has not exposed the Opus payload type yet.
                continue;
            };
            // The keyring guard lives only for this call; it is never held
            // across an await.
            let ciphertext = match self.encrypt_for_peer(*remote, &packet.payload) {
                Ok(ciphertext) => ciphertext,
                Err(err) => {
                    self.report_send_key_error(*remote, err);
                    continue;
                }
            };
            let sample = Sample {
                data: Bytes::from(ciphertext),
                duration,
                ..Sample::new(Instant::now())
            };
            if let Err(err) = peer
                .track
                .write_sample(peer.ssrc, payload_type, &sample, &[])
                .await
            {
                peer.shared.failed.store(true, Ordering::SeqCst);
                self.emit_error(format!("peer {} send failed: {err}", remote.short()));
                self.remove_shared(*remote);
            }
        }
        Ok(())
    }

    /// Seal one Opus payload for `remote`, or explain why it cannot be sealed.
    fn encrypt_for_peer(
        &self,
        remote: AuthorId,
        plaintext: &[u8],
    ) -> std::result::Result<Vec<u8>, CryptoError> {
        match self.keys.read() {
            Ok(keys) => keys.encrypt_for(remote, plaintext),
            // A poisoned keyring is no better than a missing key: refuse to
            // send rather than fall back to plaintext.
            Err(_) => Err(CryptoError::NoPeerKey),
        }
    }

    /// Report a send-side key failure, rate-limited to once per peer per
    /// epoch for a missing or stale key.
    fn report_send_key_error(&self, remote: AuthorId, err: CryptoError) {
        match err {
            CryptoError::NoPeerKey | CryptoError::WrongEpoch => {
                let epoch = self.keys.read().map(|keys| keys.epoch()).unwrap_or(0);
                let first_for_epoch = self
                    .missing_key_reported
                    .lock()
                    .map(|mut reported| reported.insert(remote, epoch) != Some(epoch))
                    .unwrap_or(false);
                if !first_for_epoch {
                    return;
                }
                self.emit_error(format!(
                    "peer {} has no usable media key at epoch {epoch}; media not sent",
                    remote.short()
                ));
            }
            other => self.emit_error(format!(
                "peer {} media encryption failed: {other}",
                remote.short()
            )),
        }
    }

    /// Read the connection statistics for every live peer.
    ///
    /// `rtt_ms` comes from the selected (nominated) ICE candidate pair. Until
    /// ICE has measured one it is 0; no estimate is invented.
    pub async fn stats(&self) -> RtcStats {
        let mut peers = self.peer_snapshot();
        peers.sort_by_key(|(author, _)| *author);
        let mut out = Vec::with_capacity(peers.len());
        for (author, peer) in peers {
            if peer.shared.failed.load(Ordering::SeqCst) {
                continue;
            }
            let (e2ee, key_epoch) = self.peer_key_state(author);
            let decrypt_errors = peer.shared.decrypt_errors.load(Ordering::Relaxed);
            let mut stats = peer_stats(author, &peer).await;
            stats.e2ee = e2ee;
            stats.key_epoch = key_epoch;
            stats.decrypt_errors = decrypt_errors;
            out.push(stats);
        }
        RtcStats {
            peers: out,
            decrypt_errors: self.decrypt_errors.load(Ordering::Relaxed),
        }
    }

    /// Whether a key is installed for `author` and, if so, its epoch.
    fn peer_key_state(&self, author: AuthorId) -> (bool, Option<u32>) {
        match self.keys.read() {
            Ok(keys) => {
                let epoch = keys.peer_epoch(author);
                (epoch.is_some(), epoch)
            }
            Err(_) => (false, None),
        }
    }

    /// Feed one RTP payload through the same authenticate-and-emit path the
    /// remote track uses.
    ///
    /// This exists so the media boundary can be exercised without a live
    /// `PeerConnection`. A frame with no key, from the wrong epoch, replayed or
    /// tampered with is counted and dropped; the call still returns `Ok`.
    pub fn ingest_rtp_payload(
        &self,
        from: AuthorId,
        seq: u16,
        timestamp_ms: u64,
        payload: &[u8],
    ) -> Result<()> {
        let peer = self.peer_opt(from).ok_or(RtcError::UnknownPeer(from))?;
        let reader = TrackReader {
            call_id: self.call_id,
            from,
            event_tx: self.event_tx.clone(),
            shared: Arc::clone(&peer.shared),
            crypto: self.crypto(),
        };
        let _ = reader.deliver(seq, timestamp_ms, payload);
        Ok(())
    }

    /// The most recent RTP payload read from `remote` before decryption.
    ///
    /// When E2EE is active this is ciphertext, so it is safe to expose for
    /// diagnostics. Returns `None` when no frame has been read yet.
    pub fn last_wire_payload(&self, remote: AuthorId) -> Option<Vec<u8>> {
        self.peer_opt(remote).and_then(|peer| {
            peer.shared
                .last_wire_payload
                .lock()
                .ok()
                .and_then(|last| last.clone())
        })
    }

    /// Close and forget the peer for `remote`. Safe to call when absent.
    pub async fn remove_peer(&mut self, remote: AuthorId) {
        self.drop_peer(remote).await;
    }

    /// Close every peer. Safe to call more than once.
    pub async fn close(&mut self) {
        let peers: Vec<Arc<Peer>> = match self.peers.lock() {
            Ok(mut peers) => peers.drain().map(|(_, peer)| peer).collect(),
            Err(_) => return,
        };
        self.pending_candidates.clear();
        self.negotiation.clear();
        if let Ok(mut reported) = self.missing_key_reported.lock() {
            reported.clear();
        }
        for peer in peers {
            peer.shared.failed.store(true, Ordering::SeqCst);
            let _ = peer.pc.close().await;
        }
    }

    // --- internal helpers -------------------------------------------------

    fn insert_peer(&self, remote: AuthorId, peer: Peer) -> Result<()> {
        self.peers
            .lock()
            .map_err(|_| RtcError::Poisoned)?
            .insert(remote, Arc::new(peer));
        Ok(())
    }

    fn peer_opt(&self, remote: AuthorId) -> Option<Arc<Peer>> {
        self.peers
            .lock()
            .ok()
            .and_then(|peers| peers.get(&remote).cloned())
    }

    fn peer_snapshot(&self) -> Vec<(AuthorId, Arc<Peer>)> {
        self.peers
            .lock()
            .map(|peers| {
                peers
                    .iter()
                    .map(|(author, peer)| (*author, Arc::clone(peer)))
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn emit_offer(&self, remote: AuthorId) -> Result<()> {
        let peer = self.peer_opt(remote).ok_or(RtcError::UnknownPeer(remote))?;
        let offer = peer
            .pc
            .create_offer(None)
            .await
            .map_err(|err| RtcError::WebRtc(err.to_string()))?;
        peer.pc
            .set_local_description(offer.clone())
            .await
            .map_err(|err| RtcError::WebRtc(err.to_string()))?;
        let _ = self
            .event_tx
            .send(RtcEvent::Signal {
                to: remote,
                signal: RtcSignal::Offer { sdp: offer.sdp },
            })
            .await;
        Ok(())
    }

    async fn on_offer(&mut self, from: AuthorId, sdp: String) -> Result<()> {
        // The lower author offers. An offer arriving from a higher author is
        // glare, which this manager does not implement.
        if from > self.local_author {
            return Err(RtcError::Signal(format!(
                "unexpected offer from {}: the lower author must offer",
                from.short()
            )));
        }
        if let Err(message) = validate_remote_sdp(&sdp, &self.config) {
            self.emit_error(format!("offer from {} rejected: {message}", from.short()));
            return Err(RtcError::Signal(message));
        }

        // An offer arriving on a peer that has already seen a remote
        // description but never connected means the offerer restarted
        // negotiation. Rebuild so the fresh ICE credentials can be applied.
        let stalled = self.peer_opt(from).is_some_and(|peer| {
            peer.has_remote.load(Ordering::SeqCst)
                && peer.shared.state() != RTCPeerConnectionState::Connected
        });
        if self.peer_opt(from).is_none() || stalled {
            if stalled {
                self.recycle_peer(from).await;
            }
            let peer = build_peer(
                self.local_author,
                from,
                self.call_id,
                &self.config,
                self.event_tx.clone(),
                Arc::downgrade(&self.peers),
                self.crypto(),
            )
            .await?;
            self.insert_peer(from, peer)?;
            let attempts = self
                .negotiation
                .get(&from)
                .map(|negotiation| negotiation.attempts + 1)
                .unwrap_or(0);
            self.negotiation.insert(
                from,
                Negotiation {
                    started: Instant::now(),
                    attempts,
                },
            );
        }

        let peer = self.peer_opt(from).ok_or(RtcError::UnknownPeer(from))?;
        if peer.has_remote.load(Ordering::SeqCst) {
            // A duplicate offer on a connected peer is glare; ignore it.
            return Ok(());
        }
        let description = match RTCSessionDescription::offer(sdp) {
            Ok(description) => description,
            Err(err) => {
                return self
                    .fail_peer(from, format!("invalid offer SDP: {err}"))
                    .await;
            }
        };
        if let Err(err) = peer.pc.set_remote_description(description).await {
            return self
                .fail_peer(from, format!("failed to apply offer: {err}"))
                .await;
        }
        peer.has_remote.store(true, Ordering::SeqCst);

        let answer = match peer.pc.create_answer(None).await {
            Ok(answer) => answer,
            Err(err) => {
                return self
                    .fail_peer(from, format!("failed to create answer: {err}"))
                    .await;
            }
        };
        if let Err(err) = peer.pc.set_local_description(answer.clone()).await {
            return self
                .fail_peer(from, format!("failed to apply local answer: {err}"))
                .await;
        }
        // Buffer candidates until both descriptions exist; the ICE agent is
        // only started by the local description.
        self.drain_pending(from, &peer).await;
        self.touch_negotiation(from);
        let _ = self
            .event_tx
            .send(RtcEvent::Signal {
                to: from,
                signal: RtcSignal::Answer { sdp: answer.sdp },
            })
            .await;
        Ok(())
    }

    async fn on_answer(&mut self, from: AuthorId, sdp: String) -> Result<()> {
        if let Err(message) = validate_remote_sdp(&sdp, &self.config) {
            self.emit_error(format!("answer from {} rejected: {message}", from.short()));
            return Err(RtcError::Signal(message));
        }
        let peer = self.peer_opt(from).ok_or(RtcError::UnknownPeer(from))?;
        if peer.has_remote.load(Ordering::SeqCst) {
            return Ok(());
        }
        let description = match RTCSessionDescription::answer(sdp) {
            Ok(description) => description,
            Err(err) => {
                return self
                    .fail_peer(from, format!("invalid answer SDP: {err}"))
                    .await;
            }
        };
        if let Err(err) = peer.pc.set_remote_description(description).await {
            return self
                .fail_peer(from, format!("failed to apply answer: {err}"))
                .await;
        }
        peer.has_remote.store(true, Ordering::SeqCst);
        self.drain_pending(from, &peer).await;
        self.touch_negotiation(from);
        Ok(())
    }

    async fn on_candidate(&mut self, from: AuthorId, candidate: RTCIceCandidateInit) {
        if let Err(reason) = validate_candidate(&candidate.candidate, &self.config) {
            self.emit_error(format!("candidate from {} rejected: {reason}", from.short()));
            return;
        }
        let Some(peer) = self.peer_opt(from) else {
            self.buffer_candidate(from, candidate);
            return;
        };
        if !peer.has_remote.load(Ordering::SeqCst) {
            self.buffer_candidate(from, candidate);
            return;
        }
        // A rejected candidate is normally a stale one; it does not take the
        // peer down.
        if let Err(err) = peer.pc.add_ice_candidate(candidate).await {
            self.emit_error(format!("candidate from {} rejected: {err}", from.short()));
        }
    }

    fn buffer_candidate(&mut self, from: AuthorId, candidate: RTCIceCandidateInit) {
        let buffered = self.pending_candidates.entry(from).or_default();
        if buffered.len() < MAX_PENDING_CANDIDATES {
            buffered.push(candidate);
        }
    }

    async fn drain_pending(&mut self, from: AuthorId, peer: &Arc<Peer>) {
        let Some(candidates) = self.pending_candidates.remove(&from) else {
            return;
        };
        for candidate in candidates {
            if let Err(err) = peer.pc.add_ice_candidate(candidate).await {
                self.emit_error(format!(
                    "buffered candidate from {} rejected: {err}",
                    from.short()
                ));
            }
        }
    }

    async fn fail_peer(&mut self, remote: AuthorId, message: String) -> Result<()> {
        self.emit_error(format!("peer {}: {message}", remote.short()));
        self.drop_peer(remote).await;
        Ok(())
    }

    /// Report a peer-level failure without blocking on the event channel.
    ///
    /// Errors are already summarised or rate-limited, and the retry machinery
    /// re-drives the peer, so dropping one under extreme pressure loses a log
    /// line rather than stalling the RTC lock.
    fn emit_error(&self, message: String) {
        let _ = self.event_tx.try_send(RtcEvent::Error(message));
    }

    /// Give the current negotiation a fresh timeout after a description was
    /// applied, so a slow ICE check is not mistaken for a stranded peer.
    fn touch_negotiation(&mut self, remote: AuthorId) {
        if let Some(negotiation) = self.negotiation.get_mut(&remote) {
            negotiation.started = Instant::now();
        }
    }

    async fn reap_failed(&mut self, remote: AuthorId) {
        let failed = self
            .peer_opt(remote)
            .map(|peer| peer.shared.failed.load(Ordering::SeqCst))
            .unwrap_or(false);
        if failed {
            self.drop_peer(remote).await;
        }
    }

    async fn drop_peer(&mut self, remote: AuthorId) {
        let removed = self
            .peers
            .lock()
            .ok()
            .and_then(|mut peers| peers.remove(&remote));
        self.pending_candidates.remove(&remote);
        self.negotiation.remove(&remote);
        if let Some(peer) = removed {
            peer.shared.failed.store(true, Ordering::SeqCst);
            let _ = peer.pc.close().await;
        }
    }

    /// Remove a peer from the shared map without awaiting its close. Used from
    /// `send_audio`, which has only `&self`; the close runs in a task.
    fn remove_shared(&self, remote: AuthorId) {
        let removed = self
            .peers
            .lock()
            .ok()
            .and_then(|mut peers| peers.remove(&remote));
        if let Some(peer) = removed {
            tokio::spawn(async move {
                let _ = peer.pc.close().await;
            });
        }
    }
}

/// Validate a trickled candidate before it reaches the ICE agent.
///
/// The address is the only field that can direct the local agent at another
/// endpoint, so loopback, link-local, unspecified, multicast and (when LAN
/// candidates are disabled) private addresses are refused.
fn validate_candidate(candidate: &str, config: &RtcConfig) -> std::result::Result<(), String> {
    let Some(address) = candidate_address(candidate) else {
        return Err("candidate address is missing or not an IP literal".to_string());
    };
    if !address_allowed(address, config) {
        return Err(format!("candidate address {address} is not permitted"));
    }
    Ok(())
}

/// Pull the address field out of `candidate:<foundation> <component>
/// <transport> <priority> <address> <port> typ <type>`.
fn candidate_address(candidate: &str) -> Option<IpAddr> {
    let line = candidate.strip_prefix("a=").unwrap_or(candidate);
    let mut fields = line.split_whitespace();
    if !fields.next()?.starts_with("candidate:") {
        return None;
    }
    let _component = fields.next()?;
    let _transport = fields.next()?;
    let _priority = fields.next()?;
    fields.next()?.parse().ok()
}

/// Check one remote candidate address against the configured classes.
fn address_allowed(address: IpAddr, config: &RtcConfig) -> bool {
    match address {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                return config.allow_loopback_candidates;
            }
            if v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast() || v4.is_link_local()
            {
                return false;
            }
            if v4.is_private() {
                return config.allow_lan_candidates;
            }
            true
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return address_allowed(IpAddr::V4(v4), config);
            }
            if v6.is_loopback() {
                return config.allow_loopback_candidates;
            }
            if v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            let first = v6.segments()[0];
            if first & 0xffc0 == 0xfe80 {
                // Link-local: never a useful call endpoint.
                return false;
            }
            if first & 0xfe00 == 0xfc00 {
                // Unique local addresses behave like private IPv4.
                return config.allow_lan_candidates;
            }
            true
        }
    }
}

/// Refuse a description that embeds a candidate the ICE agent must not dial.
///
/// Trickle ICE is preferred, but a peer can still put `a=candidate:` lines in
/// its SDP, and `set_remote_description` hands those to the agent.
fn validate_remote_sdp(sdp: &str, config: &RtcConfig) -> std::result::Result<(), String> {
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(candidate) = line.strip_prefix("a=candidate:") {
            validate_candidate(&format!("candidate:{candidate}"), config)?;
        }
    }
    Ok(())
}

/// Build the `PeerConnection`, local Opus track and sender for one remote.
async fn build_peer(
    local: AuthorId,
    remote: AuthorId,
    call_id: CallId,
    config: &RtcConfig,
    event_tx: Sender<RtcEvent>,
    peers: Weak<Mutex<PeerMap>>,
    crypto: MediaCrypto,
) -> Result<Peer> {
    let mut media = MediaEngine::default();
    media
        .register_default_codecs()
        .map_err(|err| RtcError::WebRtc(err.to_string()))?;

    let settings = SettingEngineBuilder::new()
        .with_include_loopback_candidate(config.allow_loopback_candidates)
        .build();

    let mut configuration = RTCConfigurationBuilder::default();
    if !config.ice_servers.is_empty() {
        let servers = config
            .ice_servers
            .iter()
            .map(|url| RTCIceServer {
                urls: vec![url.clone()],
                // rtc validates these only for turn:/turns: URLs; a STUN
                // server ignores them. RtcConfig::validate already refused a
                // TURN URL without both values.
                username: config.turn_username.clone(),
                credential: config.turn_credential.clone(),
            })
            .collect();
        configuration = configuration.with_ice_servers(servers);
    }
    if config.force_relay {
        configuration = configuration.with_ice_transport_policy(RTCIceTransportPolicy::Relay);
    }
    let configuration = configuration.build();

    let shared = Arc::new(PeerShared::new());
    let handler = Arc::new(PeerHandler {
        remote,
        call_id,
        event_tx,
        shared: Arc::clone(&shared),
        peers,
        crypto,
    });

    let udp_addrs = if config.allow_loopback_candidates {
        UDP_BIND_ADDRS_WITH_LOOPBACK.to_vec()
    } else {
        UDP_BIND_ADDRS.to_vec()
    };
    let pc = PeerConnectionBuilder::new()
        .with_configuration(configuration)
        .with_media_engine(media)
        .with_setting_engine(settings)
        .with_handler(handler)
        .with_udp_addrs(udp_addrs)
        .build()
        .await
        .map_err(|err| RtcError::WebRtc(err.to_string()))?;
    let pc: Arc<dyn PeerConnection> = Arc::new(pc);

    let ssrc = derive_ssrc(local, remote, call_id);
    let media_track = MediaStreamTrack::new(
        format!("ovvc-stream-{}", call_id.to_hex()),
        format!("ovvc-track-{}", remote.short()),
        "ovvc-audio".to_owned(),
        RtpCodecKind::Audio,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec: opus_codec(),
            max_bitrate: u32::from(config.audio_bitrate_kbps) * 1_000,
            ..Default::default()
        }],
    );
    let track = Arc::new(
        TrackLocalStaticSample::new(Instant::now(), media_track)
            .map_err(|err| RtcError::WebRtc(err.to_string()))?,
    );
    let sender = pc
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
        .await
        .map_err(|err| RtcError::WebRtc(err.to_string()))?;

    Ok(Peer {
        pc,
        track,
        sender,
        ssrc,
        shared,
        payload_type: AsyncMutex::new(None),
        has_remote: AtomicBool::new(false),
    })
}

/// Read one peer's stats into the public shape.
async fn peer_stats(author: AuthorId, peer: &Peer) -> RtcPeerStats {
    let state = peer.shared.state().to_string();
    let report = peer.pc.get_stats(Instant::now(), StatsSelector::None).await;

    let mut relayed_local: HashMap<String, bool> = HashMap::new();
    for entry in report.iter() {
        if let RTCStatsReportEntry::LocalCandidate(candidate) = entry {
            relayed_local.insert(
                candidate.stats.id.clone(),
                matches!(candidate.candidate_type, RTCIceCandidateType::Relay),
            );
        }
    }

    let mut rtt_ms = 0u32;
    let mut relayed = false;
    for entry in report.iter() {
        if let RTCStatsReportEntry::IceCandidatePair(pair) = entry {
            // The nominated pair is the one carrying media.
            if !pair.nominated {
                continue;
            }
            let seconds = if pair.current_round_trip_time > 0.0 {
                pair.current_round_trip_time
            } else if pair.responses_received > 0 {
                pair.total_round_trip_time / pair.responses_received as f64
            } else {
                0.0
            };
            rtt_ms = (seconds * 1_000.0).round().clamp(0.0, u32::MAX as f64) as u32;
            relayed = relayed_local
                .get(&pair.local_candidate_id)
                .copied()
                .unwrap_or(false);
        }
    }

    let mut packets_lost = 0i64;
    let mut bytes_sent = 0u64;
    let mut bytes_received = 0u64;
    for entry in report.iter() {
        match entry {
            RTCStatsReportEntry::OutboundRtp(outbound) => {
                bytes_sent = bytes_sent.saturating_add(outbound.sent_rtp_stream_stats.bytes_sent);
            }
            RTCStatsReportEntry::InboundRtp(inbound) => {
                bytes_received = bytes_received.saturating_add(inbound.bytes_received);
                packets_lost =
                    packets_lost.saturating_add(inbound.received_rtp_stream_stats.packets_lost);
            }
            _ => {}
        }
    }

    RtcPeerStats {
        author,
        state,
        rtt_ms,
        packets_lost,
        relayed,
        bytes_sent,
        bytes_received,
        // The caller fills these from the keyring and the shared counters;
        // stats() has the crypto context, peer_stats() does not.
        e2ee: false,
        key_epoch: None,
        decrypt_errors: 0,
    }
}

/// The Opus codec the default MediaEngine registers.
fn opus_codec() -> RTCRtpCodec {
    RTCRtpCodec {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: CLOCK_RATE,
        channels: 2,
        sdp_fmtp_line: OPUS_FMTP.to_owned(),
        rtcp_feedback: vec![],
    }
}

/// Deterministic, non-zero SSRC for the local track towards one remote.
///
/// The remote author is part of the hash, so two peers on the same call do not
/// share an SSRC inside one `PeerConnection` (they have separate connections
/// anyway, but a stable value makes logs and tests reproducible).
fn derive_ssrc(local: AuthorId, remote: AuthorId, call_id: CallId) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in local
        .as_bytes()
        .iter()
        .chain(remote.as_bytes())
        .chain(call_id.0.iter())
    {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash | 1
}
