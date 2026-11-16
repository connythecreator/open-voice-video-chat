use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::connection_limits::{self, ConnectionLimits};
use libp2p::kad::store::MemoryStore;
use libp2p::multiaddr::Protocol;
use libp2p::request_response::OutboundRequestId;
use libp2p::request_response::{self, ProtocolSupport, cbor};
use libp2p::swarm::DialError;
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::dial_opts::{DialOpts, PeerCondition};
use libp2p::swarm::{ConnectionId, NetworkBehaviour, SwarmEvent};
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder, dcutr, identify, kad, mdns, noise,
    ping, relay, tcp, upnp, yamux,
};
use ovvc_control::{ControlFrame, ReplayGuard, now_ms};
use ovvc_core::{
    AttachmentId, AuthorId, BLOB_CHUNK_BYTES, ChannelId, EventId, GuildId, Identity,
    MAX_ATTACHMENT_BYTES, SharedStorage, SignedEvent, SyncCursor, hash_bytes,
};
use ovvc_media::{CallId, MediaPacket};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::announce::{Announcement, MAX_ANNOUNCED_ADDRESSES};
use crate::peer_id_for_author;

/// Set `OVVC_NET_TRACE` (to any value) to print discovery and dial decisions
/// to stderr. Off in normal runs; used to diagnose a peer that never appears
/// on the LAN.
fn trace(message: std::fmt::Arguments<'_>) {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if *ENABLED.get_or_init(|| std::env::var_os("OVVC_NET_TRACE").is_some()) {
        eprintln!("ovvc-net: {message}");
    }
}

macro_rules! trace_net {
    ($($arg:tt)*) => {
        trace(format_args!($($arg)*))
    };
}

const PROTOCOL: &str = "/ovvc/1";
const SYNC_BATCH: u32 = 500;
/// Keep one sync response comfortably below the request-response codec's
/// response limit (10 MiB) and small enough that a slow page does not stall
/// the event loop. A single oversized event is still sent on its own.
const SYNC_MAX_BYTES: usize = 768 * 1024;
/// A single sync walk may not page forever. If it does, restart from the
/// beginning after a pause; duplicate events are cheap because imports dedupe.
const SYNC_MAX_PAGES: u32 = 200;
const SYNC_RETRY_DELAY: Duration = Duration::from_secs(2);
/// Safety net: periodically re-walk a peer's history even when nothing failed.
/// Incoming deliveries are already live; this closes any gap left by a lost
/// delivery or a page that raced a concurrent write.
const SYNC_RESYNC_INTERVAL: Duration = Duration::from_secs(300);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);
/// A guild peer that drops is redialled automatically, with exponential
/// backoff. Without this a chat left open overnight stays silent after one
/// network blip: mDNS re-announces a peer roughly every six minutes and only
/// for hosts on the same link.
const RECONNECT_MIN: Duration = Duration::from_secs(3);
const RECONNECT_MAX: Duration = Duration::from_secs(60);
/// How long an address that answered Hello for another guild is left out of
/// the dial candidates.
///
/// The record is per address, not per peer: one daemon serves every guild it
/// hosts from a single author identity, so its peer id is the same on each
/// guild's listening port. A refusal from one port says nothing about the
/// others, and banning the peer id used to block the port that does serve
/// this guild for the whole ten minutes. Its mDNS announcements and Identify
/// messages keep arriving after the mismatch, and every one of them used to
/// open a fresh connection only to be refused again; per-address records stop
/// that loop without taking the good port with it.
const MISMATCH_QUARANTINE: Duration = Duration::from_secs(600);
/// How many times a peer is redialled after one of its addresses answered from
/// another guild, and how many wrong-guild addresses are remembered per peer.
/// Both bound the work a peer can cause: one retry round per refusal, then
/// discovery has to produce a new address.
const MISMATCH_RETRIES: u32 = 3;
const MAX_WRONG_GUILD_ADDRS: usize = 32;
/// Discovery paths (mDNS, Identify, DHT) may report the same peer many times a
/// second. One dial per peer per cooldown keeps a noisy link from turning into
/// a dial storm, while reconnect backoff already spaces out guild peers.
const DISCOVERY_DIAL_COOLDOWN: Duration = Duration::from_secs(2);
/// A rejected or lost `Deliver` is retried a few times with backoff. Most
/// rejections are ordering: a message can arrive before the `ChannelCreated`
/// event it depends on, and the retry succeeds once that lands. The periodic
/// resync is the final backstop.
const DELIVERY_RETRY_BASE: Duration = Duration::from_secs(2);
const DELIVERY_MAX_ATTEMPTS: u32 = 6;
/// A connected peer that never completes Hello (it does not speak the
/// protocol, or keeps failing the exchange) is dropped after this many tries
/// instead of being asked every two seconds forever.
const HELLO_MAX_ATTEMPTS: u32 = 5;
/// How long a peer that failed the handshake is left alone.
///
/// This is deliberately much shorter than `MISMATCH_QUARANTINE`: a timeout or
/// an unsupported protocol does not say which guild an address serves, and a
/// slow or temporarily overloaded but healthy listener must not be taken out
/// of play for ten minutes. Only an actual guild refusal is recorded as a
/// wrong-guild address. See `rejected` and `wrong_guild_addrs`.
const HELLO_FAILURE_COOLDOWN: Duration = Duration::from_secs(60);
/// An event the store keeps rejecting while syncing is skipped only after this
/// many attempts. A transient failure (disk, ordering) must not silently lose
/// an event that a later attempt would accept.
const SYNC_REJECT_MAX_ATTEMPTS: u32 = 3;
/// Private Kademlia namespace: ovvc nodes never join the public IPFS DHT.
const KAD_PROTOCOL: &str = "/ovvc/kad/1";
const ANNOUNCE_TTL_SECS: u64 = 3600;
const ANNOUNCE_REPUBLISH: Duration = Duration::from_secs(600);
/// The most peers one media frame may be fanned out to. A call is capped well
/// below this, so the limit only bounds the damage a caller that mixed up its
/// roster can do: one frame must never become an unbounded fan-out storm.
pub const MAX_MEDIA_TARGETS: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireRequest {
    Hello {
        guild_id: GuildId,
        listen_addrs: Vec<String>,
    },
    Sync {
        guild_id: GuildId,
        cursor: SyncCursor,
        limit: u32,
    },
    Deliver {
        event: SignedEvent,
    },
    /// An ephemeral control message (presence, typing, roster, election).
    ///
    /// Control frames are signed by the author and verified before they are
    /// forwarded, but they are never written to the event log.
    Control {
        frame: ControlFrame,
    },
    /// One chunk of a content-addressed blob. `offset` is the byte position
    /// the chunk starts at; the response carries the total length so a
    /// fetcher knows when it is done.
    Blob {
        hash: AttachmentId,
        offset: u64,
    },
    /// One media frame for a voice or video call.
    ///
    /// This is the interim media path for phase 0, not RTP: a frame rides the
    /// same reliable, ordered request-response stream as control traffic, so
    /// it pays a round trip of latency and a stalled frame stalls the ones
    /// behind it. WebRTC and NAT traversal replace it in a later phase.
    ///
    /// The call id is checked against the channel id before the frame is
    /// delivered. `MediaPacket::author` is not signed at this layer: the
    /// daemon checks it against the authenticated libp2p source peer.
    Media {
        channel_id: ChannelId,
        packet: MediaPacket,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum WireResponse {
    Hello {
        guild_id: GuildId,
        listen_addrs: Vec<String>,
    },
    Events {
        events: Vec<SignedEvent>,
        more: bool,
    },
    Blob {
        hash: AttachmentId,
        offset: u64,
        total: u64,
        data: Vec<u8>,
    },
    Ack,
    Error {
        message: String,
    },
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    mdns: mdns::tokio::Behaviour,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    upnp: Toggle<upnp::tokio::Behaviour>,
    relay_client: relay::client::Behaviour,
    dcutr: dcutr::Behaviour,
    kad: kad::Behaviour<MemoryStore>,
    rr: cbor::Behaviour<WireRequest, WireResponse>,
    limits: connection_limits::Behaviour,
}

#[derive(Debug, Clone)]
pub enum NodeEvent {
    Listening(Multiaddr),
    ExternalAddr(Multiaddr),
    ExternalAddrRemoved(Multiaddr),
    PeerConnected(PeerId),
    PeerDisconnected(PeerId),
    HolePunch {
        peer: PeerId,
        success: bool,
    },
    Synced(PeerId),
    Event(SignedEvent),
    /// A verified control frame received from a peer.
    Control(ControlFrame),
    /// A media frame received from a peer.
    ///
    /// `from` is the authenticated libp2p peer the frame arrived from. The
    /// packet's author is not verified here: the daemon holds the author to
    /// peer mapping and checks one against the other.
    Media {
        channel_id: ChannelId,
        packet: MediaPacket,
        from: PeerId,
    },
    /// A DHT lookup for the given author has started.
    LookingUp(AuthorId),
    /// The DHT returned a verified announcement for the given author.
    PeerFound {
        author: AuthorId,
        addresses: Vec<Multiaddr>,
    },
    /// A DHT lookup finished without a usable record.
    LookupFailed {
        author: AuthorId,
        reason: String,
    },
    Error(String),
}

// A signed event now carries reply/mention metadata, which makes this variant
// the large one. Boxing it would put an allocation on every broadcast to buy a
// few hundred bytes on a channel that holds a handful of commands; the event is
// moved straight through to the request-response handler instead.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum NodeCommand {
    Broadcast(SignedEvent),
    Control(ControlFrame),
    /// Send a control frame to one peer. Used for targeted WebRTC signalling,
    /// which must not fan out to the whole guild.
    ControlTo {
        target: PeerId,
        frame: ControlFrame,
    },
    /// Send one media frame to exactly these peers. Media is lossy and is
    /// never retried; the next frame replaces it.
    Media {
        channel_id: ChannelId,
        packet: MediaPacket,
        targets: Vec<PeerId>,
    },
    AddExternalAddr(Multiaddr),
    Lookup(AuthorId),
    /// Fetch a blob from any connected guild peer and cache it locally.
    FetchBlob {
        hash: AttachmentId,
        reply: oneshot::Sender<std::result::Result<Vec<u8>, String>>,
    },
    Shutdown,
}

pub struct NodeConfig {
    pub identity_seed: [u8; 32],
    pub guild_id: GuildId,
    pub store: SharedStorage,
    pub listen: Vec<Multiaddr>,
    pub dial: Vec<Multiaddr>,
    pub relay: Option<Multiaddr>,
    pub enable_upnp: bool,
    pub enable_natpmp: bool,
}

/// A point-in-time view of a running node.
///
/// Frontends that already consume [`NodeEvent`] do not need this, but a
/// caller that only wants "what is this node doing right now" (a status bar,
/// a diagnostics page, the daemon's `ConnectionState` reply) should not have
/// to replay the event stream to find out.
#[derive(Debug, Clone, Default)]
pub struct NodeStatusSnapshot {
    pub listening: Vec<Multiaddr>,
    pub external: Vec<Multiaddr>,
    pub connected: Vec<PeerId>,
    /// Addresses this node has learned for peers, including peers that have
    /// not completed Hello. Stored in insertion order: `dial_peer_addrs`
    /// ranks and caps them when it dials.
    pub dial: Vec<Multiaddr>,
    /// The same addresses, kept apart by peer so a caller can ask what this
    /// node would dial for one peer rather than for all of them.
    pub known: HashMap<PeerId, Vec<Multiaddr>>,
}

/// The live state behind a [`NodeHandle`].
///
/// The node loop refreshes it once per event-loop turn, so a reader sees the
/// state as of the last event rather than a partially updated snapshot.
#[derive(Clone, Default)]
pub struct NodeStatus {
    inner: Arc<Mutex<NodeStatusSnapshot>>,
    /// False once the node loop has exited, including by a panic. The daemon
    /// would otherwise keep reporting a guild as connected after its swarm
    /// task is gone, with a listener list frozen at the last turn.
    alive: Arc<AtomicBool>,
}

impl NodeStatus {
    fn lock(&self) -> MutexGuard<'_, NodeStatusSnapshot> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn snapshot(&self) -> NodeStatusSnapshot {
        self.lock().clone()
    }

    /// Whether the node loop is still running.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    fn mark_started(&self) {
        self.alive.store(true, Ordering::Release);
    }

    fn observe(&self, swarm: &Swarm<Behaviour>, state: &NodeState) {
        let mut inner = self.lock();
        inner.listening = swarm.listeners().cloned().collect();
        inner.external = swarm.external_addresses().cloned().collect();
        inner.connected = swarm.connected_peers().copied().collect();
        inner.known = state.known_addrs.clone();
        inner.dial.clear();
        for address in state.known_addrs.values().flatten() {
            if !inner.dial.contains(address) {
                inner.dial.push(address.clone());
            }
        }
    }
}

/// Clears `NodeStatus::alive` however the node loop leaves, unwind included.
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct NodeHandle {
    pub peer_id: PeerId,
    command_tx: mpsc::UnboundedSender<NodeCommand>,
    status: NodeStatus,
}

impl NodeHandle {
    /// The node's addresses and connections as of its last event-loop turn.
    pub fn status(&self) -> NodeStatusSnapshot {
        self.status.snapshot()
    }

    /// The addresses a reconnect for this peer would dial. Stored in
    /// insertion order; `dial_peer_addrs` ranks and caps them at dial time.
    pub fn known_addrs(&self, peer: PeerId) -> Vec<Multiaddr> {
        self.status
            .snapshot()
            .known
            .get(&peer)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether the node loop is still running.
    pub fn is_alive(&self) -> bool {
        self.status.is_alive()
    }

    pub fn broadcast(&self, event: SignedEvent) {
        let _ = self.command_tx.send(NodeCommand::Broadcast(event));
    }

    /// Send an ephemeral control frame to every connected peer in this guild.
    pub fn send_control(&self, frame: ControlFrame) {
        let _ = self.command_tx.send(NodeCommand::Control(frame));
    }

    /// Send an ephemeral control frame to one peer.
    ///
    /// WebRTC SDP and ICE candidates name a single target author, so they must
    /// not travel to every guild member. libp2p will dial the peer if it is
    /// not connected yet.
    pub fn send_control_to(&self, target: PeerId, frame: ControlFrame) {
        let _ = self
            .command_tx
            .send(NodeCommand::ControlTo { target, frame });
    }

    /// Send one media frame to `targets`, which must be the call's members.
    ///
    /// This is the interim phase 0 media path: the frame travels over the
    /// reliable, ordered request-response stream and is not RTP. Targets are
    /// deduplicated and capped at [`MAX_MEDIA_TARGETS`]. A failed send is not
    /// retried and not reported, because the next frame supersedes it.
    pub fn send_media(&self, channel_id: ChannelId, packet: MediaPacket, targets: Vec<PeerId>) {
        let _ = self.command_tx.send(NodeCommand::Media {
            channel_id,
            packet,
            targets,
        });
    }

    /// Look up an author's announced addresses in the DHT and dial them.
    pub fn lookup(&self, author: AuthorId) {
        let _ = self.command_tx.send(NodeCommand::Lookup(author));
    }

    /// Fetch a blob from any connected guild peer, or return the cached copy.
    ///
    /// The receiver resolves with the verified bytes, or a message explaining
    /// why no peer could supply them. Dropping the receiver cancels nothing
    /// mid-chunk beyond the next response being ignored.
    pub fn fetch_blob(
        &self,
        hash: AttachmentId,
    ) -> Option<oneshot::Receiver<std::result::Result<Vec<u8>, String>>> {
        let (reply, receiver) = oneshot::channel();
        self.command_tx
            .send(NodeCommand::FetchBlob { hash, reply })
            .ok()?;
        Some(receiver)
    }

    pub fn shutdown(&self) {
        let _ = self.command_tx.send(NodeCommand::Shutdown);
    }
}

impl Drop for NodeHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Build the swarm shared by `start` and the address tests.
///
/// Kept separate so a test can drive the real Kademlia behaviour instead of a
/// stand-in: `replace_known_addrs` has to leave the proving address in the
/// routing table as well as in `known_addrs`, and only the real behaviour
/// shows what it actually stores (fully-qualified `/p2p/<peer>` addresses).
fn build_swarm(identity_seed: [u8; 32], enable_upnp: bool) -> Result<(Swarm<Behaviour>, PeerId)> {
    let mut seed = identity_seed;
    let keypair = libp2p::identity::Keypair::ed25519_from_bytes(&mut seed)
        .context("creating libp2p identity")?;
    let peer_id = keypair.public().to_peer_id();
    let swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(move |key, relay_client| {
            let mdns =
                mdns::tokio::Behaviour::new(mdns::Config::default(), key.public().to_peer_id())
                    .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
            let identify = identify::Behaviour::new(identify::Config::new(
                "/ovvc/identify/1".to_string(),
                key.public(),
            ));
            let ping = ping::Behaviour::new(ping::Config::new());
            let upnp = Toggle::from(enable_upnp.then(upnp::tokio::Behaviour::default));
            let dcutr = dcutr::Behaviour::new(key.public().to_peer_id());
            let mut kad_config = kad::Config::new(StreamProtocol::new(KAD_PROTOCOL));
            kad_config.set_query_timeout(Duration::from_secs(60));
            // The default (`OnConnected`) puts every peer we hold a connection
            // to into the routing table, including one that only answered
            // "wrong guild" for this node, and every later DHT query dials it
            // again. A peer enters the table only through `add_routing_address`,
            // which is called after Hello proved the guild.
            kad_config.set_kbucket_inserts(kad::BucketInserts::Manual);
            let mut kad = kad::Behaviour::with_config(
                key.public().to_peer_id(),
                MemoryStore::new(key.public().to_peer_id()),
                kad_config,
            );
            kad.set_mode(Some(kad::Mode::Server));
            let rr = cbor::Behaviour::<WireRequest, WireResponse>::new(
                [(StreamProtocol::new(PROTOCOL), ProtocolSupport::Full)],
                request_response::Config::default().with_request_timeout(Duration::from_secs(30)),
            );
            // libp2p has no default connection limits. Without these, a peer
            // (or a dial loop) can exhaust this process's file descriptors.
            //
            // The per-peer limit has to leave room for every guild node the
            // other daemon serves. All of a daemon's guild nodes share one peer
            // id, so two daemons that share two guilds meet as the same peer id
            // on two ports in each direction. At a limit of two, one wrong-guild
            // node could take both slots: the daemon refused the connection from
            // the node whose guild did match, its Hello never arrived, and the
            // peer looked offline while both ports were up. Sixteen fits eight
            // guilds in both directions and still caps a dial loop.
            let limits = connection_limits::Behaviour::new(
                ConnectionLimits::default()
                    .with_max_pending_outgoing(Some(8))
                    .with_max_pending_incoming(Some(16))
                    .with_max_established_incoming(Some(64))
                    .with_max_established_outgoing(Some(64))
                    .with_max_established(Some(128))
                    .with_max_established_per_peer(Some(16)),
            );
            Ok(Behaviour {
                mdns,
                identify,
                ping,
                upnp,
                relay_client,
                dcutr,
                kad,
                rr,
                limits,
            })
        })?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(120)))
        .build();
    Ok((swarm, peer_id))
}

