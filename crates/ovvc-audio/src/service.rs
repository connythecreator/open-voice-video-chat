//! A threaded call service around [`AudioEngine`].
//!
//! The engine drives a device backend, and the PipeWire backend is `!Send`,
//! so the service owns both inside one dedicated OS thread. Callers talk to it
//! through an unbounded command channel and receive everything else on a
//! `tokio::sync::broadcast` channel, which means the thread needs no tokio
//! runtime of its own and the frontends can subscribe from async code.
//!
//! The loop runs at half a frame period. Each pass drains pending commands,
//! captures and publishes one packet, mixes and plays whatever the peer
//! jitter buffers have ready, and reports speaking changes. Once a second it
//! publishes [`AudioStats`]. A [`AudioCommand::Stop`] closes the device, sends
//! [`AudioEvent::Stopped`] and exits; an error sends [`AudioEvent::Error`] and
//! exits after closing the device.

use std::panic::AssertUnwindSafe;
use std::time::{Duration, Instant};

use ovvc_core::{AuthorId, ChannelId, GuildId};
use ovvc_media::call::CallId;
use ovvc_media::packet::MediaPacket;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::{TryRecvError, TrySendError};

use crate::backend::{BackendKind, EngineConfig, build_backend};
use crate::engine::AudioEngine;
use crate::error::{AudioError, Result};

/// How often the service publishes [`AudioStats`].
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// Packets a call will queue before further remote media is dropped.
///
/// Voice loss is already handled by the jitter buffers, so dropping an
/// arriving packet under a flood is preferable to unbounded memory growth.
const MEDIA_QUEUE_CAPACITY: usize = 64;

/// How many queued media packets one service tick may hand to the engine.
///
/// The control queue is drained first and in full, so `Stop` and `SetState`
/// cannot be starved by a media flood.
const MAX_MEDIA_PER_TICK: usize = 8;

/// Everything one call needs to start its service thread.
#[derive(Clone)]
pub struct AudioCallConfig {
    /// Guild the call belongs to.
    pub guild_id: GuildId,
    /// Channel the call belongs to.
    pub channel_id: ChannelId,
    /// The negotiated call identity.
    pub call_id: CallId,
    /// The local author stamped on outgoing packets.
    pub local_author: AuthorId,
    /// Codec, jitter and pipeline settings.
    pub engine: EngineConfig,
    /// Which device backend to open.
    pub backend: BackendKind,
    /// Initial mute gate, applied before the first capture frame so a member
    /// joining muted cannot transmit for a tick while `SetState` is in flight.
    pub muted: bool,
    /// Initial deafen gate, applied before the first playback frame.
    pub deafened: bool,
    /// Initial push-to-talk gate. The key starts unheld.
    pub push_to_talk: bool,
    /// Where the service publishes media, speaking, stats and lifecycle
    /// events.
    pub events: broadcast::Sender<AudioEventEnvelope>,
    /// Identifies this service instance, so a late event from a stopped
    /// service cannot be mistaken for the one that replaced it.
    pub generation: u64,
}

impl std::fmt::Debug for AudioCallConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioCallConfig")
            .field("guild_id", &self.guild_id)
            .field("channel_id", &self.channel_id)
            .field("call_id", &self.call_id.to_hex())
            .field("local_author", &self.local_author)
            .field("engine", &self.engine)
            .field("backend", &self.backend)
            .field("muted", &self.muted)
            .field("deafened", &self.deafened)
            .field("push_to_talk", &self.push_to_talk)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// A service event tagged with the call it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioEventEnvelope {
    /// Guild the event belongs to.
    pub guild_id: GuildId,
    /// Channel the event belongs to.
    pub channel_id: ChannelId,
    /// The service instance that produced the event.
    pub generation: u64,
    /// The event itself.
    pub event: AudioEvent,
}