pub async fn start(
    config: NodeConfig,
    event_tx: mpsc::UnboundedSender<NodeEvent>,
) -> Result<NodeHandle> {
    // The author identity and the transport identity are derived from the same
    // ed25519 seed, which is what lets a peer id pin an author id.
    let author = Identity::from_seed(config.identity_seed);
    let (mut swarm, peer_id) = build_swarm(config.identity_seed, config.enable_upnp)?;
    trace_net!(
        "node up as {peer_id}: listen {:?}, dial {:?}",
        config.listen,
        config.dial
    );
    trace_net!(
        "[{}] guild {} online, peer {peer_id}",
        config.guild_id.short(),
        config.guild_id
    );

    if config.listen.is_empty() {
        swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;
    } else {
        for address in &config.listen {
            swarm.listen_on(address.clone())?;
        }
    }

    if let Some(relay) = &config.relay {
        let circuit = relay.clone().with(Protocol::P2pCircuit);
        if let Err(error) = swarm.listen_on(circuit) {
            let _ = event_tx.send(NodeEvent::Error(format!(
                "failed to reserve relay circuit at {relay}: {error}"
            )));
        }
    }

    // Group the configured addresses by peer and dial each peer once with all
    // of its addresses. Dialing them one at a time satisfies libp2p's
    // `DisconnectedAndNotDialing` condition on the first address and silently
    // skips every other address in the invite.
    let mut initial: HashMap<PeerId, Vec<Multiaddr>> = HashMap::new();
    for address in &config.dial {
        // The address is not handed to Kademlia yet. A configured peer is only
        // known to be in this guild after Hello, and an unverified entry in the
        // routing table is what makes a refused peer become a dial loop.
        match peer_of(address) {
            Some(peer) => initial.entry(peer).or_default().push(address.clone()),
            None => {
                if let Err(error) = swarm.dial(address.clone()) {
                    let _ = event_tx.send(NodeEvent::Error(format!(
                        "failed to dial {address}: {error}"
                    )));
                }
            }
        }
    }

    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let guild_id = config.guild_id;
    let store = config.store;
    let enable_natpmp = config.enable_natpmp;
    let status = NodeStatus::default();
    status.mark_started();
    let alive = Arc::clone(&status.alive);
    tokio::spawn(run(
        AliveGuard(alive),
        swarm,
        guild_id,
        store,
        author,
        command_rx,
        command_tx.clone(),
        event_tx,
        initial,
        HashSet::new(),
        enable_natpmp,
        status.clone(),
    ));

    Ok(NodeHandle {
        peer_id,
        command_tx,
        status,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run(
    _alive: AliveGuard,
    mut swarm: Swarm<Behaviour>,
    guild_id: GuildId,
    store: SharedStorage,
    author: Identity,
    mut command_rx: mpsc::UnboundedReceiver<NodeCommand>,
    command_tx: mpsc::UnboundedSender<NodeCommand>,
    event_tx: mpsc::UnboundedSender<NodeEvent>,
    initial: HashMap<PeerId, Vec<Multiaddr>>,
    mut listen_addrs: HashSet<Multiaddr>,
    enable_natpmp: bool,
    status: NodeStatus,
) {
    let local_peer = *swarm.local_peer_id();
    let mut state = NodeState {
        tag: guild_id.short(),
        ..NodeState::default()
    };
    for (peer, addresses) in initial {
        state.wanted.insert(peer);
        if let Some(error) = dial_peer_addrs(&mut swarm, &mut state, peer, addresses, false) {
            let _ = event_tx.send(NodeEvent::Error(error));
        }
    }
    let mut natpmp_ports = HashSet::new();
    // Dropped when `run` exits. NAT-PMP workers watch it so a shutdown does
    // not leave one task (holding channel senders open) per listen port.
    let (_natpmp_shutdown, natpmp_shutdown_rx) = tokio::sync::watch::channel(());
    let mut publish_interval = tokio::time::interval(ANNOUNCE_REPUBLISH);
    publish_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut maintenance_interval = tokio::time::interval(MAINTENANCE_INTERVAL);
    maintenance_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_resync = Instant::now();

    loop {
        status.observe(&swarm, &state);
        tokio::select! {
            command = command_rx.recv() => {
                match command {
                    Some(NodeCommand::Broadcast(event)) => {
                        forward_event(&mut swarm, &mut state, guild_id, &event, None);
                    }
                    Some(NodeCommand::Control(frame)) => {
                        forward_control(&mut swarm, &state, guild_id, &frame, None);
                    }
                    Some(NodeCommand::ControlTo { target, frame }) => {
                        send_control_to(&mut swarm, target, frame);
                    }
                    Some(NodeCommand::Media {
                        channel_id,
                        packet,
                        targets,
                    }) => {
                        send_media(&mut swarm, channel_id, packet, targets);
                    }
                    Some(NodeCommand::AddExternalAddr(address)) => {
                        swarm.add_external_address(address.clone());
                        let _ = event_tx.send(NodeEvent::ExternalAddr(address));
                        publish_announcement(&mut swarm, &author, &listen_addrs, &state);
                    }
                    Some(NodeCommand::Lookup(author_id)) => {
                        let id = swarm
                            .behaviour_mut()
                            .kad
                            .get_record(record_key_for_author(author_id));
                        state.lookups.insert(id, author_id);
                        let _ = event_tx.send(NodeEvent::LookingUp(author_id));
                    }
                    Some(NodeCommand::FetchBlob { hash, reply }) => {
                        start_blob_fetch(&mut swarm, &mut state, &store, hash, reply);
                    }
                    Some(NodeCommand::Shutdown) | None => break,
                }
            }
            _ = publish_interval.tick() => {
                publish_announcement(&mut swarm, &author, &listen_addrs, &state);
            }
            _ = maintenance_interval.tick() => {
                let now = Instant::now();
                run_maintenance(
                    &mut swarm,
                    &mut state,
                    guild_id,
                    &listen_addrs,
                    now,
                );
                if now.duration_since(last_resync) >= SYNC_RESYNC_INTERVAL {
                    last_resync = now;
                    resync_all(&mut swarm, &mut state, guild_id);
                    // The DHT only learns about peers that joined after the
                    // first bootstrap, so refresh it with the periodic resync
                    // rather than on every connection.
                    let _ = swarm.behaviour_mut().kad.bootstrap();
                }
                // A Hello that proved a guild peer asks for one announcement,
                // because the connection event that used to trigger it can no
                // longer do so while the peer is unverified. The flag is only
                // cleared when a publish was actually issued: if the last
                // verified peer left before this tick, the request survives to
                // the next Hello instead of being dropped.
                if state.announce_pending
                    && publish_announcement(&mut swarm, &author, &listen_addrs, &state)
                {
                    state.announce_pending = false;
                }
            }
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        listen_addrs.insert(address.clone());
                        if enable_natpmp
                            && let Some(port) = tcp_port(&address)
                            && natpmp_ports.insert(port)
                        {
                            tokio::spawn(natpmp_worker(
                                port,
                                command_tx.clone(),
                                event_tx.clone(),
                                natpmp_shutdown_rx.clone(),
                            ));
                        }
                        let _ = event_tx.send(NodeEvent::Listening(address));
                        publish_announcement(&mut swarm, &author, &listen_addrs, &state);
                    }
                    SwarmEvent::ConnectionEstablished {
                        peer_id,
                        connection_id,
                        endpoint,
                        num_established,
                        ..
                    } => {
                        if peer_id == local_peer {
                            continue;
                        }
                        trace_net!(
                            "[{}] connection established with {peer_id} (dialer {}, {} established)",
                            state.tag,
                            endpoint.is_dialer(),
                            num_established.get()
                        );
                        // Remembered before the redundant-connection early
                        // return below, because a refusal on any connection has
                        // to be traced back to its address.
                        state
                            .connection_addrs
                            .insert(connection_id, endpoint.get_remote_address().clone());
                        // Track which side dialled each connection. A Hello
                        // can only be sent safely while every connection to
                        // the peer is outbound; see
                        // `hello_connection_is_safe`.
                        state
                            .connections
                            .entry(peer_id)
                            .or_default()
                            .insert(connection_id, endpoint.is_dialer());
                        // Kademlia only learns the addresses of peers that
                        // proved this guild. An unverified peer in the routing
                        // table is a dial loop: every announcement query dials
                        // it, the connection is refused and closed, and the
                        // next query dials it again. Only an outbound
                        // connection's remote address is a listening address;
                        // on an inbound connection it is the peer's ephemeral
                        // source port, which cannot be dialled.
                        if endpoint.is_dialer()
                            && state.peer_guilds.get(&peer_id) == Some(&guild_id)
                        {
                            add_routing_address(
                                &mut swarm,
                                &state.tag,
                                peer_id,
                                endpoint.get_remote_address(),
                            );
                        }
                        // Per-peer work is done once, not once per redundant
                        // connection. libp2p allows several per peer.
                        if num_established.get() > 1 {
                            // A second connection can still be the one that
                            // proves the peer's guild: the first may have
                            // never completed Hello (half-open, wrong
                            // protocol, or an upgrade that raced). Only the
                            // dialer may introduce itself, for the same
                            // membership-privacy reason as below. Crucially,
                            // request-response 0.29 cannot choose the
                            // connection for a request: it picks one by
                            // `request_id % connections.len()`. If the peer
                            // has a connection in each direction, the Hello
                            // could therefore leave over the connection the
                            // peer dialled, telling a peer which has not
                            // introduced itself this node's guild and listen
                            // addresses. In that ambiguous case `hello_action`
                            // names one side to close its inbound connections
                            // first, so a Hello is never sent over an
                            // ambiguous set. When every connection is one this
                            // node dialled, any choice is safe.
                            match hello_action(
                                &state,
                                local_peer,
                                peer_id,
                                endpoint.is_dialer(),
                                num_established.get(),
                            ) {
                                HelloAction::Send => send_hello(
                                    &mut swarm,
                                    &mut state,
                                    guild_id,
                                    &listen_addrs,
                                    &peer_id,
                                ),
                                HelloAction::CloseInboundThenSend => {
                                    // Deterministic tie-break for a mutual
                                    // dial where neither side proved the
                                    // guild before both connections were
                                    // established. The larger peer id closes
                                    // its inbound connections, so every
                                    // connection left is outbound and
                                    // request-response cannot leak this
                                    // node's Hello to a peer that has not
                                    // introduced itself. The retry waits for
                                    // the closes to be processed, then sends
                                    // over the remaining outbound connection.
                                    let inbound: Vec<ConnectionId> = state
                                        .connections
                                        .get(&peer_id)
                                        .map(|connections| {
                                            connections.inbound.iter().copied().collect()
                                        })
                                        .unwrap_or_default();
                                    for connection_id in inbound {
                                        let _ = swarm.close_connection(connection_id);
                                    }
                                    state.hello_retry.insert(peer_id, Instant::now());
                                }
                                HelloAction::Wait => {}
                            }
                            continue;
                        }
                        // Reaching a peer cancels a pending redial, but does
                        // not reset the backoff: a peer that completes the
                        // upgrade and immediately drops would otherwise be
                        // redialled every few seconds forever. A successful
                        // Hello (or sync) is the signal that it is healthy.
                        state.reconnect_at.remove(&peer_id);
                        // Announce once a peer exists, so nodes that started
                        // with no reachable neighbours still publish a record.
                        // The announcement is gated on a verified guild peer;
                        // with only refusals connected it would query an empty
                        // routing table and report a failed quorum.
                        publish_announcement(&mut swarm, &author, &listen_addrs, &state);
                        let _ = event_tx.send(NodeEvent::PeerConnected(peer_id));
                        // Only the dialer introduces itself. An inbound peer
                        // has not shown that it knows this guild; sending the
                        // first Hello hands it the guild id and our listen
                        // addresses, which would make guild membership a
                        // bearer token that any network neighbour can learn
                        // simply by connecting. A legitimate inbound peer
                        // dialled us from an invite and therefore already
                        // knows the guild id, so it sends Hello first. This
                        // is the single-connection path, so a dialer here has
                        // only outbound connections and the send is safe; the
                        // guard keeps that invariant explicit.
                        if hello_action(
                            &state,
                            local_peer,
                            peer_id,
                            endpoint.is_dialer(),
                            num_established.get(),
                        ) == HelloAction::Send
                        {
                            send_hello(
                                &mut swarm,
                                &mut state,
                                guild_id,
                                &listen_addrs,
                                &peer_id,
                            );
                        }
                    }
                    SwarmEvent::ConnectionClosed {
                        peer_id,
                        connection_id,
                        num_established,
                        ..
                    } => {
                        state.connection_addrs.remove(&connection_id);
                        state.pending_close.remove(&connection_id);
                        let no_connections_left =
                            if let Some(connections) = state.connections.get_mut(&peer_id) {
                                connections.remove(connection_id);
                                connections.is_empty()
                            } else {
                                false
                            };
                        if no_connections_left {
                            state.connections.remove(&peer_id);
                        }
                        // A guild proof belongs to the connection that carried
                        // the Hello, not to the peer id. A peer can hold
                        // several connections and only one of them may have
                        // completed Hello, so closing that one must not leave
                        // a connection that never proved the guild looking
                        // proven by association.
                        let proof_remains = match state.proven_connections.get_mut(&peer_id) {
                            Some(proven) => {
                                proven.remove(&connection_id);
                                !proven.is_empty()
                            }
                            None => false,
                        };
                        if !proof_remains {
                            state.proven_connections.remove(&peer_id);
                        }
                        if peer_id == local_peer {
                            continue;
                        }
                        if num_established > 0 {
                            // Other connections remain. If none of them has
                            // proved this guild, the peer is unproven again:
                            // drop the guild and the sync state that was
                            // inherited from the closed connection. A proven
                            // connection that is still open keeps the peer
                            // proven.
                            if !proof_remains && state.peer_guilds.remove(&peer_id).is_some() {
                                state.synced.remove(&peer_id);
                                state.sync_heads.remove(&peer_id);
                                // Make sure another Hello is attempted. If
                                // every remaining connection is one this node
                                // dialled, any connection request-response
                                // picks is safe, so introduce ourselves now.
                                // Otherwise an inbound connection remains and
                                // this node must not send first; queue a retry
                                // that fires if the connection set becomes
                                // unambiguous, while the peer's own Hello over
                                // the connection it dialled is the proof.
                                if !hello_in_flight(&state, &peer_id) {
                                    if hello_connection_is_safe(&state, &peer_id) {
                                        send_hello(
                                            &mut swarm,
                                            &mut state,
                                            guild_id,
                                            &listen_addrs,
                                            &peer_id,
                                        );
                                    } else {
                                        state.hello_retry.insert(peer_id, Instant::now());
                                    }
                                }
                            }
                            continue;
                        }
                        trace_net!("[{}] connection closed with {peer_id}", state.tag);
                        // The peer has no connections left, so any tracking
                        // entry still present is stale.
                        state.connections.remove(&peer_id);
                        state.proven_connections.remove(&peer_id);
                        state.peer_guilds.remove(&peer_id);
                        state.synced.remove(&peer_id);
                        state.sync_retry.remove(&peer_id);
                        state.sync_attempts.remove(&peer_id);
                        state.hello_retry.remove(&peer_id);
                        state.sync_heads.remove(&peer_id);
                        state.pending_sync.retain(|_, pending| pending.peer != peer_id);
                        state.hello_in_flight.retain(|_, hello_peer| *hello_peer != peer_id);
                        // Known addresses are deliberately kept: they are what
                        // the automatic redial uses.
                        if state.wanted.contains(&peer_id) {
                            state
                                .reconnect_at
                                .insert(peer_id, Instant::now() + RECONNECT_MIN);
                        }
                        let _ = event_tx.send(NodeEvent::PeerDisconnected(peer_id));
                    }
                    SwarmEvent::Dialing {
                        peer_id,
                        connection_id,
                    } => {
                        // Every dial passes through the swarm, including the
                        // ones a behaviour issues on its own (Kademlia dials a
                        // peer from its routing table). A "swarm dialing" line
                        // with no matching "dialling" line above it is one of
                        // those, which is otherwise invisible.
                        trace_net!(
                            "[{}] swarm dialing {peer_id:?} conn {connection_id}",
                            state.tag
                        );
                    }
                    SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        trace_net!(
                            "[{}] outgoing connection error to {peer_id:?}: {error}",
                            state.tag
                        );
                        // A wanted peer that cannot be reached is retried
                        // automatically. The first failure is reported (a typo
                        // in an invite should not be silent); later ones are
                        // not, or an offline friend turns the screen red once
                        // a minute.
                        let first_attempt = peer_id
                            .map(|peer| state.wanted.contains(&peer) && state.reconnect_attempts.get(&peer).copied().unwrap_or(0) == 0)
                            .unwrap_or(true);
                        if let Some(peer) = peer_id
                            && state.wanted.contains(&peer)
                        {
                            state
                                .reconnect_at
                                .entry(peer)
                                .or_insert(Instant::now() + RECONNECT_MIN);
                        }
                        // Racing dials and policy-aborted dials are normal in a
                        // swarm with several connection sources. Reporting them
                        // turns a healthy network into a stream of red text.
                        if first_attempt && !benign_dial_error(&error) {
                            let suffix = peer_id
                                .map(|peer| format!(" to {peer}"))
                                .unwrap_or_default();
                            let _ = event_tx.send(NodeEvent::Error(format!(
                                "outgoing connection error{suffix}: {error}"
                            )));
                        }
                    }
                    SwarmEvent::IncomingConnectionError { .. } => {
                        // A peer that vanishes mid-handshake is not actionable;
                        // the next dial or mDNS announcement retries it.
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Discovered(peers))) => {
                        for (peer, address) in peers {
                            if peer == local_peer {
                                continue;
                            }
                            trace_net!("[{}] mdns discovered {peer} at {address}", state.tag);
                            // mDNS only proves the peer is on this link; it
                            // becomes a wanted guild peer once Hello agrees on
                            // the guild.
                            let _ = dial_peer_addrs(&mut swarm, &mut state, peer, [address], false);
                        }
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Identify(
                        identify::Event::Received { peer_id, info, .. },
                    )) => {
                        if peer_id != local_peer {
                            // Identify is received over an established
                            // connection. The addresses are recorded for the
                            // routing table, but no new connection is opened:
                            // dialing here (once per connection) is what made
                            // the connection count grow exponentially.
                            let _ = dial_peer_addrs(
                                &mut swarm,
                                &mut state,
                                peer_id,
                                info.listen_addrs,
                                false,
                            );
                        }
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Upnp(upnp::Event::NewExternalAddr(
                        address,
                    ))) => {
                        swarm.add_external_address(address.clone());
                        let _ = event_tx.send(NodeEvent::ExternalAddr(address));
                        publish_announcement(&mut swarm, &author, &listen_addrs, &state);
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Upnp(
                        upnp::Event::ExpiredExternalAddr(address),
                    )) => {
                        swarm.remove_external_address(&address);
                        let _ = event_tx.send(NodeEvent::ExternalAddrRemoved(address));
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Upnp(upnp::Event::GatewayNotFound)) => {
                        let _ = event_tx.send(NodeEvent::Error(
                            "UPnP gateway not found".to_string(),
                        ));
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Upnp(
                        upnp::Event::NonRoutableGateway,
                    )) => {
                        let _ = event_tx.send(NodeEvent::Error(
                            "UPnP gateway is not publicly routable".to_string(),
                        ));
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Dcutr(event)) => {
                        let success = event.result.is_ok();
                        let _ = event_tx.send(NodeEvent::HolePunch {
                            peer: event.remote_peer_id,
                            success,
                        });
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Kad(event)) => {
                        handle_kad_event(&mut swarm, &event_tx, &mut state, event);
                    }
                    SwarmEvent::Behaviour(BehaviourEvent::Ping(_)) => {}
                    SwarmEvent::Behaviour(BehaviourEvent::Rr(event)) => {
                        handle_request_response(
                            &mut swarm,
                            &store,
                            &event_tx,
                            guild_id,
                            &listen_addrs,
                            &mut state,
                            event,
                        );
                    }
                    _ => {}
                }
            }
        }
    }
}

/// One sync walk in flight.
struct PendingSync {
    peer: PeerId,
    cursor: SyncCursor,
    page: u32,
}

/// A sync walk that failed and is waiting to be retried.
struct SyncRetry {
    at: Instant,
    cursor: SyncCursor,
    page: u32,
}

/// The live connections to one peer, split by who dialled.
///
/// Only outbound connections are safe to send an unsolicited Hello over.
/// request-response 0.29 chooses a connection by request id, not by
/// direction, so when both sets are non-empty a Hello could leave over the
/// connection the peer opened. See `hello_connection_is_safe`.
#[derive(Default)]
struct PeerConnections {
    inbound: HashSet<ConnectionId>,
    outbound: HashSet<ConnectionId>,
}

impl PeerConnections {
    fn insert(&mut self, connection_id: ConnectionId, dialer: bool) {
        if dialer {
            self.outbound.insert(connection_id);
        } else {
            self.inbound.insert(connection_id);
        }
    }

    fn remove(&mut self, connection_id: ConnectionId) {
        self.inbound.remove(&connection_id);
        self.outbound.remove(&connection_id);
    }

    fn is_empty(&self) -> bool {
        self.inbound.is_empty() && self.outbound.is_empty()
    }
}

/// Per-node transport state.
///
/// Most of this is scoped to the current connection set: a disconnect drops
/// the peer's proof of guild membership and its sync progress. Some entries
/// deliberately outlive a disconnect: `known_addrs` is what the automatic
/// redial uses, `hello_attempts` and `reconnect_attempts` keep a flapping
/// peer's backoff, and `wrong_guild_addrs` keeps a port that refused this
/// guild out of the dial candidates. A failed sync keeps its cursor so the
/// retry resumes rather than restarts.
#[derive(Default)]
struct NodeState {
    /// Short guild id, prefixed to every trace line. A daemon can serve several
    /// guilds from one process, and without the tag the two nodes' traces are
    /// impossible to tell apart.
    tag: String,
    hello_in_flight: HashMap<OutboundRequestId, PeerId>,
    pending_sync: HashMap<OutboundRequestId, PendingSync>,
    sync_retry: HashMap<PeerId, SyncRetry>,
    /// Consecutive failed or no-progress syncs with a peer. Backs the retry
    /// delay off so a misbehaving peer costs one request a minute, not one
    /// every two seconds.
    sync_attempts: HashMap<PeerId, u32>,
    hello_retry: HashMap<PeerId, Instant>,
    /// Failed Hello exchanges with a peer, across reconnects. Reset only when a
    /// Hello succeeds, so a peer that connects and immediately drops cannot
    /// keep resetting its backoff.
    hello_attempts: HashMap<PeerId, u32>,
    /// Events seen while syncing that the store rejected, with the number of
    /// attempts so far. A permanently bad event is eventually skipped instead
    /// of blocking the rest of a peer's history.
    sync_rejects: HashMap<EventId, u32>,
    /// Cursor after the last *completed* sync with a peer. Periodic resyncs
    /// start here rather than at zero, so a long-lived guild is not re-sent in
    /// full every few minutes.
    sync_heads: HashMap<PeerId, SyncCursor>,
    peer_guilds: HashMap<PeerId, GuildId>,
    synced: HashSet<PeerId>,
    lookups: HashMap<kad::QueryId, AuthorId>,
    /// Every address we know for a peer, so a dropped connection can be
    /// redialled without waiting for mDNS or a fresh Hello.
    known_addrs: HashMap<PeerId, Vec<Multiaddr>>,
    /// Peers this node should stay connected to: the addresses it was started
    /// with, plus every peer that completed a Hello for this guild.
    wanted: HashSet<PeerId>,
    /// Whether the routing table has been bootstrapped once a peer arrived.
    /// Bootstrapping on every connection turns a rejected peer into a dial
    /// loop: each connection starts a query that dials the peer that just
    /// refused us.
    bootstrapped: bool,
    /// Set when a Hello proves a guild peer, so the announce/put_record query
    /// runs once the event loop is back in control. Publishing is gated on
    /// proven peers, so a connection that is refused must not trigger it.
    /// `put_record` returning `Ok` only means the query was accepted and the
    /// record stored locally; a later `QuorumFailed` does not set this flag
    /// again, so the next attempt is the periodic republish or the next Hello.
    announce_pending: bool,
    reconnect_at: HashMap<PeerId, Instant>,
    reconnect_attempts: HashMap<PeerId, u32>,
    /// Peers that never answered the handshake or do not speak this protocol,
    /// and the time after which they may be dialled again. This is the short
    /// `HELLO_FAILURE_COOLDOWN`, not the guild-mismatch quarantine: a timeout
    /// says nothing about which guild an address serves. A guild mismatch is
    /// *not* recorded here: see `wrong_guild_addrs`.
    rejected: HashMap<PeerId, Instant>,
    /// Addresses of a peer that answered Hello for another guild, with the
    /// time each record expires. A daemon serves every guild it hosts from one
    /// identity, so the same peer id listens on one port per guild; only the
    /// address is wrong, never the whole peer. See `MISMATCH_QUARANTINE`.
    wrong_guild_addrs: HashMap<PeerId, Vec<(Multiaddr, Instant)>>,
    /// Peers with a redial pending after a guild mismatch, with the time of the
    /// next attempt and how many have been made. Bounded by `MISMATCH_RETRIES`
    /// so a peer whose other addresses are also wrong cannot cause a loop.
    mismatch_retry: HashMap<PeerId, (Instant, u32)>,
    /// The last time each (peer, address) was dialled, so discovery events
    /// cannot drive more than one attempt per address per cooldown window. One
    /// daemon can advertise several ports under one peer id, and each port has
    /// to be tried in turn.
    dialled_at: HashMap<(PeerId, Multiaddr), Instant>,
    /// The remote address of each live connection, so a refusal that arrives
    /// with only a connection id can be pinned to the address that caused it.
    connection_addrs: HashMap<ConnectionId, Multiaddr>,
    /// The live connections to each peer, split by who dialled. Used to keep
    /// an unsolicited Hello off an inbound connection; see
    /// `hello_connection_is_safe`.
    connections: HashMap<PeerId, PeerConnections>,
    /// Connections whose Hello proved the peer serves this guild. Kept per
    /// connection rather than per peer: a peer can hold a proven connection
    /// and an unproven one at the same time, and when the proven one closes
    /// the remaining connection must not inherit its guild.
    proven_connections: HashMap<PeerId, HashSet<ConnectionId>>,
    /// Connections that must be closed after the refusal they carry has been
    /// flushed. See `handle_request`'s Hello mismatch path and the
    /// `ResponseSent` arm.
    pending_close: HashSet<ConnectionId>,
    /// In-flight `Deliver` requests, kept with the event so a rejection or a
    /// lost request can be retried instead of silently dropped.
    deliveries: HashMap<OutboundRequestId, (PeerId, SignedEvent, u32)>,
    delivery_retries: Vec<PendingDelivery>,
    /// In-flight blob chunk requests. One entry per chunk, carrying the bytes
    /// assembled so far and the channel the caller is waiting on.
    fetch_blobs: HashMap<OutboundRequestId, PendingBlob>,
    /// Ephemeral control frames are deduplicated here before they reach the
    /// daemon, which keeps its own per-session guard for membership rules.
    control_replay: ReplayGuard,
}

/// One in-progress blob download.
struct PendingBlob {
    /// The peer the outstanding chunk request went to.
    peer: PeerId,
    /// Peers already asked, in order. A `Blob` response that says "not found"
    /// or a request that times out moves on to the next name here, because a
    /// guild may hold peers that never downloaded the attachment.
    tried: Vec<PeerId>,
    hash: AttachmentId,
    /// Byte offset of the next chunk to request.
    offset: u64,
    /// Total length reported by the first response, once known.
    total: Option<u64>,
    data: Vec<u8>,
    reply: oneshot::Sender<std::result::Result<Vec<u8>, String>>,
}

/// The next connected guild peer that has not been asked for this blob yet.
///
/// Candidates are sorted by peer id so two runs of the same topology ask the
/// same peer first, which keeps a failed fetch reproducible in tests.
fn next_blob_peer(swarm: &Swarm<Behaviour>, state: &NodeState, tried: &[PeerId]) -> Option<PeerId> {
    let mut candidates: Vec<PeerId> = state
        .peer_guilds
        .keys()
        .copied()
        .filter(|peer| swarm.is_connected(peer) && !tried.contains(peer))
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// Point `pending` at a fresh peer, restarting the download from byte zero.
///
/// Returns the new request id, or `None` when every connected guild peer has
/// been tried, in which case `pending` is left untouched so the caller can fail
/// the fetch with the last error it received.
fn next_blob_request(
    swarm: &mut Swarm<Behaviour>,
    state: &NodeState,
    pending: &mut PendingBlob,
) -> Option<OutboundRequestId> {
    let peer = next_blob_peer(swarm, state, &pending.tried)?;
    pending.tried.push(peer);
    pending.peer = peer;
    pending.offset = 0;
    pending.total = None;
    pending.data.clear();
    let request = WireRequest::Blob {
        hash: pending.hash,
        offset: 0,
    };
    Some(swarm.behaviour_mut().rr.send_request(&peer, request))
}

fn start_blob_fetch(
    swarm: &mut Swarm<Behaviour>,
    state: &mut NodeState,
    store: &SharedStorage,
    hash: AttachmentId,
    reply: oneshot::Sender<std::result::Result<Vec<u8>, String>>,
) {
    match store.lock().expect("store mutex poisoned").get_blob(hash) {
        Ok(Some(bytes)) => {
            let _ = reply.send(Ok(bytes));
            return;
        }
        Ok(None) => {}
        Err(error) => {
            let _ = reply.send(Err(error.to_string()));
            return;
        }
    }
    let Some(peer) = next_blob_peer(swarm, state, &[]) else {
        let _ = reply.send(Err("no connected peer is in this commune".to_string()));
        return;
    };
    let mut pending = PendingBlob {
        peer,
        tried: Vec::new(),
        hash,
        offset: 0,
        total: None,
        data: Vec::new(),
        reply,
    };
    match next_blob_request(swarm, state, &mut pending) {
        Some(request_id) => {
            state.fetch_blobs.insert(request_id, pending);
        }
        None => {
            let _ = pending
                .reply
                .send(Err("no connected peer is in this commune".to_string()));
        }
    }
}

/// A delivery that failed and is waiting to be sent again.
struct PendingDelivery {
    at: Instant,
    peer: PeerId,
    event: SignedEvent,
    attempts: u32,
}

fn send_delivery(
    swarm: &mut Swarm<Behaviour>,
    state: &mut NodeState,
    peer: PeerId,
    event: SignedEvent,
    attempts: u32,
) {
    let request = WireRequest::Deliver {
        event: event.clone(),
    };
    let request_id = swarm.behaviour_mut().rr.send_request(&peer, request);
    state.deliveries.insert(request_id, (peer, event, attempts));
}

fn schedule_delivery_retry(state: &mut NodeState, peer: PeerId, event: SignedEvent, attempts: u32) {
    if attempts >= DELIVERY_MAX_ATTEMPTS {
        return;
    }
    let delay = DELIVERY_RETRY_BASE
        .saturating_mul(1u32 << attempts.min(4))
        .min(RECONNECT_MAX);
    state.delivery_retries.push(PendingDelivery {
        at: Instant::now() + delay,
        peer,
        event,
        attempts: attempts + 1,
    });
}

fn hello_in_flight(state: &NodeState, peer: &PeerId) -> bool {
    state.hello_in_flight.values().any(|value| value == peer)
}

/// Whether a Hello for `peer` can be sent without it possibly leaving over a
/// connection the peer dialled.
///
/// libp2p-request-response 0.29 picks the connection for a request by
/// `request_id % connections.len()` and has no API for targeting a specific
/// one. A Hello carries this node's guild id and listening addresses, and only
/// the dialer may introduce itself, so if the peer has both an inbound and an
/// outbound connection any send can leak them over the inbound one. With only
/// outbound connections every choice is safe. With only inbound connections
/// this node must not send at all: the peer's own Hello over the connection it
/// dialled is the proof.
fn hello_connection_is_safe(state: &NodeState, peer: &PeerId) -> bool {
    state.connections.get(peer).is_some_and(|connections| {
        !connections.outbound.is_empty() && connections.inbound.is_empty()
    })
}

/// What to do about proving the guild when a connection is established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelloAction {
    /// Send the first Hello now.
    Send,
    /// Do nothing: the peer's own Hello, another connection event, or a queued
    /// retry will settle the guild.
    Wait,
    /// Both directions are connected and neither side has proved anything.
    /// The larger peer id closes its inbound connections, leaving only the
    /// outbound ones, and queues a retry that fires once those closes are
    /// processed.
    CloseInboundThenSend,
}

/// Decide whether a freshly established connection should carry a Hello.
///
/// This is the whole first-contact policy in one place, because the sequence
/// of `ConnectionEstablished` events is not under this node's control:
///
/// * Only the dialer introduces itself, or any network neighbour could learn
///   the guild id by connecting.
/// * A send is safe only when every tracked connection is outbound; see
///   `hello_connection_is_safe`.
/// * If both directions are connected before either side's Hello is accepted,
///   both nodes would otherwise wait forever. The event that makes the set
///   ambiguous can be the inbound or the outbound one, so the decision cannot
///   depend on which side of the pair this event happens to be. Comparing peer
///   ids gives both nodes the same answer without another round trip: the
///   larger id closes its inbound connections and then sends over the
///   outbound ones, and the smaller id keeps waiting and answers that Hello.
fn hello_action(
    state: &NodeState,
    local_peer: PeerId,
    peer: PeerId,
    dialer: bool,
    num_established: u32,
) -> HelloAction {
    if state.peer_guilds.contains_key(&peer) || hello_in_flight(state, &peer) {
        return HelloAction::Wait;
    }
    let (has_inbound, has_outbound) = state
        .connections
        .get(&peer)
        .map(|connections| {
            (
                !connections.inbound.is_empty(),
                !connections.outbound.is_empty(),
            )
        })
        .unwrap_or((false, false));
    if has_outbound && !has_inbound {
        // Every remaining connection is one this node dialled, so any
        // connection request-response picks is safe.
        return if dialer {
            HelloAction::Send
        } else {
            HelloAction::Wait
        };
    }
    if num_established > 1 && has_inbound && has_outbound && local_peer > peer {
        return HelloAction::CloseInboundThenSend;
    }
    // Only an inbound connection, or this is the smaller half of an ambiguous
    // pair. The peer's Hello over the connection it dialled is the proof.
    HelloAction::Wait
}

fn sync_in_flight(state: &NodeState, peer: &PeerId) -> bool {
    state
        .pending_sync
        .values()
        .any(|pending| pending.peer == *peer)
}

fn send_hello(
    swarm: &mut Swarm<Behaviour>,
    state: &mut NodeState,
    guild_id: GuildId,
    listen_addrs: &HashSet<Multiaddr>,
    peer: &PeerId,
) {
    let request = WireRequest::Hello {
        guild_id,
        listen_addrs: hello_addrs(swarm, listen_addrs),
    };
    let request_id = swarm.behaviour_mut().rr.send_request(peer, request);
    state.hello_in_flight.insert(request_id, *peer);
    state.hello_retry.remove(peer);
}

fn start_sync(
    swarm: &mut Swarm<Behaviour>,
    state: &mut NodeState,
    guild_id: GuildId,
    peer: &PeerId,
    cursor: SyncCursor,
    page: u32,
) {
    let request = WireRequest::Sync {
        guild_id,
        cursor,
        limit: SYNC_BATCH,
    };
    // A pending retry is superseded by this walk; leaving it queued would start
    // a second walk once the first is still in flight.
    state.sync_retry.remove(peer);
    let request_id = swarm.behaviour_mut().rr.send_request(peer, request);
    state.pending_sync.insert(
        request_id,
        PendingSync {
            peer: *peer,
            cursor,
            page,
        },
    );
}