/// Commands accepted by a running service.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioCommand {
    /// A packet received from a remote peer.
    Media(MediaPacket),
    /// Replace the mute, deafen and push-to-talk gates.
    SetState {
        /// Stop sending microphone audio.
        muted: bool,
        /// Stop playing remote audio and discard buffered audio.
        deafened: bool,
        /// Only send while the push-to-talk key is held.
        push_to_talk: bool,
    },
    /// Tell the engine whether the push-to-talk key is currently held.
    ///
    /// This is local device state and is never published as a roster flag.
    PushToTalkHeld(bool),
    /// Write one already-mixed PCM frame to the playback device.
    ///
    /// A star hub owns its local mix in [`HubMixer`](crate::HubMixer) and
    /// sends the result here, because the device backend lives in this thread.
    /// The mesh engine never receives this command, so the two playback paths
    /// cannot write at the same time. Deafen is still applied by the engine.
    Playback(Vec<i16>),
    /// Close the device and end the service thread.
    Stop,
}

/// Events published by a running service.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioEvent {
    /// An encoded local packet for the transport to broadcast.
    Media(MediaPacket),
    /// The speaking detector changed state.
    Speaking(bool),
    /// Counters sampled once a second.
    Stats(AudioStats),
    /// The service stopped because of this error.
    Error(String),
    /// The service closed the device and exited.
    Stopped,
}

/// A one-second snapshot of the call.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioStats {
    /// The backend that was actually opened: `"pipewire"`, `"null"` or
    /// another implementation's name.
    pub backend: String,
    /// Frames read from the device since the service started.
    pub captured: u64,
    /// Packets encoded since the service started.
    pub packets: u64,
    /// Frames written to the device since the service started.
    pub played: u64,
    /// Frames the peer jitter buffers concealed.
    pub concealed: u64,
    /// Packets the peer jitter buffers considered lost.
    pub lost: u64,
    /// Peer payloads the decoders rejected.
    pub decode_errors: u64,
    /// Remote peer streams currently mixed.
    pub peers: usize,
    /// Backend underruns or overruns.
    pub xruns: u64,
    /// Estimated output latency in milliseconds.
    pub latency_ms: u64,
    /// Largest absolute mixed sample since the previous stats event.
    pub peak: i16,
    /// Root-mean-square level of the mixed samples since the previous stats
    /// event.
    pub rms: f32,
}

/// A cloneable command handle to a running service.
#[derive(Debug, Clone)]
pub struct AudioHandle {
    control: mpsc::UnboundedSender<AudioCommand>,
    media: mpsc::Sender<MediaPacket>,
}

impl AudioHandle {
    /// Send a command to the service thread.
    ///
    /// Media is offered to a bounded queue and dropped when the queue is full,
    /// so one flooding call member cannot grow memory without bound. Lifecycle
    /// and state commands go on an unbounded control queue and cannot be lost
    /// behind that media. A closed control queue means the thread has exited,
    /// which lets a caller detect a stopped call rather than silently dropping
    /// commands.
    pub fn send(&self, command: AudioCommand) -> Result<()> {
        match command {
            AudioCommand::Media(packet) => match self.media.try_send(packet) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(_)) => Ok(()),
                Err(TrySendError::Closed(_)) => Err(stopped()),
            },
            command => self.control.send(command).map_err(|_| stopped()),
        }
    }
}

fn stopped() -> AudioError {
    AudioError::Backend("the audio service is no longer running".to_string())
}

/// Start a call service on its own OS thread.
///
/// The backend is built and started inside the thread, which is required for
/// the `!Send` PipeWire backend. The returned join handle finishes after the
/// service sends [`AudioEvent::Stopped`] or [`AudioEvent::Error`].
pub fn spawn(config: AudioCallConfig) -> Result<(AudioHandle, std::thread::JoinHandle<()>)> {
    // Validate before spawning so an obviously bad call never starts a thread.
    config.engine.validate()?;

    let (control, control_rx) = mpsc::unbounded_channel();
    let (media, media_rx) = mpsc::channel(MEDIA_QUEUE_CAPACITY);
    let events = config.events.clone();
    let guild_id = config.guild_id;
    let channel_id = config.channel_id;
    let generation = config.generation;
    let join = std::thread::Builder::new()
        .name("ovvc-audio-service".to_string())
        .spawn(move || {
            // A panic must still retire the service: otherwise the daemon
            // keeps a handle to a dead thread and never learns why the call
            // went silent.
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                run_service(config, control_rx, media_rx)
            }));
            if outcome.is_err() {
                let _ = events.send(AudioEventEnvelope {
                    guild_id,
                    channel_id,
                    generation,
                    event: AudioEvent::Error("the audio service panicked".to_string()),
                });
            }
        })
        .map_err(AudioError::Io)?;
    Ok((AudioHandle { control, media }, join))
}