fn schedule_sync_retry(state: &mut NodeState, peer: PeerId, cursor: SyncCursor, page: u32) {
    let attempts = state.sync_attempts.entry(peer).or_insert(0);
    let delay = SYNC_RETRY_DELAY
        .saturating_mul(1u32 << (*attempts).min(4))
        .min(RECONNECT_MAX);
    *attempts = attempts.saturating_add(1);
    state.sync_retry.insert(
        peer,
        SyncRetry {
            at: Instant::now() + delay,
            cursor,
            page,
        },
    );
}

/// Send `event` to every peer in this guild except `source`.
///
/// Excluding the sender both avoids an immediate echo and guarantees that
/// forwarding terminates: a peer that already has the event imports it as a
/// duplicate and does not forward it again.
fn forward_event(
    swarm: &mut Swarm<Behaviour>,
    state: &mut NodeState,
    guild_id: GuildId,
    event: &SignedEvent,
    source: Option<PeerId>,
) {
    let targets: Vec<PeerId> = state
        .peer_guilds
        .iter()
        .filter(|(peer, guild)| **guild == guild_id && Some(**peer) != source)
        .map(|(peer, _)| *peer)
        .collect();
    for peer in targets {
        // Tracked so a rejected or lost delivery is retried instead of
        // silently losing the message until the periodic sweep.
        send_delivery(swarm, state, peer, event.clone(), 0);
    }
}

/// Where one control frame should go.
#[derive(Debug, PartialEq, Eq)]
enum ControlRoute {
    /// Deliver only to this peer. A relayed targeted frame produces an empty
    /// broadcast instead, so it is never fanned out again.
    Targeted(PeerId),
    /// Deliver to every peer in the guild except the source.
    Broadcast(Vec<PeerId>),
}

/// Decide the peers for a control frame without touching the swarm.
fn control_route(
    state: &NodeState,
    guild_id: GuildId,
    frame: &ControlFrame,
    source: Option<PeerId>,
) -> ControlRoute {
    // WebRTC signalling names one author. A relay that receives it has already
    // delivered it to its own daemon, so it must not fan it out to the guild.
    if let Some((_, to)) = frame.kind.rtc_target() {
        if source.is_none()
            && let Some(peer) = peer_id_for_author(to)
            && state.peer_guilds.get(&peer) == Some(&guild_id)
        {
            return ControlRoute::Targeted(peer);
        }
        return ControlRoute::Broadcast(Vec::new());
    }

    let mut targets: Vec<PeerId> = state
        .peer_guilds
        .iter()
        .filter(|(peer, guild)| **guild == guild_id && Some(**peer) != source)
        .map(|(peer, _)| *peer)
        .collect();
    targets.sort();
    ControlRoute::Broadcast(targets)
}

fn forward_control(
    swarm: &mut Swarm<Behaviour>,
    state: &NodeState,
    guild_id: GuildId,
    frame: &ControlFrame,
    source: Option<PeerId>,
) {
    match control_route(state, guild_id, frame, source) {
        ControlRoute::Targeted(peer) => send_control_to(swarm, peer, frame.clone()),
        ControlRoute::Broadcast(targets) => {
            for peer in targets {
                let request = WireRequest::Control {
                    frame: frame.clone(),
                };
                // Control frames are ephemeral: a lost one is replaced by the
                // next heartbeat, typing tick or roster snapshot, so there is
                // no retry.
                swarm.behaviour_mut().rr.send_request(&peer, request);
            }
        }
    }
}

/// Send one control frame to exactly one peer, with no forwarding.
fn send_control_to(swarm: &mut Swarm<Behaviour>, target: PeerId, frame: ControlFrame) {
    let request = WireRequest::Control { frame };
    // Signalling has its own restart path in the RTC manager, so a lost frame
    // is recovered by the next reconcile rather than an unbounded retry here.
    swarm.behaviour_mut().rr.send_request(&target, request);
}

/// Send one media frame to each target, with no retry and no bookkeeping.
///
/// Media is lossy: a frame that never arrives is replaced by the next one, so
/// unlike a `Deliver` there is no in-flight entry and no retry. Targets are
/// deduplicated and capped at [`MAX_MEDIA_TARGETS`]. A peer that is not
/// connected is still handed to libp2p, which will dial it if it can.
fn send_media(
    swarm: &mut Swarm<Behaviour>,
    channel_id: ChannelId,
    packet: MediaPacket,
    targets: Vec<PeerId>,
) {
    if !media_packet_is_for_call(channel_id, &packet) {
        // A caller that built a packet for another call must not put it on the
        // wire, where every receiver would refuse it and report a fault.
        trace_net!(
            "dropped media packet for channel {channel_id}: call id {} does not belong to it",
            packet.call_id
        );
        return;
    }
    for peer in media_targets(targets) {
        let request = WireRequest::Media {
            channel_id,
            packet: packet.clone(),
        };
        swarm.behaviour_mut().rr.send_request(&peer, request);
    }
}

/// Deduplicate and cap a media target list, keeping the caller's order.
fn media_targets(targets: Vec<PeerId>) -> Vec<PeerId> {
    let mut unique = Vec::new();
    for peer in targets {
        if unique.len() >= MAX_MEDIA_TARGETS {
            break;
        }
        if !unique.contains(&peer) {
            unique.push(peer);
        }
    }
    unique
}

/// Whether a media packet belongs to the call negotiated for `channel_id`.
///
/// The call id is derived from the channel, so this is the network layer's
/// whole routing check: a frame addressed to one channel cannot be replayed
/// into another.
fn media_packet_is_for_call(channel_id: ChannelId, packet: &MediaPacket) -> bool {
    packet.call_id == CallId::for_channel(channel_id)
}

/// Import an event from `source`, tell the frontend, and forward it.
///
/// Returns whether the event was new (`Ok(false)` for a duplicate). An event
/// the store rejects is an error for the sender too: reporting it as an `Ack`
/// would tell the sender the message arrived while it was thrown away.
fn import_and_announce(
    swarm: &mut Swarm<Behaviour>,
    state: &mut NodeState,
    store: &SharedStorage,
    event_tx: &mpsc::UnboundedSender<NodeEvent>,
    guild_id: GuildId,
    event: SignedEvent,
    source: Option<PeerId>,
) -> std::result::Result<bool, String> {
    match store
        .lock()
        .expect("store mutex poisoned")
        .import_event(event.clone())
    {
        Ok(imported) => {
            if imported {
                // Every event kind reaches the frontend: messages render,
                // member events update names and presence, channel events
                // update panes.
                let _ = event_tx.send(NodeEvent::Event(event.clone()));
                forward_event(swarm, state, guild_id, &event, source);
            }
            Ok(imported)
        }
        Err(error) => {
            let message = error.to_string();
            let from = source
                .map(|peer| format!(" from {peer}"))
                .unwrap_or_default();
            let _ = event_tx.send(NodeEvent::Error(format!("rejected event{from}: {message}")));
            Err(message)
        }
    }
}

fn run_maintenance(
    swarm: &mut Swarm<Behaviour>,
    state: &mut NodeState,
    guild_id: GuildId,
    listen_addrs: &HashSet<Multiaddr>,
    now: Instant,
) {
    let due_syncs: Vec<(PeerId, SyncCursor, u32)> = state
        .sync_retry
        .iter()
        .filter(|(_, retry)| retry.at <= now)
        .map(|(peer, retry)| (*peer, retry.cursor, retry.page))
        .collect();
    for (peer, cursor, page) in due_syncs {
        state.sync_retry.remove(&peer);
        let matches_guild = state.peer_guilds.get(&peer) == Some(&guild_id);
        if matches_guild && swarm.is_connected(&peer) && !sync_in_flight(state, &peer) {
            start_sync(swarm, state, guild_id, &peer, cursor, page);
        }
    }

    let due_hellos: Vec<PeerId> = state
        .hello_retry
        .iter()
        .filter(|(_, at)| **at <= now)
        .map(|(peer, _)| *peer)
        .collect();
    for peer in due_hellos {
        state.hello_retry.remove(&peer);
        if !swarm.is_connected(&peer)
            || state.peer_guilds.contains_key(&peer)
            || hello_in_flight(state, &peer)
        {
            continue;
        }
        if hello_connection_is_safe(state, &peer) {
            send_hello(swarm, state, guild_id, listen_addrs, &peer);
        } else {
            // Both directions are connected, or only an inbound connection
            // remains. request-response cannot target the outbound
            // connection, so sending now could introduce this node over the
            // connection the peer dialled. Keep the retry queued and let the
            // peer's own Hello prove it; if the inbound connection later
            // closes the retry becomes safe.
            state.hello_retry.insert(peer, now + SYNC_RETRY_DELAY);
        }
    }

    let (due_deliveries, later_deliveries): (Vec<_>, Vec<_>) =
        std::mem::take(&mut state.delivery_retries)
            .into_iter()
            .partition(|retry| retry.at <= now);
    state.delivery_retries = later_deliveries;
    for retry in due_deliveries {
        if state.peer_guilds.get(&retry.peer) == Some(&guild_id) && swarm.is_connected(&retry.peer)
        {
            send_delivery(swarm, state, retry.peer, retry.event, retry.attempts);
        } else if state.wanted.contains(&retry.peer) {
            // The peer is temporarily away. Hold the retry until it is back
            // rather than dropping a message it has never seen.
            state.delivery_retries.push(PendingDelivery {
                at: now + RECONNECT_MIN,
                ..retry
            });
        }
    }

    // Wrong-guild addresses are also pruned here, not only when the peer is
    // dialled again: a peer that is never seen again must not leave a record
    // behind for the life of the process.
    state.wrong_guild_addrs.retain(|_, records| {
        records.retain(|(_, until)| now < *until);
        !records.is_empty()
    });

    // A peer that answered from another guild's address gets a few more
    // attempts at its remaining addresses. This waits for the refused
    // connection to close first: a dial to a peer that is still connected is
    // rejected by the peer condition, not attempted.
    let due_retries: Vec<PeerId> = state
        .mismatch_retry
        .iter()
        .filter(|(_, (at, _))| *at <= now)
        .map(|(peer, _)| *peer)
        .collect();
    for peer in due_retries {
        let attempts = state
            .mismatch_retry
            .get(&peer)
            .map(|(_, attempts)| *attempts)
            .unwrap_or(0);
        if attempts >= MISMATCH_RETRIES {
            state.mismatch_retry.remove(&peer);
            continue;
        }
        if swarm.is_connected(&peer) {
            if state.peer_guilds.get(&peer) == Some(&guild_id) {
                // A connection that did complete Hello survived the refusal.
                state.mismatch_retry.remove(&peer);
            } else if !hello_in_flight(state, &peer) {
                // The connection is still open but was never proven: the Hello
                // that failed travelled over the refused connection. Ask again
                // over the one that remains, but only when every remaining
                // connection is one this node dialled. An ambiguous peer must
                // not receive an unsolicited Hello over a connection it
                // dialled; its own Hello is the proof. Re-evaluate after the
                // cooldown either way, since closing an inbound connection can
                // make the remaining outbound one safe.
                if hello_connection_is_safe(state, &peer) {
                    send_hello(swarm, state, guild_id, listen_addrs, &peer);
                }
                state
                    .mismatch_retry
                    .insert(peer, (now + RECONNECT_MIN, attempts));
            }
            continue;
        }
        state
            .mismatch_retry
            .insert(peer, (now + RECONNECT_MIN, attempts + 1));
        let Some(addresses) = state.known_addrs.get(&peer).cloned() else {
            continue;
        };
        let _ = dial_peer_addrs(swarm, state, peer, addresses, false);
    }

    // Redial wanted guild peers that went away. Backoff doubles up to a
    // minute so a machine that is switched off costs one failed dial a
    // minute, not a tight loop.
    let due_redials: Vec<PeerId> = state
        .reconnect_at
        .iter()
        .filter(|(peer, at)| {
            **at <= now && state.wanted.contains(peer) && !swarm.is_connected(peer)
        })
        .map(|(peer, _)| *peer)
        .collect();
    for peer in due_redials {
        let attempts = state.reconnect_attempts.entry(peer).or_insert(0);
        let delay = RECONNECT_MIN
            .saturating_mul(1u32 << (*attempts).min(4))
            .min(RECONNECT_MAX);
        *attempts = attempts.saturating_add(1);
        state.reconnect_at.insert(peer, now + delay);
        let Some(addresses) = state.known_addrs.get(&peer).cloned() else {
            continue;
        };
        if addresses.is_empty() {
            continue;
        }
        let _ = dial_peer_addrs(swarm, state, peer, addresses, false);
    }
}

fn resync_all(swarm: &mut Swarm<Behaviour>, state: &mut NodeState, guild_id: GuildId) {
    let peers: Vec<PeerId> = state
        .peer_guilds
        .iter()
        .filter(|(_, guild)| **guild == guild_id)
        .map(|(peer, _)| *peer)
        .collect();
    for peer in peers {
        // A walk that is in flight or waiting to resume already owns the
        // cursor; restarting it from `sync_heads` here would discard the
        // progress it made.
        if !swarm.is_connected(&peer)
            || sync_in_flight(state, &peer)
            || state.sync_retry.contains_key(&peer)
        {
            continue;
        }
        let cursor = state
            .sync_heads
            .get(&peer)
            .copied()
            .unwrap_or_else(SyncCursor::start);
        start_sync(swarm, state, guild_id, &peer, cursor, 0);
    }
}

fn benign_dial_error(error: &DialError) -> bool {
    matches!(
        error,
        DialError::DialPeerConditionFalse(_) | DialError::Aborted | DialError::NoAddresses
    )
}

#[allow(clippy::too_many_arguments)]
fn handle_request_response(
    swarm: &mut Swarm<Behaviour>,
    store: &SharedStorage,
    event_tx: &mpsc::UnboundedSender<NodeEvent>,
    guild_id: GuildId,
    listen_addrs: &HashSet<Multiaddr>,
    state: &mut NodeState,
    event: request_response::Event<WireRequest, WireResponse>,
) {
    match event {
        request_response::Event::Message {
            peer,
            connection_id,
            message,
        } => match message {
            request_response::Message::Request {
                request, channel, ..
            } => {
                let (response, close) = handle_request(
                    swarm,
                    store,
                    event_tx,
                    guild_id,
                    listen_addrs,
                    state,
                    &peer,
                    connection_id,
                    request,
                );
                let sent = swarm
                    .behaviour_mut()
                    .rr
                    .send_response(channel, response)
                    .is_ok();
                if close && sent {
                    // Close only the connection that was refused, and only
                    // once the refusal has actually been flushed. Calling
                    // `close_connection` here tears the connection down before
                    // the queued response leaves, so the dialer sees a dropped
                    // connection instead of "guild mismatch" and retries the
                    // same address forever. `ResponseSent` marks the flush.
                    state.pending_close.insert(connection_id);
                }
            }
            request_response::Message::Response {
                request_id,
                response,
            } => {
                handle_response(
                    swarm,
                    store,
                    event_tx,
                    guild_id,
                    state,
                    connection_id,
                    request_id,
                    peer,
                    response,
                );
            }
        },
        request_response::Event::OutboundFailure {
            connection_id,
            request_id,
            peer,
            error,
            ..
        } => {
            let was_hello = state.hello_in_flight.remove(&request_id).is_some();
            if let Some(mut pending) = state.fetch_blobs.remove(&request_id) {
                // A peer that timed out or dropped mid-download may simply not
                // have the blob, so ask the next guild peer before failing the
                // caller. Every partial chunk is discarded: the download
                // restarts from zero on the new peer.
                match next_blob_request(swarm, state, &mut pending) {
                    Some(request_id) => {
                        state.fetch_blobs.insert(request_id, pending);
                    }
                    None => {
                        let _ = pending
                            .reply
                            .send(Err(format!("blob fetch from {peer} failed: {error}")));
                    }
                }
            } else if let Some(pending) = state.pending_sync.remove(&request_id) {
                // A failed page is not data loss: retry from the same cursor.
                // Only report it while the peer is still reachable; a peer
                // that disconnected will re-sync through Hello on reconnect.
                if swarm.is_connected(&peer) {
                    state.sync_retry.insert(
                        peer,
                        SyncRetry {
                            at: Instant::now() + SYNC_RETRY_DELAY,
                            cursor: pending.cursor,
                            page: pending.page,
                        },
                    );
                    let _ = event_tx.send(NodeEvent::Error(format!(
                        "sync with {peer} interrupted ({error}); retrying"
                    )));
                }
            } else if was_hello {
                let unsupported = matches!(
                    &error,
                    request_response::OutboundFailure::UnsupportedProtocols
                );
                if unsupported {
                    // An unrelated libp2p node (for example one found by
                    // mDNS) will never answer this protocol. Drop this
                    // connection and, if the peer has never proved this
                    // guild, reject the peer for the short handshake
                    // cooldown. That peer-level record is what keeps
                    // discovery from redialling the address, so no
                    // wrong-guild address record is needed here; this is not
                    // a guild refusal and must not use the ten-minute
                    // mismatch quarantine. Closing only this connection
                    // matters: one daemon serves every guild from a single
                    // peer id, so a peer-level disconnect would also tear
                    // down the connection that serves this guild on another
                    // port.
                    state.hello_retry.remove(&peer);
                    state.hello_attempts.remove(&peer);
                    let _ = swarm.close_connection(connection_id);
                    reject_peer_unless_proven(state, peer);
                } else if swarm.is_connected(&peer) {
                    let attempts = state.hello_attempts.entry(peer).or_insert(0);
                    *attempts = attempts.saturating_add(1);
                    if *attempts >= HELLO_MAX_ATTEMPTS {
                        let _ = event_tx.send(NodeEvent::Error(format!(
                            "giving up on the handshake with {peer} after {attempts} attempts"
                        )));
                        state.hello_retry.remove(&peer);
                        // A timeout or a dropped stream says nothing about
                        // the guild the address serves, so it is not recorded
                        // as a wrong-guild address. Close this connection and
                        // apply the short handshake cooldown instead of the
                        // ten-minute mismatch quarantine.
                        let _ = swarm.close_connection(connection_id);
                        reject_peer_unless_proven(state, peer);
                    } else {
                        state
                            .hello_retry
                            .insert(peer, Instant::now() + SYNC_RETRY_DELAY);
                    }
                } else {
                    // The peer is gone; ConnectionClosed schedules the redial.
                    state.hello_retry.remove(&peer);
                }
            } else if let Some((peer, event, attempts)) = state.deliveries.remove(&request_id) {
                // A delivery that never got an answer is retried; the periodic
                // resync is the backstop if every attempt fails.
                schedule_delivery_retry(state, peer, event, attempts);
            }
        }
        request_response::Event::InboundFailure { peer, error, .. } => {
            // A peer hanging up mid-request, or one that does not speak the
            // protocol, is normal churn in a swarm; neither is actionable.
            if !matches!(
                error,
                request_response::InboundFailure::ConnectionClosed
                    | request_response::InboundFailure::UnsupportedProtocols
            ) {
                let _ = event_tx.send(NodeEvent::Error(format!(
                    "request from {peer} failed: {error}"
                )));
            }
        }
        request_response::Event::ResponseSent { connection_id, .. } => {
            // The refusal is on the wire now, so the connection can go. Left
            // open, a peer that never closes would hold it until the 120 s idle
            // timeout; closed earlier, the refusal is lost and the peer retries.
            if state.pending_close.remove(&connection_id) {
                let _ = swarm.close_connection(connection_id);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_request(
    swarm: &mut Swarm<Behaviour>,
    store: &SharedStorage,
    event_tx: &mpsc::UnboundedSender<NodeEvent>,
    guild_id: GuildId,
    listen_addrs: &HashSet<Multiaddr>,
    state: &mut NodeState,
    peer: &PeerId,
    connection_id: ConnectionId,
    request: WireRequest,
) -> (WireResponse, bool) {
    match request {
        WireRequest::Hello {
            guild_id: peer_guild,
            listen_addrs: peer_addrs,
        } => {
            if peer_guild != guild_id {
                // Do not answer a guild probe with our own guild id. Two
                // nodes in different guilds must not keep a connection alive:
                // every sync or delivery between them would fail. Only the
                // address is refused, though: the same peer id can serve this
                // guild on another listening port, so the peer stays dialable
                // and only the ports it announced here are set aside.
                trace_net!(
                    "[{}] hello from {peer} names guild {peer_guild}, this node serves {guild_id}",
                    state.tag
                );
                mark_wrong_guild(state, *peer, parse_addrs(&peer_addrs));
                return (
                    WireResponse::Error {
                        message: "guild mismatch".to_string(),
                    },
                    true,
                );
            }
            trace_net!(
                "[{}] hello from {peer} accepted for guild {guild_id}",
                state.tag
            );
            state.peer_guilds.insert(*peer, peer_guild);
            state
                .proven_connections
                .entry(*peer)
                .or_default()
                .insert(connection_id);
            state.rejected.remove(peer);
            state.mismatch_retry.remove(peer);
            state.wanted.insert(*peer);
            // A completed Hello is the signal that a peer is healthy, so the
            // reconnect and handshake backoffs start fresh, exactly as in the
            // response path.
            state.reconnect_at.remove(peer);
            state.reconnect_attempts.remove(peer);
            state.hello_attempts.remove(peer);
            // The Hello arrived over a connection the peer dialled, so its
            // remote address is the peer's ephemeral source port, not a
            // listening address. Only the announced listeners below go into
            // Kademlia; adding the source port would teach the routing table
            // an address that cannot be dialled.
            //
            // A Hello is the peer's own account of where it listens now, so it
            // replaces whatever was remembered before. After a restart on an
            // ephemeral port the old list is every address the peer no longer
            // answers on, and a reconnect that prefers it never reaches the
            // peer again.
            replace_known_addrs(swarm, state, *peer, parse_addrs(&peer_addrs), None);
            if !state.bootstrapped {
                state.bootstrapped = true;
                let _ = swarm.behaviour_mut().kad.bootstrap();
            }
            state.announce_pending = true;
            if !sync_in_flight(state, peer) {
                start_sync(swarm, state, guild_id, peer, SyncCursor::start(), 0);
            }
            (
                WireResponse::Hello {
                    guild_id,
                    listen_addrs: hello_addrs(swarm, listen_addrs),
                },
                false,
            )
        }
        WireRequest::Sync {
            guild_id: requested_guild,
            cursor,
            limit,
        } => {
            if requested_guild != guild_id {
                return (
                    WireResponse::Error {
                        message: "guild mismatch".to_string(),
                    },
                    false,
                );
            }
            let limit = (limit.max(1) as usize).min(SYNC_BATCH as usize);
            match store.lock().expect("store mutex poisoned").events_after(
                guild_id,
                cursor,
                limit,
                SYNC_MAX_BYTES,
            ) {
                Ok(page) => (
                    WireResponse::Events {
                        events: page.events,
                        more: page.more,
                    },
                    false,
                ),
                Err(error) => (
                    WireResponse::Error {
                        message: error.to_string(),
                    },
                    false,
                ),
            }
        }
        WireRequest::Control { frame } => {
            if frame.guild_id != guild_id {
                return (
                    WireResponse::Error {
                        message: "guild mismatch".to_string(),
                    },
                    false,
                );
            }
            let now = now_ms();
            if let Err(error) = frame.verify().and_then(|()| frame.check_freshness(now)) {
                return (
                    WireResponse::Error {
                        message: format!("invalid control frame: {error}"),
                    },
                    false,
                );
            }
            if let Err(error) = state.control_replay.check_and_record(&frame, now) {
                return (
                    WireResponse::Error {
                        message: format!("control frame rejected: {error}"),
                    },
                    false,
                );
            }
            forward_control(swarm, state, guild_id, &frame, Some(*peer));
            let _ = event_tx.send(NodeEvent::Control(frame));
            (WireResponse::Ack, false)
        }
        WireRequest::Blob { hash, offset } => {
            // A peer that has not completed Hello is not known to be in this
            // guild, so it must not learn whether a given blob exists here.
            if state.peer_guilds.get(peer) != Some(&guild_id) {
                return (
                    WireResponse::Error {
                        message: "commune not verified".to_string(),
                    },
                    false,
                );
            }
            match store.lock().expect("store mutex poisoned").get_blob(hash) {
                Ok(Some(bytes)) => {
                    let total = bytes.len() as u64;
                    if offset > total {
                        return (
                            WireResponse::Error {
                                message: format!("blob offset {offset} past {total}"),
                            },
                            false,
                        );
                    }
                    let start = offset as usize;
                    let end = start.saturating_add(BLOB_CHUNK_BYTES).min(bytes.len());
                    (
                        WireResponse::Blob {
                            hash,
                            offset,
                            total,
                            data: bytes[start..end].to_vec(),
                        },
                        false,
                    )
                }
                Ok(None) => (
                    WireResponse::Error {
                        message: "blob not found".to_string(),
                    },
                    false,
                ),
                Err(error) => (
                    WireResponse::Error {
                        message: error.to_string(),
                    },
                    false,
                ),
            }
        }
        WireRequest::Media { channel_id, packet } => {
            // The packet is not signed at this layer, but the call it claims
            // to belong to is checked against the channel the sender named.
            // A mismatch is a routing bug or a replay into the wrong call.
            if !media_packet_is_for_call(channel_id, &packet) {
                return (
                    WireResponse::Error {
                        message: "media packet belongs to another call".to_string(),
                    },
                    false,
                );
            }
            let _ = event_tx.send(NodeEvent::Media {
                channel_id,
                packet,
                from: *peer,
            });
            (WireResponse::Ack, false)
        }
        WireRequest::Deliver { event } => {
            if event.body.guild_id != guild_id {
                return (
                    WireResponse::Error {
                        message: "guild mismatch".to_string(),
                    },
                    false,
                );
            }
            match import_and_announce(swarm, state, store, event_tx, guild_id, event, Some(*peer)) {
                // Ok includes duplicates: delivery is idempotent.
                Ok(_) => (WireResponse::Ack, false),
                // Tell the sender the event was rejected so it can resync
                // from the peer that actually has it, instead of assuming the
                // message landed.
                Err(message) => (WireResponse::Error { message }, false),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_response(
    swarm: &mut Swarm<Behaviour>,
    store: &SharedStorage,
    event_tx: &mpsc::UnboundedSender<NodeEvent>,
    guild_id: GuildId,
    state: &mut NodeState,
    connection_id: ConnectionId,
    request_id: OutboundRequestId,
    peer: PeerId,
    response: WireResponse,
) {
    state.hello_in_flight.remove(&request_id);
    let pending = state.pending_sync.remove(&request_id);
    let delivered = state.deliveries.remove(&request_id);
    let fetching = state.fetch_blobs.remove(&request_id);

    match response {
        WireResponse::Hello {
            guild_id: peer_guild,
            listen_addrs,
        } => {
            if peer_guild != guild_id {
                // The responder served another guild on the address this
                // connection reached. Its announced listen addresses are its
                // own ports, so they are all wrong for this guild, and so is
                // the address the connection was opened to. Set those aside and
                // try the peer's other addresses shortly; the peer itself is
                // not refused, because the same daemon may serve this guild on
                // a different port.
                let mut wrong = parse_addrs(&listen_addrs);
                if let Some(address) = state.connection_addrs.get(&connection_id).cloned() {
                    wrong.push(address);
                }
                mark_wrong_guild(state, peer, wrong);
                trace_net!(
                    "[{}] hello from {peer} answered guild {peer_guild}, this node serves {guild_id}",
                    state.tag
                );
                // Discovery finds every ovvc node on the LAN, not only the ones
                // in this guild, so a mismatch is routine and cannot be fixed
                // from the frontend. Only a peer this node was explicitly told
                // to reach, and that has never proved this guild, is worth
                // showing: that is a wrong invite or dial address, and the user
                // can correct it.
                if state.wanted.contains(&peer) && !state.peer_guilds.contains_key(&peer) {
                    let _ = event_tx.send(NodeEvent::Error(format!(
                        "{peer} serves a different commune at that address; trying its other addresses"
                    )));
                }
                let _ = swarm.close_connection(connection_id);
                schedule_mismatch_retry(state, peer);
                return;
            }
            state.peer_guilds.insert(peer, peer_guild);
            state
                .proven_connections
                .entry(peer)
                .or_default()
                .insert(connection_id);
            state.rejected.remove(&peer);
            state.mismatch_retry.remove(&peer);
            state.wanted.insert(peer);
            state.reconnect_at.remove(&peer);
            state.reconnect_attempts.remove(&peer);
            // A completed Hello is the signal that a peer is healthy, so its
            // reconnect backoff starts fresh.
            state.hello_attempts.remove(&peer);
            // The response is the responder's own account of where it listens,
            // so it replaces the addresses remembered from before. A reconnect
            // after the responder restarts must dial the new ports, not a full
            // list of the old ones.
            //
            // The address that completed Hello is kept outside the announced
            // cap. `replace_known_addrs` removes every old known address (and
            // its Kademlia entry) that is not in the new set, and the
            // responder may not announce the exact port or interface this
            // connection reached (NAT, a relay, a single-interface listener).
            // It must survive even when the peer announced a full list of
            // higher-ranked addresses.
            let proving = state.connection_addrs.get(&connection_id).cloned();
            replace_known_addrs(swarm, state, peer, parse_addrs(&listen_addrs), proving);
            if !state.bootstrapped {
                state.bootstrapped = true;
                let _ = swarm.behaviour_mut().kad.bootstrap();
            }
            state.announce_pending = true;
            if !sync_in_flight(state, &peer) {
                start_sync(swarm, state, guild_id, &peer, SyncCursor::start(), 0);
            }
        }
        WireResponse::Events { events, more } => {
            let Some(pending) = pending else {
                // A late page from a walk we already gave up on.
                return;
            };
            let mut cursor = pending.cursor;
            let mut usable = 0u32;
            let mut blocked = false;
            for event in events {
                if event.body.guild_id != guild_id {
                    let _ = event_tx.send(NodeEvent::Error(format!(
                        "ignored event for a different commune from {peer}"
                    )));
                    continue;
                }
                let next = match SyncCursor::after(&event) {
                    Ok(next) => next,
                    Err(error) => {
                        let _ = event_tx.send(NodeEvent::Error(format!(
                            "ignored malformed event from {peer}: {error}"
                        )));
                        continue;
                    }
                };
                let Ok(event_id) = event.id() else {
                    // `SyncCursor::after` already rejects undecodable events,
                    // so this is unreachable in practice.
                    continue;
                };
                match import_and_announce(
                    swarm,
                    state,
                    store,
                    event_tx,
                    guild_id,
                    event,
                    Some(peer),
                ) {
                    Ok(_) => {
                        state.sync_rejects.remove(&event_id);
                        cursor = next;
                        usable += 1;
                    }
                    Err(_) => {
                        let attempts = state.sync_rejects.entry(event_id).or_insert(0);
                        *attempts = attempts.saturating_add(1);
                        if *attempts >= SYNC_REJECT_MAX_ATTEMPTS {
                            // The store keeps refusing this event. Holding the
                            // cursor here would block every later event from
                            // this peer, so skip it having reported each
                            // attempt. A later resync can still re-fetch it.
                            state.sync_rejects.remove(&event_id);
                            cursor = next;
                            usable += 1;
                        } else {
                            // Do not advance past an event the store refused:
                            // the rejection may be transient, and the retry
                            // must offer it again.
                            blocked = true;
                            break;
                        }
                    }
                }
            }

            if usable > 0 && !blocked {
                state.sync_attempts.remove(&peer);
            }

            if blocked {
                // Resume from the last accepted event so the refused one is
                // retried instead of being left behind the cursor.
                schedule_sync_retry(state, peer, cursor, pending.page);
            } else if more {
                if usable == 0 {
                    // The peer claims more but sent nothing we can advance
                    // past. Retry the same cursor later instead of spinning.
                    schedule_sync_retry(state, peer, pending.cursor, pending.page);
                } else if pending.page + 1 >= SYNC_MAX_PAGES {
                    // Safety valve: pause a walk that is taking a very long
                    // time, then continue from where it stopped. Restarting
                    // from zero (as this used to) means a history larger than
                    // the page budget never finishes syncing at all.
                    schedule_sync_retry(state, peer, cursor, 0);
                } else if !sync_in_flight(state, &peer) {
                    start_sync(swarm, state, guild_id, &peer, cursor, pending.page + 1);
                }
            } else {
                state.sync_heads.insert(peer, cursor);
                state.sync_retry.remove(&peer);
                state.sync_attempts.remove(&peer);
                state.synced.insert(peer);
                let _ = event_tx.send(NodeEvent::Synced(peer));
            }
        }
        WireResponse::Blob {
            hash,
            offset,
            total,
            data,
        } => {
            let Some(mut pending) = fetching else {
                // A late chunk from a fetch whose caller went away.
                return;
            };
            let fail = |pending: PendingBlob, message: String| {
                let _ = pending.reply.send(Err(message));
            };
            if hash != pending.hash || offset != pending.offset {
                fail(
                    pending,
                    format!("blob response for {hash} at {offset} did not match the request"),
                );
                return;
            }
            if total > MAX_ATTACHMENT_BYTES {
                fail(
                    pending,
                    format!("blob is {total} bytes, over the {MAX_ATTACHMENT_BYTES} limit"),
                );
                return;
            }
            if let Some(known) = pending.total
                && known != total
            {
                fail(
                    pending,
                    format!("blob size changed mid-download: {known} then {total}"),
                );
                return;
            }
            pending.total = Some(total);
            if data.len() as u64 > BLOB_CHUNK_BYTES as u64
                || pending.data.len() as u64 + data.len() as u64 > total
                || (data.is_empty() && total > 0)
            {
                fail(
                    pending,
                    format!("blob chunk at {offset} is not usable ({total} byte blob)"),
                );
                return;
            }
            pending.data.extend_from_slice(&data);
            if pending.data.len() as u64 == total {
                if hash_bytes(&pending.data) != hash {
                    fail(pending, "blob contents do not match its hash".to_string());
                    return;
                }
                if let Err(error) = store
                    .lock()
                    .expect("store mutex poisoned")
                    .put_blob(hash, &pending.data)
                {
                    let _ = event_tx.send(NodeEvent::Error(format!(
                        "fetched blob {hash} could not be cached: {error}"
                    )));
                }
                let _ = pending.reply.send(Ok(pending.data));
                return;
            }
            // More to come. Request the next chunk from the same peer: it
            // proved it has the blob, and switching peers mid-download would
            // only make partial progress harder to reason about.
            pending.offset = pending.data.len() as u64;
            let request = WireRequest::Blob {
                hash,
                offset: pending.offset,
            };
            let request_id = swarm
                .behaviour_mut()
                .rr
                .send_request(&pending.peer, request);
            state.fetch_blobs.insert(request_id, pending);
        }
        WireResponse::Ack => {}
        WireResponse::Error { message } => {
            let mut retried_blob = false;
            if let Some(mut pending) = fetching {
                match next_blob_request(swarm, state, &mut pending) {
                    Some(request_id) => {
                        state.fetch_blobs.insert(request_id, pending);
                        retried_blob = true;
                    }
                    None => {
                        let _ = pending.reply.send(Err(message.clone()));
                    }
                }
            }
            // A retried blob fetch is a normal miss, not a guild-level fault,
            // so it must not reach the daemon as an error line. The same goes
            // for a guild mismatch: one daemon serves every guild it hosts
            // from a single peer id, request-response may route a request over
            // any connection to that peer, and this one reached a port of
            // another guild. The request is retried over the peer's other
            // addresses, so the refusal belongs in the trace, not in a banner
            // the user can do nothing about. A peer the user asked for, which
            // has never proved this guild, is the exception: its invite or
            // dial address really is wrong.
            let guild_mismatch = message.contains("guild mismatch");
            let worth_showing = !retried_blob
                && (!guild_mismatch
                    || (state.wanted.contains(&peer) && !state.peer_guilds.contains_key(&peer)));
            if worth_showing {
                let _ = event_tx.send(NodeEvent::Error(format!("{peer}: {message}")));
            }
            if guild_mismatch {
                // The connection this request travelled over reached a node of
                // another guild, which means the address behind it is one this
                // guild does not answer on. Set that address aside and close
                // only this connection: the peer may hold a good one too.
                // Retrying the same address cannot work, but the peer's other
                // addresses may, so the request is retried through the normal
                // path rather than dropped.
                let address = state.connection_addrs.get(&connection_id).cloned();
                trace_net!(
                    "[{}] {peer} answered guild mismatch over {address:?}",
                    state.tag
                );
                if let Some(address) = address {
                    mark_wrong_guild(state, peer, [address]);
                }
                let _ = swarm.close_connection(connection_id);
                state.sync_retry.remove(&peer);
                match (pending, delivered) {
                    (Some(pending), _) => {
                        schedule_sync_retry(state, peer, pending.cursor, pending.page)
                    }
                    (None, Some((delivered_peer, event, attempts))) => {
                        schedule_delivery_retry(state, delivered_peer, event, attempts)
                    }
                    (None, None) => schedule_mismatch_retry(state, peer),
                }
            } else if let Some(pending) = pending {
                schedule_sync_retry(state, peer, pending.cursor, pending.page);
            } else if let Some(delivery) = delivered {
                let (delivered_peer, event, attempts) = delivery;
                schedule_delivery_retry(state, delivered_peer, event, attempts);
            }
        }
    }
}

fn handle_kad_event(
    swarm: &mut Swarm<Behaviour>,
    event_tx: &mpsc::UnboundedSender<NodeEvent>,
    state: &mut NodeState,
    event: kad::Event,
) {
    let kad::Event::OutboundQueryProgressed { id, result, .. } = event else {
        return;
    };

    match result {
        kad::QueryResult::GetRecord(Ok(kad::GetRecordOk::FoundRecord(peer_record))) => {
            let Some(author) = state.lookups.remove(&id) else {
                return;
            };
            let announcement = match Announcement::decode(&peer_record.record.value) {
                Ok(announcement) => announcement,
                Err(error) => {
                    let _ = event_tx.send(NodeEvent::LookupFailed {
                        author,
                        reason: format!("invalid announcement: {error}"),
                    });
                    return;
                }
            };
            if let Err(error) = announcement.verify(author) {
                let _ = event_tx.send(NodeEvent::LookupFailed {
                    author,
                    reason: format!("unverifiable announcement: {error}"),
                });
                return;
            }
            let Some(peer) = peer_id_for_author(author) else {
                let _ = event_tx.send(NodeEvent::LookupFailed {
                    author,
                    reason: "author key is not a valid peer identity".to_string(),
                });
                return;
            };
            let addresses = parse_dialable(&announcement.body.addresses);
            let _ = dial_peer_addrs(swarm, state, peer, addresses.iter().cloned(), false);
            let _ = event_tx.send(NodeEvent::PeerFound { author, addresses });
        }
        kad::QueryResult::GetRecord(Ok(kad::GetRecordOk::FinishedWithNoAdditionalRecord {
            ..
        })) => {
            if let Some(author) = state.lookups.remove(&id) {
                let _ = event_tx.send(NodeEvent::LookupFailed {
                    author,
                    reason: "no announcement found".to_string(),
                });
            }
        }
        kad::QueryResult::GetRecord(Err(error)) => {
            if let Some(author) = state.lookups.remove(&id) {
                let _ = event_tx.send(NodeEvent::LookupFailed {
                    author,
                    reason: error.to_string(),
                });
            }
        }
        kad::QueryResult::PutRecord(Err(error)) => {
            let _ = event_tx.send(NodeEvent::Error(format!(
                "publishing peer announcement failed: {error}"
            )));
        }
        _ => {}
    }
}

fn record_key_for_author(author: AuthorId) -> kad::RecordKey {
    kad::RecordKey::new(author.as_bytes())
}

fn publish_announcement(
    swarm: &mut Swarm<Behaviour>,
    author: &Identity,
    listen_addrs: &HashSet<Multiaddr>,
    state: &NodeState,
) -> bool {
    // A connected peer is not enough: it only counts once Hello has shown that
    // it serves this guild. An unverified peer has no routing entry, so a put
    // here would query nobody and report QuorumFailed to the frontend, which
    // reads as a fault on a machine that is simply alone.
    if state.peer_guilds.is_empty() {
        return false;
    }
    let external: HashSet<String> = swarm
        .external_addresses()
        .map(ToString::to_string)
        .collect();
    let mut addresses: Vec<String> = listen_addrs.iter().map(ToString::to_string).collect();
    addresses.extend(external.iter().cloned());
    addresses = ranked_addresses(addresses, &external);
    if addresses.is_empty() {
        return false;
    }

    let Ok(announcement) = Announcement::sign(author, addresses, ANNOUNCE_TTL_SECS) else {
        return false;
    };
    let Ok(value) = announcement.encode() else {
        return false;
    };
    let mut record = kad::Record::new(record_key_for_author(author.author_id()), value);
    record.expires = Some(Instant::now() + Duration::from_secs(ANNOUNCE_TTL_SECS));
    swarm
        .behaviour_mut()
        .kad
        .put_record(record, kad::Quorum::One)
        .is_ok()
}

fn parse_dialable(values: &[String]) -> Vec<Multiaddr> {
    let mut seen = HashSet::new();
    let mut scored: Vec<(i32, Multiaddr)> = values
        .iter()
        .filter_map(|address| address.parse::<Multiaddr>().ok())
        .filter(is_dialable_addr)
        .filter(|address| seen.insert(address.to_string()))
        .map(|address| (address_score(&address, false), address))
        .collect();
    scored.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.to_string().cmp(&right.1.to_string()))
    });
    scored
        .into_iter()
        .take(MAX_ANNOUNCED_ADDRESSES)
        .map(|(_, address)| address)
        .collect()
}

/// Parse the addresses a Hello announced.
///
/// No cap is applied here. A peer can put junk or unusable addresses in the
/// first entries and a real listener last, so filtering must happen before
/// truncation: `replace_known_addrs` filters, dedupes, ranks and only then
/// truncates to `MAX_ANNOUNCED_ADDRESSES`, and `dial_peer_addrs` does the same
/// for dial candidates.
fn parse_addrs(values: &[String]) -> Vec<Multiaddr> {
    values
        .iter()
        .filter_map(|address| address.parse::<Multiaddr>().ok())
        .collect()
}

/// Stop treating a peer as a guild member until `HELLO_FAILURE_COOLDOWN` has
/// passed. It never answered the handshake or does not speak this protocol, so
/// a reconnect can only fail again for a while. This short cooldown is not the
/// guild-mismatch quarantine: a timeout does not identify the guild the
/// address serves. Guild mismatches are not handled here: one daemon can serve
/// several guilds from one peer id, and only the address that refused is at
/// fault. See `mark_wrong_guild`.
///
/// The peer itself and its addresses are kept. One daemon serves every guild
/// from one peer id on its own port, so an address that timed out says nothing
/// about the peer's other ports. Dropping `wanted` and `known_addrs` here took
/// a healthy port out of the automatic redial loop until mDNS happened to find
/// the peer again, and an explicitly wanted peer never came back at all. The
/// cooldown only postpones the next automatic dial: discovery may still try
/// another port immediately, and `due_redials` resumes from `known_addrs` when
/// `reconnect_at` arrives.
fn reject_peer(state: &mut NodeState, peer: PeerId) {
    state.peer_guilds.remove(&peer);
    state.proven_connections.remove(&peer);
    state.mismatch_retry.remove(&peer);
    let until = Instant::now() + HELLO_FAILURE_COOLDOWN;
    state.rejected.insert(peer, until);
    state.reconnect_at.insert(peer, until);
}

/// Reject a peer that never proved this guild, unless another connection has
/// already proved it.
///
/// Proof is peer-wide, so a failure on one address must not discard the guild
/// state of a healthy connection on another port of the same peer id.
fn reject_peer_unless_proven(state: &mut NodeState, peer: PeerId) {
    if !state.peer_guilds.contains_key(&peer) {
        reject_peer(state, peer);
    }
}

/// Remember that these addresses belong to a node of another guild.
///
/// Called with what the peer said about itself (its own listening addresses)
/// as well as, where it is known, the address the refusing connection reached.
/// Records expire after `MISMATCH_QUARANTINE`: a port that serves another guild
/// today may serve this one after a configuration change, and a peer that is
/// later invited here must not be blocked for good.
fn mark_wrong_guild(
    state: &mut NodeState,
    peer: PeerId,
    addresses: impl IntoIterator<Item = Multiaddr>,
) {
    let now = Instant::now();
    let until = now + MISMATCH_QUARANTINE;
    let records = state.wrong_guild_addrs.entry(peer).or_default();
    records.retain(|(_, expiry)| now < *expiry);
    for address in addresses {
        let address = without_peer(&address);
        if address.is_empty() {
            continue;
        }
        if let Some(record) = records.iter_mut().find(|(known, _)| *known == address) {
            record.1 = until;
        } else if records.len() < MAX_WRONG_GUILD_ADDRS {
            records.push((address, until));
        }
    }
}

/// Whether this exact address recently answered for another guild, pruning
/// expired records on the way through.
fn wrong_guild_at(state: &mut NodeState, peer: PeerId, address: &Multiaddr) -> bool {
    let now = Instant::now();
    let mut empty = false;
    let found = match state.wrong_guild_addrs.get_mut(&peer) {
        Some(records) => {
            records.retain(|(_, until)| now < *until);
            empty = records.is_empty();
            records.iter().any(|(known, _)| known == address)
        }
        None => false,
    };
    if empty {
        state.wrong_guild_addrs.remove(&peer);
    }
    found
}

/// Arrange one more attempt at a peer that answered from another guild's
/// address. The delay lets the refused connection close first, so the redial
/// is not rejected as still connected.
fn schedule_mismatch_retry(state: &mut NodeState, peer: PeerId) {
    if state.known_addrs.get(&peer).is_none_or(Vec::is_empty) {
        // Nothing known to try, and discovery will call `dial_peer_addrs` by
        // itself when it learns something new.
        return;
    }
    state
        .mismatch_retry
        .entry(peer)
        .or_insert_with(|| (Instant::now() + RECONNECT_MIN, 0));
}

/// Replace a peer's remembered addresses with the listening addresses it
/// announced over a connection that completed Hello.
///
/// A Hello comes from the peer itself and names its current listeners, so at
/// the moment it is accepted it replaces everything learned earlier. That
/// matters most after a restart: a node that listens on an ephemeral port
/// comes back on a new one, and the addresses remembered from before the
/// restart are dead. Kept alongside the new ones they fill the capped list and
/// become the only thing a reconnect has to dial, which is how a restarted
/// machine ended up unreachable while mDNS kept finding it. The replaced
/// addresses leave the routing table too, or every DHT query would keep
/// dialling them.
///
/// The replacement is not permanent, and it is not a promise that the list
/// stays equal to the Hello's announcement. Discovery and DHT announcements
/// merge addresses back in through `dial_peer_addrs`, which appends to this
/// list and re-adds them to the routing table. That is deliberate: a completed
/// Hello is the best evidence of where the peer listens now, but another
/// source may know a path the Hello did not announce, such as a newly opened
/// relay. A stale or newly learned address can therefore reappear after the
/// Hello and remain until the next completed Hello replaces the list again.
///
/// `proving` is the address of the connection that completed Hello. It is
/// kept outside the announced cap and placed at the front: the peer may
/// announce eight higher-ranked addresses, and the path that actually worked
/// must not then be the one truncated away. This is what makes the
/// proving-address invariant absolute instead of merely likely.
fn replace_known_addrs(
    swarm: &mut Swarm<Behaviour>,
    state: &mut NodeState,
    peer: PeerId,
    announced: impl IntoIterator<Item = Multiaddr>,
    proving: Option<Multiaddr>,
) {
    let proving = proving
        .map(|address| without_peer(&address))
        .filter(|address| !address.is_empty() && is_dialable_addr(address));
    let mut seen = HashSet::new();
    let mut fresh: Vec<Multiaddr> = announced
        .into_iter()
        .map(|address| without_peer(&address))
        .filter(|address| !address.is_empty() && is_dialable_addr(address))
        .filter(|address| proving.as_ref() != Some(address))
        .filter(|address| seen.insert(address.clone()))
        .collect();
    if fresh.is_empty() && proving.is_none() {
        // Nothing usable was announced (a wildcard-only listener, say).
        // Dropping the old list on that evidence would leave no way back.
        return;
    }
    fresh.sort_by(|left, right| {
        address_score(right, false)
            .cmp(&address_score(left, false))
            .then_with(|| left.to_string().cmp(&right.to_string()))
    });
    fresh.truncate(MAX_ANNOUNCED_ADDRESSES);
    if let Some(proving) = proving {
        fresh.insert(0, proving);
    }
    let old = state
        .known_addrs
        .insert(peer, fresh.clone())
        .unwrap_or_default();
    for address in old {
        if !fresh.contains(&address) {
            swarm.behaviour_mut().kad.remove_address(&peer, &address);
        }
    }
    for address in &fresh {
        add_routing_address(swarm, &state.tag, peer, address);
    }
}

/// Record a peer's addresses and, if that peer is not already connected or
/// being dialed, open exactly one connection to it.
///
/// Every dial in the node goes through here. The previous code dialed each
/// address directly, which meant a peer's Identify event (received over an
/// existing connection) opened a new connection for every advertised address;
/// each of those produced another Identify event, doubling the connection
/// count each generation until the process ran out of file descriptors.
fn dial_peer_addrs(
    swarm: &mut Swarm<Behaviour>,
    state: &mut NodeState,
    peer: PeerId,
    addresses: impl IntoIterator<Item = Multiaddr>,
    wanted: bool,
) -> Option<String> {
    let now = Instant::now();
    if let Some(until) = state.rejected.get(&peer).copied() {
        if now < until && !state.wanted.contains(&peer) {
            trace_net!(
                "[{}] dial {peer} skipped: quarantined after a failed handshake",
                state.tag
            );
            return None;
        }
        if now >= until {
            state.rejected.remove(&peer);
        }
    }
    // The cooldown is keyed per address, so every peer ever dialled leaves an
    // entry behind. One is useless once the cooldown has passed, and the map is
    // swept here to keep a long-running daemon from remembering thousands of
    // addresses from peers that are long gone.
    if state.dialled_at.len() > 64 {
        state
            .dialled_at
            .retain(|_, at| now.duration_since(*at) < DISCOVERY_DIAL_COOLDOWN * 30);
    }
    // A discovery source that keeps reporting the same address must not become
    // a dial storm. The cooldown is per address, not per peer: one daemon
    // advertises a different port for each guild it hosts under the same peer
    // id, and each port has to be tried once when it is first discovered. It
    // applies to wanted peers too: their redials are already spaced by
    // `reconnect_at` (never under three seconds), so the cooldown never delays
    // a healthy reconnect.
    let own: HashSet<Multiaddr> = swarm
        .listeners()
        .chain(swarm.external_addresses())
        .cloned()
        .collect();
    let mut seen = HashSet::new();
    let mut candidates: Vec<Multiaddr> = Vec::new();
    let mut wrong_guild = 0usize;
    for address in addresses {
        let address = without_peer(&address);
        if address.is_empty()
            || !is_dialable_addr(&address)
            || own.contains(&address)
            || !seen.insert(address.clone())
        {
            continue;
        }
        if wrong_guild_at(state, peer, &address) {
            // This port answered for another guild the last time it was tried.
            // The daemon behind the peer id may still serve this guild on a
            // different port, so the rest of the addresses stay in play.
            trace_net!(
                "[{}] dial {peer} at {address} skipped: that address serves another guild",
                state.tag
            );
            wrong_guild += 1;
            continue;
        }
        if state
            .dialled_at
            .get(&(peer, address.clone()))
            .is_some_and(|at| now.duration_since(*at) < DISCOVERY_DIAL_COOLDOWN)
        {
            trace_net!(
                "[{}] dial {peer} at {address} skipped: inside the discovery cooldown",
                state.tag
            );
            continue;
        }
        candidates.push(address);
        // Rank before truncating, so a long tail of Docker or loopback
        // addresses cannot push the one routable address out of the list.
        if candidates.len() >= MAX_ANNOUNCED_ADDRESSES * 8 {
            break;
        }
    }
    candidates.sort_by(|left, right| {
        address_score(right, false)
            .cmp(&address_score(left, false))
            .then_with(|| left.to_string().cmp(&right.to_string()))
    });
    candidates.truncate(MAX_ANNOUNCED_ADDRESSES);
    // Only a peer that completed Hello belongs in the routing table. Adding
    // any address that merely got dialled is what let a refused peer be
    // redialled by every subsequent DHT query.
    if state.peer_guilds.contains_key(&peer) {
        for address in &candidates {
            add_routing_address(swarm, &state.tag, peer, address);
        }
    }
    if wanted {
        state.wanted.insert(peer);
    }
    if !candidates.is_empty() {
        // Keep every address we have ever been told about. A reconnect may
        // need a different one than the dial that originally worked.
        let known = state.known_addrs.entry(peer).or_default();
        for address in &candidates {
            if !known.contains(address) && known.len() < MAX_ANNOUNCED_ADDRESSES {
                known.push(address.clone());
            }
        }
    }
    if candidates.is_empty() {
        // Filtering can legitimately leave nothing (a QUIC-only peer, a
        // wildcard listen address, or only addresses that served another
        // guild). For a peer we are meant to stay connected to, that must not
        // be silent: schedule a retry and report it, or the peer would never be
        // revisited.
        if state.wanted.contains(&peer) && !swarm.is_connected(&peer) {
            state
                .reconnect_at
                .entry(peer)
                .or_insert(Instant::now() + RECONNECT_MIN);
            if wrong_guild > 0 {
                return Some(format!(
                    "no address for {peer} answers for this commune yet; waiting for discovery"
                ));
            }
            return Some(format!("no dialable address for {peer}"));
        }
        return None;
    }
    if swarm.is_connected(&peer) {
        return None;
    }
    trace_net!("[{}] dialling {peer} at {candidates:?}", state.tag);
    for address in &candidates {
        state
            .dialled_at
            .insert((peer, address.clone()), Instant::now());
    }
    match swarm.dial(
        DialOpts::peer_id(peer)
            .addresses(candidates)
            .condition(PeerCondition::DisconnectedAndNotDialing)
            .build(),
    ) {
        Ok(()) => None,
        Err(error) => {
            trace_net!("[{}] dial {peer} failed immediately: {error}", state.tag);
            if state.wanted.contains(&peer) {
                state
                    .reconnect_at
                    .entry(peer)
                    .or_insert(Instant::now() + RECONNECT_MIN);
            }
            Some(format!("failed to dial {peer}: {error}"))
        }
    }
}

/// Addresses worth dialing: IP (or DNS name) + TCP. Loopback is allowed here
/// so same-host use still works; `dial_peer_addrs` prefers routable addresses
/// whenever the peer offers any.
fn is_dialable_addr(address: &Multiaddr) -> bool {
    let mut saw_host = false;
    let mut saw_tcp = false;
    for protocol in address.iter() {
        match protocol {
            Protocol::Ip4(ip) => {
                if !is_usable_v4(ip) {
                    return false;
                }
                saw_host = true;
            }
            Protocol::Ip6(ip) => {
                if !is_usable_v6(ip) {
                    return false;
                }
                saw_host = true;
            }
            // Hostnames need the `dns` transport, which is enabled.
            Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) => saw_host = true,
            Protocol::Tcp(_) => saw_tcp = true,
            // Relay hops are still dialable: the TCP connection goes to the
            // relay, and the circuit is opened from there.
            Protocol::P2p(_) | Protocol::P2pCircuit => {}
            // This transport only speaks TCP; QUIC/WebSocket addresses would
            // just produce dial errors.
            _ => return false,
        }
    }
    saw_host && saw_tcp
}

fn is_usable_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    let unspecified = a == 0;
    let multicast = (224..=239).contains(&a);
    let reserved = a >= 240;
    let link_local = a == 169 && b == 254;
    !(unspecified || multicast || reserved || link_local)
}

fn is_usable_v6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_usable_v4(mapped);
    }
    let first = ip.segments()[0];
    let multicast = first & 0xff00 == 0xff00;
    let link_local = first & 0xffc0 == 0xfe80;
    !(ip.is_unspecified() || multicast || link_local)
}

fn add_routing_address(swarm: &mut Swarm<Behaviour>, tag: &str, peer: PeerId, address: &Multiaddr) {
    let address = without_peer(address);
    if address.is_empty() {
        return;
    }
    trace_net!("[{tag}] kad add {peer} {address}");
    swarm.behaviour_mut().kad.add_address(&peer, address);
}

/// The destination peer of an address.
///
/// A relayed address looks like `/ip4/…/tcp/…/p2p/<relay>/p2p-circuit/p2p/<dest>`:
/// the *last* `/p2p` is the peer to dial, the first is the relay. Taking the
/// first one identified the relay as the destination and made relayed peers
/// unreachable.
fn peer_of(address: &Multiaddr) -> Option<PeerId> {
    address
        .iter()
        .filter_map(|protocol| match protocol {
            Protocol::P2p(peer) => Some(peer),
            _ => None,
        })
        .last()
}

/// Strip only a trailing `/p2p/<peer>`.
///
/// The previous version truncated at the first `/p2p`, which turned a relayed
/// address into `/ip4/<relay>` and produced a dial to an address with no
/// port.
fn without_peer(address: &Multiaddr) -> Multiaddr {
    let protocols: Vec<Protocol<'_>> = address.iter().collect();
    if !matches!(protocols.last(), Some(Protocol::P2p(_))) {
        return address.clone();
    }
    let mut out = Multiaddr::empty();
    for protocol in &protocols[..protocols.len() - 1] {
        out.push(protocol.clone());
    }
    out
}

fn tcp_port(address: &Multiaddr) -> Option<u16> {
    address.iter().find_map(|protocol| match protocol {
        Protocol::Tcp(port) => Some(port),
        _ => None,
    })
}

async fn natpmp_worker(
    port: u16,
    command_tx: mpsc::UnboundedSender<NodeCommand>,
    event_tx: mpsc::UnboundedSender<NodeEvent>,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) {
    loop {
        match natpmp::new_tokio_natpmp().await {
            Ok(mut client) => {
                let mapping = tokio::time::timeout(Duration::from_secs(10), async {
                    client
                        .send_port_mapping_request(natpmp::Protocol::TCP, port, port, 3600)
                        .await?;
                    client.read_response_or_retry().await
                })
                .await;

                match mapping {
                    Ok(Ok(natpmp::Response::TCP(mapping))) => {
                        let public = tokio::time::timeout(Duration::from_secs(10), async {
                            client.send_public_address_request().await?;
                            client.read_response_or_retry().await
                        })
                        .await;
                        match public {
                            Ok(Ok(natpmp::Response::Gateway(gateway))) => {
                                let address: Multiaddr = format!(
                                    "/ip4/{}/tcp/{}",
                                    gateway.public_address(),
                                    mapping.public_port()
                                )
                                .parse()
                                .expect("valid NAT-PMP multiaddr");
                                let _ = command_tx.send(NodeCommand::AddExternalAddr(address));
                            }
                            Ok(Ok(_)) => {
                                let _ = event_tx.send(NodeEvent::Error(
                                    "NAT-PMP returned an unexpected public-address response"
                                        .to_string(),
                                ));
                            }
                            Ok(Err(error)) => {
                                let _ = event_tx.send(NodeEvent::Error(format!(
                                    "NAT-PMP public address: {error}"
                                )));
                            }
                            Err(_) => {
                                let _ = event_tx.send(NodeEvent::Error(
                                    "NAT-PMP public-address request timed out".to_string(),
                                ));
                            }
                        }
                    }
                    Ok(Ok(_)) => {
                        let _ = event_tx.send(NodeEvent::Error(
                            "NAT-PMP returned an unexpected mapping response".to_string(),
                        ));
                    }
                    Ok(Err(error)) => {
                        let _ = event_tx
                            .send(NodeEvent::Error(format!("NAT-PMP mapping failed: {error}")));
                    }
                    Err(_) => {
                        let _ = event_tx
                            .send(NodeEvent::Error("NAT-PMP mapping timed out".to_string()));
                    }
                }
            }
            Err(error) => {
                let _ = event_tx.send(NodeEvent::Error(format!("NAT-PMP unavailable: {error}")));
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1800)) => {}
            // The node is shutting down; stop holding its channels open.
            _ = shutdown.changed() => break,
        }
    }
}