fn run_service(
    config: AudioCallConfig,
    commands: mpsc::UnboundedReceiver<AudioCommand>,
    media: mpsc::Receiver<MediaPacket>,
) {
    let guild_id = config.guild_id;
    let channel_id = config.channel_id;
    let generation = config.generation;
    let events = config.events.clone();

    if let Err(err) = service_loop(config, commands, media) {
        let _ = events.send(AudioEventEnvelope {
            guild_id,
            channel_id,
            generation,
            event: AudioEvent::Error(err.to_string()),
        });
    }
}

fn service_loop(
    config: AudioCallConfig,
    mut commands: mpsc::UnboundedReceiver<AudioCommand>,
    mut media: mpsc::Receiver<MediaPacket>,
) -> Result<()> {
    let (backend, backend_name) = build_backend(config.backend, &config.engine)?;
    let mut engine = AudioEngine::new(
        config.call_id,
        config.local_author,
        config.engine.clone(),
        backend,
    )?;
    // Apply the join-time gates before the first capture or playback step, so
    // a member who joins muted, deafened or on push-to-talk never sends or
    // plays a frame while the daemon's initial SetState is in flight.
    engine.set_muted(config.muted)?;
    engine.set_deafened(config.deafened)?;
    engine.set_push_to_talk(config.push_to_talk);

    let events = config.events.clone();
    let guild_id = config.guild_id;
    let channel_id = config.channel_id;
    let generation = config.generation;
    let envelope = |event: AudioEvent| AudioEventEnvelope {
        guild_id,
        channel_id,
        generation,
        event,
    };

    // Half a frame keeps command latency below one playback step.
    let tick = Duration::from_micros(u64::from(config.engine.frame_ms) * 1_000 / 2)
        .max(Duration::from_millis(1));
    let mut last_speaking = engine.speaking();
    // Publish the first stats immediately so a caller learns the backend name
    // without waiting a second.
    let mut last_stats = Instant::now()
        .checked_sub(STATS_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut energy = Energy::default();

    let result = drive(
        &mut engine,
        &mut commands,
        &mut media,
        &events,
        &envelope,
        backend_name,
        tick,
        &mut last_speaking,
        &mut last_stats,
        &mut energy,
    );

    // A Stop inside `drive` already closed the device; stop is idempotent, and
    // this also covers the error path and a dropped command handle.
    let stopped = engine.shutdown();
    result?;
    stopped
}

#[allow(clippy::too_many_arguments)]
fn drive(
    engine: &mut AudioEngine,
    commands: &mut mpsc::UnboundedReceiver<AudioCommand>,
    media: &mut mpsc::Receiver<MediaPacket>,
    events: &broadcast::Sender<AudioEventEnvelope>,
    envelope: &impl Fn(AudioEvent) -> AudioEventEnvelope,
    backend_name: &'static str,
    tick: Duration,
    last_speaking: &mut bool,
    last_stats: &mut Instant,
    energy: &mut Energy,
) -> Result<()> {
    loop {
        let mut stop = false;
        loop {
            match commands.try_recv() {
                Ok(AudioCommand::SetState {
                    muted,
                    deafened,
                    push_to_talk,
                }) => {
                    engine.set_muted(muted)?;
                    engine.set_deafened(deafened)?;
                    engine.set_push_to_talk(push_to_talk);
                }
                Ok(AudioCommand::PushToTalkHeld(held)) => engine.set_push_to_talk_held(held),
                Ok(AudioCommand::Playback(samples)) => {
                    // An empty mix is a valid tick with nothing ready; it must
                    // not be written as a zero-length device frame.
                    if !samples.is_empty() {
                        engine.play_local_mix(&samples)?;
                        energy.accumulate(&samples);
                    }
                }
                Ok(AudioCommand::Media(packet)) => {
                    // Media sent through the control channel is still
                    // accepted; it is rare (tests and old callers) and the
                    // bounded queue below is the production path.
                    engine.receive(&packet)?;
                }
                Ok(AudioCommand::Stop) => {
                    stop = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // The handle was dropped; treat it as a stop request.
                    stop = true;
                    break;
                }
            }
        }

        // Drain a bounded slice of remote media, so capture and playback still
        // run under a flood and the queue cannot crowd out the control path.
        for _ in 0..MAX_MEDIA_PER_TICK {
            match media.try_recv() {
                Ok(packet) => engine.receive(&packet)?,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        if stop {
            // Close the device before announcing the stop, as the contract
            // for a clean shutdown.
            engine.shutdown()?;
            let _ = events.send(envelope(AudioEvent::Stopped));
            return Ok(());
        }

        if let Some(packet) = engine.pump_capture()? {
            let _ = events.send(envelope(AudioEvent::Media(packet)));
        }

        if let Some(samples) = engine.pump_playback()? {
            energy.accumulate(&samples);
        }

        let speaking = engine.speaking();
        if speaking != *last_speaking {
            *last_speaking = speaking;
            let _ = events.send(envelope(AudioEvent::Speaking(speaking)));
        }

        if last_stats.elapsed() >= STATS_INTERVAL {
            *last_stats = Instant::now();
            let stats = collect_stats(engine, backend_name, energy);
            let _ = events.send(envelope(AudioEvent::Stats(stats)));
        }

        std::thread::sleep(tick);
    }
}

fn collect_stats(
    engine: &AudioEngine,
    backend_name: &'static str,
    energy: &mut Energy,
) -> AudioStats {
    let stats = engine.stats();
    let backend = engine.backend_stats();
    let (peak, rms) = energy.take();
    AudioStats {
        backend: backend_name.to_string(),
        captured: stats.captured,
        packets: stats.packets,
        played: stats.played,
        concealed: stats.concealed,
        lost: stats.lost,
        decode_errors: stats.decode_errors,
        peers: stats.peers,
        xruns: backend.xruns,
        latency_ms: backend.latency_ms,
        peak,
        rms,
    }
}

/// Peak and RMS accumulated between stats events.
#[derive(Debug, Default)]
struct Energy {
    peak: i16,
    sum_squares: f64,
    count: u64,
}

impl Energy {
    fn accumulate(&mut self, samples: &[i16]) {
        for &sample in samples {
            self.peak = self.peak.max(sample.saturating_abs());
            let value = f64::from(sample);
            self.sum_squares += value * value;
            self.count = self.count.saturating_add(1);
        }
    }

    fn take(&mut self) -> (i16, f32) {
        let rms = if self.count == 0 {
            0.0
        } else {
            (self.sum_squares / self.count as f64).sqrt() as f32
        };
        let peak = self.peak;
        *self = Self::default();
        (peak, rms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_media::packet::MediaKind;

    const GUILD: GuildId = GuildId([0x51; 32]);
    const CHANNEL: ChannelId = ChannelId([0x52; 16]);
    const CALL: CallId = CallId([0x53; 16]);
    const LOCAL: AuthorId = AuthorId([0x54; 32]);
    const REMOTE: AuthorId = AuthorId([0x55; 32]);

    fn config(events: broadcast::Sender<AudioEventEnvelope>) -> AudioCallConfig {
        AudioCallConfig {
            guild_id: GUILD,
            channel_id: CHANNEL,
            call_id: CALL,
            local_author: LOCAL,
            engine: EngineConfig {
                jitter_depth: 1,
                ..EngineConfig::default()
            },
            backend: BackendKind::Null,
            muted: false,
            deafened: false,
            push_to_talk: false,
            events,
            generation: 0,
        }
    }

    #[test]
    fn the_service_emits_media_and_stats_and_stops_cleanly() {
        let (events, mut subscriber) = broadcast::channel(64);
        let (handle, join) = spawn(config(events)).expect("spawn service");

        handle
            .send(AudioCommand::Media(
                MediaPacket::new(MediaKind::Audio, CALL, REMOTE, 0, 0, vec![1, 2, 3])
                    .expect("packet"),
            ))
            .expect("send media");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_media = false;
        let mut saw_stats = false;
        while Instant::now() < deadline && !(saw_media && saw_stats) {
            match subscriber.try_recv() {
                Ok(AudioEventEnvelope { event, .. }) => match event {
                    AudioEvent::Media(_) => saw_media = true,
                    AudioEvent::Stats(stats) => {
                        assert_eq!(stats.backend, "null");
                        saw_stats = true;
                    }
                    AudioEvent::Error(err) => panic!("service error: {err}"),
                    _ => {}
                },
                Err(broadcast::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("event channel closed: {err}"),
            }
        }

        assert!(saw_media, "the capture side never emitted a Media event");
        assert!(saw_stats, "the service never emitted Stats");

        handle.send(AudioCommand::Stop).expect("stop");
        join.join().expect("service thread");

        let mut stopped = false;
        while let Ok(AudioEventEnvelope { event, .. }) = subscriber.try_recv() {
            if event == AudioEvent::Stopped {
                stopped = true;
            }
        }
        assert!(stopped, "Stop must publish AudioEvent::Stopped");
    }

    #[test]
    fn a_dropped_handle_stops_the_service() {
        let (events, mut subscriber) = broadcast::channel(64);
        let (handle, join) = spawn(config(events)).expect("spawn service");
        drop(handle);

        join.join().expect("service thread");

        let mut stopped = false;
        while let Ok(AudioEventEnvelope { event, .. }) = subscriber.try_recv() {
            if event == AudioEvent::Stopped {
                stopped = true;
            }
        }
        assert!(stopped, "a dropped handle must stop the service");
    }

    #[test]
    fn bad_network_media_does_not_stop_the_service() {
        let (events, mut subscriber) = broadcast::channel(64);
        let (handle, join) = spawn(config(events)).expect("spawn service");

        // A non-audio kind and an oversized payload are both possible on the
        // wire. They must be dropped, not treated as a service failure.
        handle
            .send(AudioCommand::Media(
                MediaPacket::new(MediaKind::Video, CALL, REMOTE, 0, 0, vec![1, 2, 3])
                    .expect("packet"),
            ))
            .expect("send video");
        handle
            .send(AudioCommand::Media(MediaPacket {
                version: ovvc_media::MEDIA_VERSION,
                kind: MediaKind::Audio,
                call_id: CALL,
                author: REMOTE,
                seq: 0,
                timestamp_ms: 0,
                payload: vec![0u8; ovvc_media::MAX_MEDIA_PAYLOAD + 1],
            }))
            .expect("send oversized");

        // Give the service a few ticks to process them, then stop cleanly.
        std::thread::sleep(Duration::from_millis(100));
        handle.send(AudioCommand::Stop).expect("stop");
        join.join().expect("service thread");

        let mut saw_error = false;
        let mut saw_stats = false;
        while let Ok(AudioEventEnvelope { event, .. }) = subscriber.try_recv() {
            match event {
                AudioEvent::Error(message) => {
                    saw_error = true;
                    eprintln!("unexpected service error: {message}");
                }
                AudioEvent::Stats(_) => saw_stats = true,
                _ => {}
            }
        }
        assert!(!saw_error, "bad media was treated as a fatal service error");
        assert!(saw_stats, "the service stopped instead of publishing stats");
    }

    #[test]
    fn events_carry_the_service_generation() {
        let (events, mut subscriber) = broadcast::channel(64);
        let mut config = config(events);
        config.generation = 7;
        let (handle, join) = spawn(config).expect("spawn service");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut generation = None;
        while Instant::now() < deadline && generation.is_none() {
            match subscriber.try_recv() {
                Ok(envelope) => generation = Some(envelope.generation),
                Err(broadcast::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("event channel closed: {err}"),
            }
        }
        handle.send(AudioCommand::Stop).expect("stop");
        join.join().expect("service thread");
        assert_eq!(generation, Some(7), "envelopes lost the service generation");
    }
}