fn hello_addrs(swarm: &Swarm<Behaviour>, listen_addrs: &HashSet<Multiaddr>) -> Vec<String> {
    let mut addresses = listen_addrs.clone();
    // External addresses (UPnP, NAT-PMP, manually added) are the ones a peer
    // on another machine can actually reach; listening addresses alone are
    // often a wildcard or a LAN address.
    addresses.extend(swarm.external_addresses().cloned());
    shareable_addrs(&addresses)
}

fn shareable_addrs(addresses: &HashSet<Multiaddr>) -> Vec<String> {
    ranked_addresses(
        addresses.iter().map(ToString::to_string).collect(),
        &HashSet::new(),
    )
}

/// Order addresses by how likely they are to be reachable from another host,
/// then cap them for an announcement or a Hello.
///
/// Observed external addresses come first because they survived a round trip
/// through the network. DNS names are stable across address changes, public IPs
/// beat private ones, and private LAN addresses beat Docker bridge addresses.
/// Loopback is advertised only when there is nothing else, and relay circuits
/// sit above Docker bridges because a relay hop works through NAT by design.
fn ranked_addresses(addresses: Vec<String>, external: &HashSet<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut scored: Vec<(i32, String)> = addresses
        .into_iter()
        .filter(|address| is_dialable(address))
        .filter(|address| seen.insert(address.clone()))
        .filter_map(|address| {
            let parsed = address.parse::<Multiaddr>().ok()?;
            let score = address_score(&parsed, external.contains(&address));
            Some((score, address))
        })
        .collect();
    if scored.iter().any(|(_, address)| !is_loopback_str(address)) {
        scored.retain(|(_, address)| !is_loopback_str(address));
    }
    scored.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    scored
        .into_iter()
        .take(MAX_ANNOUNCED_ADDRESSES)
        .map(|(_, address)| address)
        .collect()
}

/// Higher is more reachable. The absolute values only matter relative to each
/// other; tests pin the ordering.
fn address_score(address: &Multiaddr, external: bool) -> i32 {
    let mut score = if external { 1000 } else { 0 };
    let mut relayed = false;
    let mut host = 0;
    for protocol in address.iter() {
        match protocol {
            Protocol::P2pCircuit => relayed = true,
            Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) => host = 250,
            Protocol::Ip4(ip) => host = ipv4_score(ip),
            Protocol::Ip6(ip) => host = ipv6_score(ip),
            _ => {}
        }
    }
    score += if relayed { 100 } else { host };
    score
}

fn ipv4_score(ip: Ipv4Addr) -> i32 {
    if ip.is_loopback() {
        10
    } else if is_docker_bridge(ip) {
        -400
    } else if ip.is_private() {
        150
    } else {
        200
    }
}

fn ipv6_score(ip: Ipv6Addr) -> i32 {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return ipv4_score(mapped);
    }
    if ip.is_loopback() {
        10
    } else if ip.is_unique_local() {
        100
    } else if ip.is_unicast_link_local() {
        -400
    } else {
        120
    }
}

/// Docker's default bridge and the first compose network. They are real
/// addresses inside one host, but they are usually the least useful thing to
/// announce, so they rank below loopback's score plus every LAN address.
fn is_docker_bridge(ip: Ipv4Addr) -> bool {
    let [first, second, ..] = ip.octets();
    first == 172 && (second == 17 || second == 18)
}

fn is_loopback_str(address: &str) -> bool {
    address.starts_with("/ip4/127.") || address.starts_with("/ip6/::1/")
}

fn is_dialable(address: &str) -> bool {
    !address.starts_with("/ip4/0.0.0.0/") && !address.starts_with("/ip6/::/")
}

#[cfg(test)]
mod address_tests {
    use super::*;

    fn ranked(values: &[&str], external: &[&str]) -> Vec<String> {
        let external: HashSet<String> = external.iter().map(|value| value.to_string()).collect();
        ranked_addresses(
            values.iter().map(|value| value.to_string()).collect(),
            &external,
        )
    }

    #[test]
    fn external_and_lan_rank_above_dns_public_docker_and_loopback() {
        let order = ranked(
            &[
                "/ip4/172.17.0.1/tcp/41000",
                "/ip4/127.0.0.1/tcp/41000",
                "/ip4/192.0.2.5/tcp/41000",
                "/ip4/10.0.0.5/tcp/41000",
                "/dns4/peer.example/tcp/41000",
                "/ip4/203.0.113.9/tcp/41000",
            ],
            &["/ip4/192.0.2.5/tcp/41000"],
        );
        assert_eq!(order[0], "/ip4/192.0.2.5/tcp/41000");
        assert_eq!(order[1], "/dns4/peer.example/tcp/41000");
        assert_eq!(order[2], "/ip4/203.0.113.9/tcp/41000");
        assert_eq!(order[3], "/ip4/10.0.0.5/tcp/41000");
        assert_eq!(order[4], "/ip4/172.17.0.1/tcp/41000");
        assert!(
            !order.iter().any(|address| address.starts_with("/ip4/127.")),
            "loopback must be dropped when a routable address exists: {order:?}"
        );
    }

    #[test]
    fn relay_circuits_rank_between_a_lan_address_and_docker() {
        let relay = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(203, 0, 113, 9)))
            .with(Protocol::Tcp(41000))
            .with(Protocol::P2pCircuit)
            .to_string();
        let order = ranked(
            &[
                "/ip4/172.17.0.1/tcp/41000",
                "/ip4/10.0.0.5/tcp/41000",
                &relay,
            ],
            &[],
        );
        assert_eq!(order[0], "/ip4/10.0.0.5/tcp/41000");
        assert_eq!(order[1], relay);
        assert_eq!(order[2], "/ip4/172.17.0.1/tcp/41000");
    }

    #[test]
    fn dial_order_is_deterministic_and_deduped() {
        let values = vec![
            "/ip4/172.17.0.1/tcp/41000".to_string(),
            "/ip4/192.0.2.5/tcp/41000".to_string(),
            "/ip4/192.0.2.5/tcp/41000".to_string(),
            "/ip4/127.0.0.1/tcp/41000".to_string(),
        ];
        let parsed = parse_dialable(&values);
        assert_eq!(parsed[0].to_string(), "/ip4/192.0.2.5/tcp/41000");
        assert_eq!(
            parsed
                .iter()
                .filter(|address| address.to_string() == "/ip4/192.0.2.5/tcp/41000")
                .count(),
            1
        );
        assert_eq!(
            parsed.last().unwrap().to_string(),
            "/ip4/172.17.0.1/tcp/41000"
        );
        assert_eq!(parsed, parse_dialable(&values));
    }

    fn state_with_connection(peer: PeerId, dialer: bool) -> NodeState {
        let mut state = NodeState::default();
        state
            .connections
            .entry(peer)
            .or_default()
            .insert(ConnectionId::new_unchecked(1), dialer);
        state
    }

    #[test]
    fn hello_is_safe_only_while_every_connection_is_outbound() {
        let peer = PeerId::random();
        let mut state = state_with_connection(peer, true);
        assert!(hello_connection_is_safe(&state, &peer));

        // Adding an inbound connection makes the choice of connection
        // ambiguous: request-response may route the Hello over it, handing
        // this node's guild and listen addresses to a peer that never
        // introduced itself.
        state
            .connections
            .get_mut(&peer)
            .unwrap()
            .insert(ConnectionId::new_unchecked(2), false);
        assert!(!hello_connection_is_safe(&state, &peer));

        // A peer reached only over a connection it dialled is never sent an
        // unsolicited Hello; its own Hello is the proof.
        let inbound_only = PeerId::random();
        let state = state_with_connection(inbound_only, false);
        assert!(!hello_connection_is_safe(&state, &inbound_only));

        assert!(!hello_connection_is_safe(&state, &PeerId::random()));
    }

    #[test]
    fn dialability_filters_wildcard_multicast_link_local_and_non_tcp() {
        let cases = [
            ("/ip4/192.0.2.5/tcp/41000", true),
            ("/ip4/127.0.0.1/tcp/41000", true),
            ("/dns4/peer.example/tcp/41000", true),
            ("/ip6/::1/tcp/41000", true),
            ("/ip4/0.0.0.0/tcp/41000", false),
            ("/ip6/::/tcp/41000", false),
            ("/ip4/239.1.1.1/tcp/41000", false),
            ("/ip4/240.0.0.1/tcp/41000", false),
            ("/ip4/169.254.1.1/tcp/41000", false),
            ("/ip6/fe80::1/tcp/41000", false),
            ("/ip6/ff02::1/tcp/41000", false),
            ("/ip4/192.0.2.5/udp/41000/quic-v1", false),
            ("/ip4/192.0.2.5", false),
        ];
        for (address, expected) in cases {
            let parsed: Multiaddr = address.parse().unwrap();
            assert_eq!(
                is_dialable_addr(&parsed),
                expected,
                "{address} should be dialable={expected}"
            );
        }

        let relayed: Multiaddr = "/ip4/203.0.113.9/tcp/41000/p2p/12D3KooWDtbHHqPnjCDwAdqRaSees1KbQtC7jAmUHFyuCxihP8Lo/p2p-circuit"
            .parse()
            .unwrap();
        assert!(is_dialable_addr(&relayed));
    }

    #[test]
    fn usable_v4_and_v6_reject_only_unroutable_classes() {
        assert!(is_usable_v4(Ipv4Addr::new(10, 0, 0, 5)));
        assert!(is_usable_v4(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!is_usable_v4(Ipv4Addr::new(0, 0, 0, 0)));
        assert!(!is_usable_v4(Ipv4Addr::new(169, 254, 1, 1)));
        assert!(!is_usable_v4(Ipv4Addr::new(224, 0, 0, 1)));
        assert!(!is_usable_v4(Ipv4Addr::new(250, 0, 0, 1)));

        assert!(is_usable_v6("2001:db8::1".parse().unwrap()));
        assert!(is_usable_v6("::1".parse().unwrap()));
        assert!(!is_usable_v6("::".parse().unwrap()));
        assert!(!is_usable_v6("fe80::1".parse().unwrap()));
        assert!(!is_usable_v6("ff02::1".parse().unwrap()));
        assert!(!is_usable_v6("::ffff:169.254.1.1".parse().unwrap()));
        assert!(is_usable_v6("::ffff:192.0.2.5".parse().unwrap()));
    }

    #[test]
    fn addresses_keep_only_the_trailing_peer_and_expose_the_tcp_port() {
        let plain: Multiaddr = "/ip4/192.0.2.5/tcp/41000".parse().unwrap();
        assert_eq!(tcp_port(&plain), Some(41000));
        assert_eq!(without_peer(&plain), plain);
        assert_eq!(peer_of(&plain), None);

        let direct = format!("{}/p2p/{}", plain, PeerId::random())
            .parse::<Multiaddr>()
            .unwrap();
        assert_eq!(without_peer(&direct), plain);
        assert!(peer_of(&direct).is_some());

        let relay = PeerId::random();
        let destination = PeerId::random();
        let relayed: Multiaddr =
            format!("/ip4/203.0.113.9/tcp/41000/p2p/{relay}/p2p-circuit/p2p/{destination}")
                .parse()
                .unwrap();
        assert_eq!(peer_of(&relayed), Some(destination));
        assert_eq!(
            without_peer(&relayed).to_string(),
            format!("/ip4/203.0.113.9/tcp/41000/p2p/{relay}/p2p-circuit")
        );

        assert_eq!(tcp_port(&"/ip6/::1".parse().unwrap()), None);
    }

    #[test]
    fn ipv6_scores_rank_global_above_ula_loopback_and_link_local() {
        assert!(
            ipv6_score("2001:db8::1".parse().unwrap()) > ipv6_score("fd00::1".parse().unwrap())
        );
        assert!(ipv6_score("fd00::1".parse().unwrap()) > ipv6_score("::1".parse().unwrap()));
        assert!(ipv6_score("::1".parse().unwrap()) > ipv6_score("fe80::1".parse().unwrap()));
        assert_eq!(
            ipv6_score("::ffff:192.0.2.5".parse().unwrap()),
            ipv4_score(Ipv4Addr::new(192, 0, 2, 5))
        );
    }

    #[test]
    fn shareable_addresses_drop_loopback_only_when_another_host_exists() {
        let mut addresses = HashSet::new();
        addresses.insert("/ip4/127.0.0.1/tcp/41000".parse().unwrap());
        assert_eq!(
            shareable_addrs(&addresses),
            vec!["/ip4/127.0.0.1/tcp/41000".to_string()]
        );

        addresses.insert("/ip4/192.0.2.5/tcp/41000".parse().unwrap());
        addresses.insert("/ip4/0.0.0.0/tcp/41000".parse().unwrap());
        assert_eq!(
            shareable_addrs(&addresses),
            vec!["/ip4/192.0.2.5/tcp/41000".to_string()]
        );
    }

    #[test]
    fn dial_parsing_ignores_garbage_wildcards_and_duplicates() {
        let values = vec![
            "not a multiaddr".to_string(),
            "/ip4/0.0.0.0/tcp/41000".to_string(),
            "/ip4/192.0.2.5/tcp/41000".to_string(),
            "/ip4/192.0.2.5/tcp/41000".to_string(),
        ];
        let parsed = parse_dialable(&values);
        assert_eq!(
            parsed.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["/ip4/192.0.2.5/tcp/41000".to_string()]
        );
        assert_eq!(parse_addrs(&values).len(), 3);
    }

    #[test]
    fn announced_addresses_are_not_capped_before_filtering() {
        // Eight parseable but undialable addresses used to fill the cap
        // before `replace_known_addrs` could drop them, losing the listener
        // that followed.
        let mut values: Vec<String> = (0..MAX_ANNOUNCED_ADDRESSES)
            .map(|port| format!("/ip4/0.0.0.0/tcp/{}", 4000 + port))
            .collect();
        let listener = "/ip4/127.0.0.1/tcp/5000".to_string();
        values.push(listener.clone());
        let parsed = parse_addrs(&values);
        assert_eq!(parsed.len(), values.len());
        assert_eq!(parsed.last().unwrap().to_string(), listener);
    }

    #[test]
    fn handshake_rejection_keeps_wanted_peer_and_schedules_its_redial() {
        let peer = PeerId::random();
        let mut state = NodeState::default();
        state.wanted.insert(peer);
        let address: Multiaddr = "/ip4/127.0.0.1/tcp/5000".parse().unwrap();
        state.known_addrs.insert(peer, vec![address.clone()]);

        let before = Instant::now();
        reject_peer(&mut state, peer);

        // One timed-out or unsupported address says nothing about the peer's
        // other ports: the daemon serves every guild from the same peer id.
        assert!(state.wanted.contains(&peer));
        assert_eq!(state.known_addrs.get(&peer), Some(&vec![address]));

        // The peer is left alone until the cooldown expires, but then it is
        // dialled from the addresses already known instead of waiting for
        // mDNS to find it again.
        let until = state
            .rejected
            .get(&peer)
            .copied()
            .expect("cooldown recorded");
        assert!(until >= before + HELLO_FAILURE_COOLDOWN);
        assert_eq!(state.reconnect_at.get(&peer).copied(), Some(until));
    }

    #[test]
    fn mutual_dial_without_proof_is_resolved_by_the_larger_peer_id() {
        let first = PeerId::random();
        let second = PeerId::random();
        let (larger, smaller) = if first > second {
            (first, second)
        } else {
            (second, first)
        };
        let inbound = ConnectionId::new_unchecked(1);
        let outbound = ConnectionId::new_unchecked(2);

        // Each side handled its inbound connection first and did nothing, as
        // it must: only a dialer introduces itself. Then the outbound
        // connection completed before either side's Hello was accepted.
        let mut larger_state = NodeState::default();
        larger_state
            .connections
            .entry(smaller)
            .or_default()
            .insert(inbound, false);
        assert_eq!(
            hello_action(&larger_state, larger, smaller, false, 1),
            HelloAction::Wait
        );
        larger_state
            .connections
            .get_mut(&smaller)
            .unwrap()
            .insert(outbound, true);

        let mut smaller_state = NodeState::default();
        smaller_state
            .connections
            .entry(larger)
            .or_default()
            .insert(inbound, false);
        smaller_state
            .connections
            .get_mut(&larger)
            .unwrap()
            .insert(outbound, true);

        // Before the deterministic tie-break both sides returned Wait here,
        // leaving the pair connected in both directions and never proven.
        // Now exactly one side acts: the larger peer id closes its inbound
        // connections, and the smaller side waits to answer that Hello.
        assert_eq!(
            hello_action(&larger_state, larger, smaller, true, 2),
            HelloAction::CloseInboundThenSend
        );
        assert_eq!(
            hello_action(&smaller_state, smaller, larger, true, 2),
            HelloAction::Wait
        );

        // Once the larger side's inbound close is processed, only outbound
        // connections remain, so request-response cannot pick an inbound
        // connection and the queued retry can send safely.
        larger_state
            .connections
            .get_mut(&smaller)
            .unwrap()
            .remove(inbound);
        assert!(hello_connection_is_safe(&larger_state, &smaller));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn proving_address_survives_the_announced_cap_in_known_addrs_and_kademlia() {
        let (mut swarm, _) = build_swarm([7u8; 32], false).expect("building the swarm");
        let mut state = NodeState::default();
        let peer = PeerId::random();

        // Every announced address outranks loopback, so without the exemption
        // the proving address would be the ninth entry and be truncated.
        let announced: Vec<Multiaddr> = (1..=MAX_ANNOUNCED_ADDRESSES)
            .map(|index| format!("/ip4/203.0.113.{index}/tcp/4000").parse().unwrap())
            .collect();
        let proving: Multiaddr = "/ip4/127.0.0.1/tcp/5000".parse().unwrap();

        replace_known_addrs(
            &mut swarm,
            &mut state,
            peer,
            announced,
            Some(proving.clone()),
        );

        let known = state.known_addrs.get(&peer).expect("addresses recorded");
        assert_eq!(known.first(), Some(&proving));
        assert_eq!(
            known.iter().filter(|address| **address == proving).count(),
            1,
            "the proving address is kept once, outside the cap: {known:?}"
        );

        // Kademlia stores fully-qualified addresses, so strip `/p2p/<peer>`
        // before comparing.
        let routing_addrs: Vec<Multiaddr> = swarm
            .behaviour_mut()
            .kad
            .kbucket(peer)
            .map(|bucket| {
                bucket
                    .iter()
                    .flat_map(|entry| entry.node.value.iter().cloned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            routing_addrs
                .iter()
                .any(|address| without_peer(address) == proving),
            "proving address missing from the routing table: {routing_addrs:?}"
        );
    }
    fn signed_message() -> SignedEvent {
        use ovvc_core::model::{ChannelId, EVENT_VERSION, EventBody, EventKind};
        let identity = Identity::from_seed([7u8; 32]);
        identity
            .sign_body(EventBody {
                version: EVENT_VERSION,
                guild_id: GuildId([1u8; 32]),
                channel_id: Some(ChannelId([2u8; 16])),
                author: identity.author_id(),
                seq: 1,
                lamport: 1,
                timestamp: 0,
                prev: None,
                kind: EventKind::Message {
                    body: "hello".to_string(),
                },
            })
            .unwrap()
    }

    #[test]
    fn hello_action_only_introduces_over_outbound_connections() {
        let peer = PeerId::random();
        let local = PeerId::random();
        let mut state = NodeState::default();
        assert_eq!(
            hello_action(&state, local, peer, true, 1),
            HelloAction::Wait,
            "no tracked connection waits"
        );

        state
            .connections
            .entry(peer)
            .or_default()
            .insert(ConnectionId::new_unchecked(1), true);
        assert_eq!(
            hello_action(&state, local, peer, true, 1),
            HelloAction::Send
        );
        assert_eq!(
            hello_action(&state, local, peer, false, 1),
            HelloAction::Wait,
            "the non-dialer answers the Hello"
        );

        state
            .connections
            .entry(peer)
            .or_default()
            .insert(ConnectionId::new_unchecked(2), false);
        // Ambiguous pair: only the larger peer id closes its inbound leg.
        let (bigger, smaller) = if local > peer {
            (local, peer)
        } else {
            (peer, local)
        };
        let mut larger_side = NodeState::default();
        larger_side
            .connections
            .entry(smaller)
            .or_default()
            .insert(ConnectionId::new_unchecked(1), true);
        larger_side
            .connections
            .entry(smaller)
            .or_default()
            .insert(ConnectionId::new_unchecked(2), false);
        assert_eq!(
            hello_action(&larger_side, bigger, smaller, true, 2),
            HelloAction::CloseInboundThenSend
        );
        assert_eq!(
            hello_action(&larger_side, smaller, bigger, true, 2),
            HelloAction::Wait
        );
        assert_eq!(
            hello_action(&larger_side, bigger, smaller, true, 1),
            HelloAction::Wait,
            "a single connection cannot be an ambiguous pair"
        );

        larger_side.peer_guilds.insert(smaller, GuildId([1u8; 32]));
        assert_eq!(
            hello_action(&larger_side, bigger, smaller, true, 2),
            HelloAction::Wait,
            "a proven peer needs no Hello"
        );
    }

    #[test]
    fn delivery_retries_back_off_and_stop_at_the_cap() {
        let mut state = NodeState::default();
        let peer = PeerId::random();
        let event = signed_message();

        schedule_delivery_retry(&mut state, peer, event.clone(), DELIVERY_MAX_ATTEMPTS);
        assert!(state.delivery_retries.is_empty(), "the cap is final");

        schedule_delivery_retry(&mut state, peer, event.clone(), 0);
        schedule_delivery_retry(&mut state, peer, event.clone(), 2);
        assert_eq!(state.delivery_retries.len(), 2);
        assert_eq!(state.delivery_retries[0].attempts, 1);
        assert_eq!(state.delivery_retries[1].attempts, 3);
        assert!(state.delivery_retries[0].at <= state.delivery_retries[1].at);
    }

    #[test]
    fn mismatch_retry_needs_a_known_address_and_quarantine_is_per_address() {
        let mut state = NodeState::default();
        let peer = PeerId::random();
        schedule_mismatch_retry(&mut state, peer);
        assert!(!state.mismatch_retry.contains_key(&peer));

        let address: Multiaddr = "/ip4/192.0.2.5/tcp/41000".parse().unwrap();
        state.known_addrs.insert(peer, vec![address.clone()]);
        schedule_mismatch_retry(&mut state, peer);
        assert!(state.mismatch_retry.contains_key(&peer));

        let direct = format!("{address}/p2p/{peer}")
            .parse::<Multiaddr>()
            .unwrap();
        mark_wrong_guild(&mut state, peer, [direct, address.clone()]);
        assert!(wrong_guild_at(&mut state, peer, &address));
        assert!(!wrong_guild_at(
            &mut state,
            peer,
            &"/ip4/192.0.2.6/tcp/41000".parse().unwrap()
        ));
        assert!(!wrong_guild_at(&mut state, PeerId::random(), &address));

        // A past expiry is pruned and drops the empty peer entry.
        state
            .wrong_guild_addrs
            .get_mut(&peer)
            .unwrap()
            .iter_mut()
            .for_each(|(_, until)| *until = Instant::now() - Duration::from_secs(1));
        assert!(!wrong_guild_at(&mut state, peer, &address));
        assert!(!state.wrong_guild_addrs.contains_key(&peer));
    }

    #[test]
    fn proven_peers_survive_rejection() {
        let mut state = NodeState::default();
        let proven = PeerId::random();
        let unknown = PeerId::random();
        state.peer_guilds.insert(proven, GuildId([1u8; 32]));

        reject_peer_unless_proven(&mut state, proven);
        assert!(state.peer_guilds.contains_key(&proven));
        assert!(!state.rejected.contains_key(&proven));

        reject_peer_unless_proven(&mut state, unknown);
        assert!(!state.peer_guilds.contains_key(&unknown));
        assert!(state.rejected.contains_key(&unknown));
    }

    #[test]
    fn record_keys_and_address_class_helpers_are_stable() {
        let author = AuthorId([9u8; 32]);
        assert_eq!(
            record_key_for_author(author).to_vec(),
            author.as_bytes().to_vec()
        );

        assert!(is_docker_bridge(Ipv4Addr::new(172, 17, 0, 1)));
        assert!(is_docker_bridge(Ipv4Addr::new(172, 18, 0, 1)));
        assert!(!is_docker_bridge(Ipv4Addr::new(172, 19, 0, 1)));
        assert!(is_loopback_str("/ip4/127.0.0.1/tcp/1"));
        assert!(is_loopback_str("/ip6/::1/tcp/1"));
        assert!(!is_loopback_str("/ip4/192.0.2.5/tcp/1"));
        assert!(!is_dialable("/ip4/0.0.0.0/tcp/1"));
        assert!(is_dialable("/ip4/192.0.2.5/tcp/1"));

        assert!(benign_dial_error(&DialError::NoAddresses));
        assert!(benign_dial_error(&DialError::Aborted));
        assert!(!benign_dial_error(&DialError::LocalPeerId {
            address: "/ip4/127.0.0.1/tcp/1".parse().unwrap(),
        }));
    }
}

#[cfg(test)]
mod control_routing_tests {
    use super::*;
    use ovvc_control::ControlKind;

    fn signed(identity: &Identity, guild_id: GuildId, kind: ControlKind) -> ControlFrame {
        ControlFrame::sign(
            identity,
            guild_id,
            ovvc_control::SessionId::for_guild(guild_id),
            0,
            kind,
            1,
        )
        .expect("sign the control frame")
    }

    fn guild_with_peers() -> (
        GuildId,
        Identity,
        PeerId,
        PeerId,
        PeerId,
        AuthorId,
        AuthorId,
    ) {
        let guild_id = GuildId([7; 32]);
        let identity = Identity::generate();
        let local = peer_id_for_author(identity.author_id()).expect("local peer id");
        let b_identity = Identity::generate();
        let c_identity = Identity::generate();
        let b_author = b_identity.author_id();
        let c_author = c_identity.author_id();
        let b = peer_id_for_author(b_author).expect("peer b");
        let c = peer_id_for_author(c_author).expect("peer c");
        (guild_id, identity, local, b, c, b_author, c_author)
    }

    fn state_with(b: PeerId, c: PeerId, guild_id: GuildId) -> NodeState {
        let mut state = NodeState::default();
        state.peer_guilds.insert(b, guild_id);
        state.peer_guilds.insert(c, guild_id);
        state
    }

    #[test]
    fn rtc_signalling_is_targeted_and_never_reforwarded() {
        let (guild_id, identity, local, b, c, b_author, _) = guild_with_peers();
        let state = state_with(b, c, guild_id);
        let offer = signed(
            &identity,
            guild_id,
            ControlKind::RtcOffer {
                channel_id: ChannelId([1; 16]),
                to: b_author,
                sdp: "v=0\r\n".to_owned(),
            },
        );

        // Locally originated: only the named author gets it.
        assert_eq!(
            control_route(&state, guild_id, &offer, None),
            ControlRoute::Targeted(b)
        );
        // Received from a relay: deliver locally, fan out nowhere.
        assert_eq!(
            control_route(&state, guild_id, &offer, Some(local)),
            ControlRoute::Broadcast(Vec::new())
        );
        // A target known only in another guild gets nothing.
        let other = GuildId([9; 32]);
        assert_eq!(
            control_route(&state, other, &offer, None),
            ControlRoute::Broadcast(Vec::new())
        );
    }

    #[test]
    fn non_rtc_control_still_broadcasts_to_the_guild() {
        let (guild_id, identity, local, b, c, _, _) = guild_with_peers();
        let state = state_with(b, c, guild_id);
        let typing = signed(
            &identity,
            guild_id,
            ControlKind::Typing {
                channel_id: ChannelId([1; 16]),
                until_ms: 10,
            },
        );

        let mut expected = vec![b, c];
        expected.sort();
        assert_eq!(
            control_route(&state, guild_id, &typing, None),
            ControlRoute::Broadcast(expected.clone())
        );
        // The source never receives its own broadcast back.
        assert_eq!(
            control_route(&state, guild_id, &typing, Some(b)),
            ControlRoute::Broadcast(vec![c])
        );
        assert_eq!(
            control_route(&state, guild_id, &typing, Some(local)),
            ControlRoute::Broadcast(expected)
        );
    }
}

#[cfg(test)]
mod media_tests {
    use futures::io::Cursor;
    use libp2p::request_response::Codec as _;
    use ovvc_media::MediaKind;

    use super::*;

    fn channel() -> ChannelId {
        ChannelId([3; 16])
    }

    fn packet(channel_id: ChannelId, seq: u16) -> MediaPacket {
        MediaPacket::new(
            MediaKind::Audio,
            CallId::for_channel(channel_id),
            AuthorId([4; 32]),
            seq,
            1_700_000_000_000,
            vec![9, 8, 7, 6],
        )
        .expect("packet")
    }

    /// Media rides the same cbor codec as the rest of the protocol, so the
    /// packet must survive the exact encode/decode path `rr` uses.
    #[tokio::test]
    async fn media_request_round_trips_through_the_cbor_codec() {
        let channel_id = channel();
        let packet = packet(channel_id, 513);
        let request = WireRequest::Media {
            channel_id,
            packet: packet.clone(),
        };

        let mut codec = cbor::codec::Codec::<WireRequest, WireResponse>::default();
        let protocol = StreamProtocol::new(PROTOCOL);
        let mut out = Cursor::new(Vec::new());
        codec
            .write_request(&protocol, &mut out, request)
            .await
            .expect("encode media request");
        let encoded = out.into_inner();
        assert!(!encoded.is_empty());

        let mut input = Cursor::new(encoded);
        let decoded = codec
            .read_request(&protocol, &mut input)
            .await
            .expect("decode media request");
        match decoded {
            WireRequest::Media {
                channel_id: decoded_channel,
                packet: decoded_packet,
            } => {
                assert_eq!(decoded_channel, channel_id);
                assert_eq!(decoded_packet, packet);
            }
            other => panic!("expected a media request, got {other:?}"),
        }
    }

    #[test]
    fn a_wrong_call_id_fails_the_inbound_check() {
        let other_channel = ChannelId([7; 16]);
        // A packet whose call id belongs to another channel is refused for
        // this one, which is what `handle_request` does before emitting it.
        let foreign = packet(other_channel, 1);
        assert!(!media_packet_is_for_call(channel(), &foreign));
        // The genuine pairing passes the same check.
        let genuine = packet(channel(), 1);
        assert!(media_packet_is_for_call(channel(), &genuine));
    }

    #[test]
    fn media_targets_are_deduped_and_capped() {
        let peers: Vec<PeerId> = (0..MAX_MEDIA_TARGETS + 8)
            .map(|_| PeerId::random())
            .collect();
        let mut targets = peers.clone();
        targets.extend(peers.iter().copied());
        let unique = media_targets(targets);
        assert_eq!(unique.len(), MAX_MEDIA_TARGETS);
        let mut seen = HashSet::new();
        for peer in &unique {
            assert!(seen.insert(*peer), "a duplicate target survived");
        }
        assert_eq!(unique, peers[..MAX_MEDIA_TARGETS].to_vec());
    }
}
