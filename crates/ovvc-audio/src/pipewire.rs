//! A PipeWire capture and playback backend for the voice engine.
//!
//! This module is compiled on Linux only, behind the `pipewire` cargo feature,
//! because it needs the PipeWire development headers at build time. The
//! workspace default build does not enable it; [`AudioBackend`] keeps the
//! engine independent of the device API either way.
//!
//! Two streams are opened: `ovvc-capture` reads the default source and
//! `ovvc-playback` writes to the default sink. Both negotiate S16LE mono at
//! the engine rate with a 10 ms node latency, while the engine consumes and
//! produces 20 ms frames. The process callbacks run in PipeWire's realtime
//! data thread ([`StreamFlags::RT_PROCESS`]) and only move samples through two
//! preallocated single-producer single-consumer rings, using atomics rather
//! than locks, allocation or logging.
//!
//! The main loop is driven from [`PipeWireBackend::roundtrip`], which
//! [`read_capture`](AudioBackend::read_capture) calls while it waits for data.
//! A caller with its own timing loop can call `roundtrip` directly.
//!
//! [`StreamFlags::RT_PROCESS`]: pipewire::stream::StreamFlags::RT_PROCESS

use std::cell::UnsafeCell;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ::pipewire as pw;
use pw::properties::properties;
use pw::spa;

use crate::backend::{AudioBackend, BackendStats, EngineConfig};
use crate::error::{AudioError, Result};

/// Frames buffered on each side on top of the one being processed. Eight
/// 20 ms frames is 160 ms, far more than the scheduler ever needs, while the
/// ring stays small enough that a stalled reader is noticed as an overrun.
const RING_FRAMES: usize = 8;

/// Scratch samples converted per step by the playback callback. A stack
/// buffer keeps the realtime path free of allocation.
const PLAYBACK_SCRATCH: usize = 512;

/// How long [`PipeWireBackend::start`] waits for both streams to stream.
const START_TIMEOUT: Duration = Duration::from_secs(3);

/// A bounded single-producer single-consumer ring of `i16` samples.
///
/// Indices are free-running sample counters; the mask turns them into slots.
/// The producer owns writes up to the published write index and the consumer
/// owns reads up to the published read index, so neither ever touches a slot
/// the other is using.
struct SampleRing {
    data: UnsafeCell<Box<[UnsafeCell<i16>]>>,
    mask: usize,
    write: AtomicUsize,
    read: AtomicUsize,
    overruns: AtomicU64,
    underruns: AtomicU64,
    started: AtomicBool,
}

// Safety: the write and read indices are published with acquire/release
// ordering, and a slot in between them belongs to exactly one side.
unsafe impl Sync for SampleRing {}

impl SampleRing {
    fn new(capacity: usize) -> Self {
        let capacity = capacity.max(2).next_power_of_two();
        let data = (0..capacity)
            .map(|_| UnsafeCell::new(0i16))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            data: UnsafeCell::new(data),
            mask: capacity - 1,
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            overruns: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            started: AtomicBool::new(false),
        }
    }

    fn capacity(&self) -> usize {
        self.mask + 1
    }

    fn slot(&self, index: usize) -> &UnsafeCell<i16> {
        // Safety: `data` is never resized after construction, and the mask
        // keeps the index inside the slice.
        let data: &[UnsafeCell<i16>] = unsafe { &*self.data.get() };
        &data[index & self.mask]
    }

    fn write_slot(&self, index: usize, value: i16) {
        // Safety: the caller is the single producer and owns this slot.
        unsafe { *self.slot(index).get() = value };
    }

    fn read_slot(&self, index: usize) -> i16 {
        // Safety: the caller is the single consumer and the producer has
        // published this slot with release ordering.
        unsafe { *self.slot(index).get() }
    }

    /// Queue interleaved samples. Any that do not fit are dropped and count
    /// as one overrun.
    fn push(&self, samples: &[i16]) {
        self.started.store(true, Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        let write = self.write.load(Ordering::Relaxed);
        let free = self.capacity() - write.wrapping_sub(read);
        let kept = samples.len().min(free);

        for (offset, &sample) in samples.iter().take(kept).enumerate() {
            self.write_slot(write.wrapping_add(offset), sample);
        }
        self.write
            .store(write.wrapping_add(kept), Ordering::Release);

        if kept < samples.len() {
            self.overruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Queue little-endian S16 samples straight out of a PipeWire chunk.
    fn push_le_bytes(&self, bytes: &[u8]) {
        self.started.store(true, Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        let write = self.write.load(Ordering::Relaxed);
        let free = self.capacity() - write.wrapping_sub(read);
        let supplied = bytes.len() / 2;
        let kept = supplied.min(free);

        for (offset, pair) in bytes.as_chunks::<2>().0.iter().take(kept).enumerate() {
            self.write_slot(write.wrapping_add(offset), i16::from_le_bytes(*pair));
        }
        self.write
            .store(write.wrapping_add(kept), Ordering::Release);

        if kept < supplied {
            self.overruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Copy up to `out.len()` samples, returning how many were available.
    fn pop_partial(&self, out: &mut [i16]) -> usize {
        let read = self.read.load(Ordering::Relaxed);
        let count = out.len().min(self.available());

        for (offset, slot) in out.iter_mut().take(count).enumerate() {
            *slot = self.read_slot(read.wrapping_add(offset));
        }
        self.read.store(read.wrapping_add(count), Ordering::Release);
        count
    }

    fn available(&self) -> usize {
        self.write
            .load(Ordering::Acquire)
            .wrapping_sub(self.read.load(Ordering::Relaxed))
    }

    /// Copy exactly one frame, or nothing when it is not buffered yet.
    fn pop(&self, out: &mut [i16]) -> bool {
        if out.len() <= self.available() {
            self.pop_partial(out) == out.len()
        } else {
            false
        }
    }

    fn note_underrun(&self) {
        // A playback stream that has never been given a frame is silent by
        // design, not starved, so ignore those callbacks.
        if self.started.load(Ordering::Relaxed) {
            self.underruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Discard everything buffered by moving the read index to the write
    /// index. Only safe while the producer is paused, which is how capture is
    /// flushed on unmute.
    fn clear(&self) {
        let write = self.write.load(Ordering::Acquire);
        self.read.store(write, Ordering::Release);
    }

    fn overruns(&self) -> u64 {
        self.overruns.load(Ordering::Relaxed)
    }

    fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }
}

/// The streams and their event listeners. Dropped together so a listener is
/// always unregistered before the stream it belongs to.
struct Streams {
    capture_listener: pw::stream::StreamListener<()>,
    playback_listener: pw::stream::StreamListener<()>,
    capture: pw::stream::StreamRc,
    playback: pw::stream::StreamRc,
}

/// The native PipeWire capture and playback device.
///
/// [`start`](AudioBackend::start) opens both streams against the default
/// source and sink. If the session or a default device is missing it returns
/// [`AudioError::Backend`] with the PipeWire error, never a panic.
pub struct PipeWireBackend {
    streams: Option<Streams>,
    capture_ring: Option<Arc<SampleRing>>,
    playback_ring: Option<Arc<SampleRing>>,
    // Declared after `streams` so the streams are torn down before the
    // objects they were created from.
    main_loop: Option<pw::main_loop::MainLoopRc>,
    context: Option<pw::context::ContextRc>,
    core: Option<pw::core::CoreRc>,
    config: EngineConfig,
    stats: BackendStats,
    /// True while capture was paused on purpose, so an empty ring is not
    /// reported as an underrun.
    capture_paused: bool,
}

impl PipeWireBackend {
    /// A backend with no device open. Call [`AudioBackend::start`] to open it.
    pub fn new() -> Self {
        Self {
            streams: None,
            capture_ring: None,
            playback_ring: None,
            main_loop: None,
            context: None,
            core: None,
            config: EngineConfig::default(),
            stats: BackendStats::default(),
            capture_paused: false,
        }
    }

    /// Drive the PipeWire main loop for about `frames` frames of the
    /// configured duration.
    ///
    /// The realtime process callbacks move samples on their own thread, but
    /// state changes, format negotiation and stream errors are dispatched from
    /// the main loop, so a caller with its own loop must call this. `frames`
    /// of zero dispatches whatever is pending without waiting.
    pub fn roundtrip(&mut self, frames: usize) -> Result<()> {
        let Some(main_loop) = self.main_loop.as_ref() else {
            return Ok(());
        };
        let budget =
            Duration::from_millis(u64::from(self.config.frame_ms).saturating_mul(frames as u64));
        let started = Instant::now();
        loop {
            let timeout = if frames == 0 {
                pw::loop_::Timeout::None
            } else {
                pw::loop_::Timeout::Finite(Duration::from_millis(5))
            };
            main_loop.loop_().iterate(timeout);
            if frames == 0 || started.elapsed() >= budget {
                return Ok(());
            }
        }
    }

    /// Pause or resume the capture stream.
    ///
    /// A paused stream stops delivering buffers, so the capture ring cannot
    /// overrun or hold audio that was captured while the caller was mute.
    /// Resuming flushes whatever was buffered before the pause.
    pub fn set_capture_active(&mut self, active: bool) -> Result<()> {
        if let Some(streams) = self.streams.as_ref() {
            if active && let Some(ring) = self.capture_ring.as_ref() {
                ring.clear();
            }
            streams
                .capture
                .set_active(active)
                .map_err(|err| backend_error("capture active", err))?;
        }
        self.capture_paused = !active;
        Ok(())
    }

    /// Turn a stream error reported after start into a backend error.
    ///
    /// The start path already waits for both streams, but a device can fail
    /// later, for example when it is unplugged mid-call. An `Unconnected`
    /// state after start means the node lost its device, which would
    /// otherwise look like a healthy but permanently empty source.
    fn check_stream_health(&self) -> Result<()> {
        let Some(streams) = self.streams.as_ref() else {
            return Ok(());
        };
        check_stream_state(&streams.capture.state(), "capture")?;
        check_stream_state(&streams.playback.state(), "playback")?;
        Ok(())
    }

    /// The estimated output latency in milliseconds.
    ///
    /// PipeWire reports the device delay in ticks on the playback stream; the
    /// configured frame length is the conservative fallback before the graph
    /// has run or after [`AudioBackend::stop`].
    fn latency_ms(&self) -> u64 {
        let fallback = u64::from(self.config.frame_ms);
        let Some(streams) = self.streams.as_ref() else {
            return fallback;
        };
        let Ok(time) = streams.playback.time() else {
            return fallback;
        };
        let rate = time.rate();
        let delay = time.delay();
        if delay <= 0 || rate.denom == 0 {
            return fallback;
        }
        let seconds = delay as f64 * f64::from(rate.num) / f64::from(rate.denom);
        if !seconds.is_finite() || seconds < 0.0 {
            return fallback;
        }
        (seconds * 1_000.0).round().max(1.0) as u64
    }
}

impl Default for PipeWireBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioBackend for PipeWireBackend {
    fn start(&mut self, config: &EngineConfig) -> Result<()> {
        config.validate()?;
        self.stop()?;

        pw::init();

        let main_loop =
            pw::main_loop::MainLoopRc::new(None).map_err(|err| backend_error("main loop", err))?;
        let context = pw::context::ContextRc::new(&main_loop, None)
            .map_err(|err| backend_error("context", err))?;
        let core = context.connect_rc(None).map_err(connect_error)?;

        let capture_ring = Arc::new(SampleRing::new(RING_FRAMES * config.frame_samples()));
        let playback_ring = Arc::new(SampleRing::new(RING_FRAMES * config.frame_samples()));

        let capture = pw::stream::StreamRc::new(
            core.clone(),
            "ovvc-capture",
            properties! {
                *pw::keys::NODE_NAME => "ovvc-capture",
                *pw::keys::NODE_DESCRIPTION => "ovvc voice capture",
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Communication",
                *pw::keys::NODE_LATENCY => "480/48000",
            },
        )
        .map_err(|err| backend_error("capture stream", err))?;

        let capture_ring_for_callback = Arc::clone(&capture_ring);
        let capture_listener = capture
            .add_local_listener::<()>()
            .process(move |stream, ()| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let (offset, size) = {
                    let chunk = data.chunk();
                    (chunk.offset() as usize, chunk.size() as usize)
                };
                if size == 0 {
                    return;
                }
                let Some(bytes) = data.data() else {
                    return;
                };
                let end = offset.saturating_add(size).min(bytes.len());
                if end > offset {
                    capture_ring_for_callback.push_le_bytes(&bytes[offset..end]);
                }
            })
            .register()
            .map_err(|err| backend_error("capture listener", err))?;

        let playback = pw::stream::StreamRc::new(
            core.clone(),
            "ovvc-playback",
            properties! {
                *pw::keys::NODE_NAME => "ovvc-playback",
                *pw::keys::NODE_DESCRIPTION => "ovvc voice playback",
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::MEDIA_ROLE => "Communication",
                *pw::keys::NODE_LATENCY => "480/48000",
            },
        )
        .map_err(|err| backend_error("playback stream", err))?;

        let playback_ring_for_callback = Arc::clone(&playback_ring);
        let playback_listener = playback
            .add_local_listener::<()>()
            .process(move |stream, ()| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                // `requested` is the frame count PipeWire wants this cycle.
                // Filling the whole mapping instead would hand the graph a
                // multi-hundred-millisecond chunk and add that much latency.
                let requested = buffer.requested() as usize;
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let mut scratch = [0i16; PLAYBACK_SCRATCH];
                let mut starved = false;
                let sample_bytes = {
                    let Some(bytes) = data.data() else {
                        return;
                    };
                    let mapping_bytes = bytes.len() - bytes.len() % 2;
                    let samples = if requested == 0 {
                        mapping_bytes / 2
                    } else {
                        requested.min(mapping_bytes / 2)
                    };
                    let sample_bytes = samples * 2;
                    let mut done = 0;
                    while done < samples {
                        let count = (samples - done).min(PLAYBACK_SCRATCH);
                        let taken = playback_ring_for_callback.pop_partial(&mut scratch[..count]);
                        if taken < count {
                            starved = true;
                        }
                        for (index, &sample) in scratch[..count].iter().enumerate() {
                            let value = if index < taken { sample } else { 0 };
                            let at = (done + index) * 2;
                            bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
                        }
                        done += count;
                    }
                    sample_bytes
                };
                if starved {
                    playback_ring_for_callback.note_underrun();
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = 2;
                *chunk.size_mut() = sample_bytes as u32;
            })
            .register()
            .map_err(|err| backend_error("playback listener", err))?;

        connect_stream(&capture, spa::utils::Direction::Input, config)?;
        connect_stream(&playback, spa::utils::Direction::Output, config)?;

        wait_for_streaming(&main_loop, &capture, &playback)?;

        self.streams = Some(Streams {
            capture_listener,
            playback_listener,
            capture,
            playback,
        });
        self.capture_ring = Some(capture_ring);
        self.playback_ring = Some(playback_ring);
        self.main_loop = Some(main_loop);
        self.context = Some(context);
        self.core = Some(core);
        self.config = config.clone();
        self.stats = BackendStats::default();
        Ok(())
    }

    fn set_capture_active(&mut self, active: bool) -> Result<()> {
        PipeWireBackend::set_capture_active(self, active)
    }

    fn read_capture(&mut self) -> Result<Option<Vec<i16>>> {
        self.check_stream_health()?;
        let Some(ring) = self.capture_ring.clone() else {
            return Err(AudioError::Backend(
                "PipeWireBackend::start() has not been called".to_string(),
            ));
        };
        let mut frame = vec![0i16; self.config.frame_samples()];
        if ring.pop(&mut frame) {
            self.stats.capture_frames = self.stats.capture_frames.saturating_add(1);
            return Ok(Some(frame));
        }

        // Give the graph one frame to deliver more samples, and dispatch any
        // pending stream events while we wait.
        self.roundtrip(1)?;
        if ring.pop(&mut frame) {
            self.stats.capture_frames = self.stats.capture_frames.saturating_add(1);
            Ok(Some(frame))
        } else {
            // An empty ring after a roundtrip means the source starved.
            // Intentional pauses are not underruns.
            if !self.capture_paused {
                ring.note_underrun();
            }
            Ok(None)
        }
    }

    fn write_playback(&mut self, samples: &[i16]) -> Result<()> {
        self.check_stream_health()?;
        let Some(ring) = self.playback_ring.as_ref() else {
            return Err(AudioError::Backend(
                "PipeWireBackend::start() has not been called".to_string(),
            ));
        };
        if samples.len() != self.config.frame_samples() {
            return Err(AudioError::Backend(format!(
                "playback frame is {} samples, expected {}",
                samples.len(),
                self.config.frame_samples()
            )));
        }
        ring.push(samples);
        self.stats.playback_frames = self.stats.playback_frames.saturating_add(1);
        Ok(())
    }

    fn stats(&self) -> BackendStats {
        let xruns = self
            .capture_ring
            .as_ref()
            .map_or(0, |ring| ring.overruns().saturating_add(ring.underruns()))
            .saturating_add(
                self.playback_ring
                    .as_ref()
                    .map_or(0, |ring| ring.overruns().saturating_add(ring.underruns())),
            );
        BackendStats {
            capture_frames: self.stats.capture_frames,
            playback_frames: self.stats.playback_frames,
            xruns,
            latency_ms: self.latency_ms(),
        }
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(streams) = self.streams.take() {
            let Streams {
                capture_listener,
                playback_listener,
                capture,
                playback,
            } = streams;
            // Unregister the callbacks before disconnecting so no process
            // callback can observe a stream being torn down.
            drop(capture_listener);
            drop(playback_listener);
            let _ = capture.disconnect();
            let _ = playback.disconnect();
        }
        self.main_loop = None;
        self.context = None;
        self.core = None;
        // Detach the rings too, so a read or write after stop is an error
        // rather than a silent operation on a ring nothing is draining.
        self.capture_ring = None;
        self.playback_ring = None;
        self.stats = BackendStats::default();
        self.capture_paused = false;
        Ok(())
    }
}

/// Serialise the S16LE mono format requested by the engine.
fn format_bytes(config: &EngineConfig) -> Result<Vec<u8>> {
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(config.sample_rate);
    audio_info.set_channels(u32::from(config.channels));
    let mut position = [0u32; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    audio_info.set_position(position);

    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let serialized = spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(|err| AudioError::Backend(format!("pipewire format serialisation: {err}")))?;
    Ok(serialized.0.into_inner())
}

fn connect_stream(
    stream: &pw::stream::StreamRc,
    direction: spa::utils::Direction,
    config: &EngineConfig,
) -> Result<()> {
    let bytes = format_bytes(config)?;
    let pod = spa::pod::Pod::from_bytes(&bytes)
        .ok_or_else(|| AudioError::Backend("pipewire format pod is malformed".to_string()))?;
    let mut params = [pod];
    stream
        .connect(
            direction,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|err| backend_error("stream connect", err))
}

/// Run the main loop until both streams are streaming, a stream reports an
/// error, or the start timeout expires.
fn wait_for_streaming(
    main_loop: &pw::main_loop::MainLoopRc,
    capture: &pw::stream::StreamRc,
    playback: &pw::stream::StreamRc,
) -> Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let capture_state = capture.state();
        let playback_state = playback.state();

        if let pw::stream::StreamState::Error(message) = &capture_state {
            return Err(AudioError::Backend(format!(
                "PipeWire capture stream failed (no default source, or format negotiation failed): {message}"
            )));
        }
        if let pw::stream::StreamState::Error(message) = &playback_state {
            return Err(AudioError::Backend(format!(
                "PipeWire playback stream failed (no default sink, or format negotiation failed): {message}"
            )));
        }
        if capture_state == pw::stream::StreamState::Streaming
            && playback_state == pw::stream::StreamState::Streaming
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(AudioError::Backend(format!(
                "PipeWire streams did not reach Streaming within {} s (capture {capture_state:?}, \
                 playback {playback_state:?}); check that a default source and sink exist with `wpctl status`",
                START_TIMEOUT.as_secs()
            )));
        }
        main_loop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(10)));
    }
}

/// Classify a stream state observed after start.
///
/// `Paused` is not an error because [`PipeWireBackend::set_capture_active`]
/// uses it deliberately. `Unconnected` is fatal here: start only returns once
/// both streams are streaming, so becoming unconnected later means the node
/// lost its device.
fn check_stream_state(state: &pw::stream::StreamState, name: &str) -> Result<()> {
    match state {
        pw::stream::StreamState::Error(message) => Err(AudioError::Backend(format!(
            "PipeWire {name} stream failed: {message}"
        ))),
        pw::stream::StreamState::Unconnected => Err(AudioError::Backend(format!(
            "PipeWire {name} stream became unconnected after start; the device may have been removed"
        ))),
        _ => Ok(()),
    }
}

fn backend_error(what: &str, err: pw::Error) -> AudioError {
    AudioError::Backend(format!("pipewire {what}: {err}"))
}

fn connect_error(err: pw::Error) -> AudioError {
    let runtime = std::env::var("PIPEWIRE_RUNTIME_DIR")
        .or_else(|_| std::env::var("XDG_RUNTIME_DIR"))
        .unwrap_or_else(|_| "unset".to_string());
    AudioError::Backend(format!(
        "cannot connect to PipeWire (runtime dir {runtime}); is a PipeWire session running? {err}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_drops_samples_it_cannot_hold() {
        let ring = SampleRing::new(4);
        ring.push(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(ring.overruns(), 1);

        let mut out = [0i16; 4];
        assert!(ring.pop(&mut out));
        assert_eq!(out, [1, 2, 3, 4]);
        assert!(!ring.pop(&mut out));
    }

    #[test]
    fn the_ring_wraps_around_its_capacity() {
        let ring = SampleRing::new(4);
        let mut out = [0i16; 2];
        for generation in 0..4i16 {
            ring.push(&[generation, generation + 1]);
            assert!(ring.pop(&mut out));
            assert_eq!(out, [generation, generation + 1]);
        }
    }

    #[test]
    fn bytes_are_read_as_little_endian_samples() {
        let ring = SampleRing::new(4);
        ring.push_le_bytes(&[0x34, 0x12, 0xFF, 0xFF]);
        let mut out = [0i16; 2];
        assert!(ring.pop(&mut out));
        assert_eq!(out, [0x1234, -1]);
    }

    #[test]
    fn a_partial_pop_only_takes_available_samples() {
        let ring = SampleRing::new(8);
        ring.push(&[7, 8]);
        let mut out = [0i16; 4];
        assert_eq!(ring.pop_partial(&mut out), 2);
        assert_eq!(out, [7, 8, 0, 0]);
        assert_eq!(ring.pop_partial(&mut out), 0);
    }

    fn backend_with_rings() -> PipeWireBackend {
        let config = EngineConfig::default();
        let capacity = 8 * config.frame_samples();
        let mut backend = PipeWireBackend::new();
        backend.config = config;
        backend.capture_ring = Some(Arc::new(SampleRing::new(capacity)));
        backend.playback_ring = Some(Arc::new(SampleRing::new(capacity)));
        backend.stats.capture_frames = 7;
        backend
    }

    #[test]
    fn stop_releases_the_rings_and_resets_stats() {
        let mut backend = backend_with_rings();
        backend.stop().expect("stop");

        assert!(backend.capture_ring.is_none(), "capture ring survived stop");
        assert!(
            backend.playback_ring.is_none(),
            "playback ring survived stop"
        );
        assert_eq!(backend.stats, BackendStats::default());
        assert!(
            backend.read_capture().is_err(),
            "read after stop must not succeed on a detached ring"
        );
        assert!(
            backend.write_playback(&[0i16; 960]).is_err(),
            "write after stop must not vanish into a detached ring"
        );
    }

    #[test]
    fn capture_underruns_are_counted() {
        let config = EngineConfig::default();
        let mut backend = PipeWireBackend::new();
        backend.config = config.clone();
        let ring = Arc::new(SampleRing::new(8 * config.frame_samples()));
        // One sample marks the ring as streaming but cannot fill a frame.
        ring.push(&[1]);
        backend.capture_ring = Some(ring);

        assert!(backend.read_capture().expect("read").is_none());
        assert!(
            backend.stats().xruns >= 1,
            "an empty capture ring after a roundtrip must count as an underrun"
        );
    }

    #[test]
    fn stream_health_reports_a_device_that_became_unconnected() {
        use pw::stream::StreamState;

        assert!(check_stream_state(&StreamState::Streaming, "capture").is_ok());
        assert!(
            check_stream_state(&StreamState::Paused, "capture").is_ok(),
            "a deliberate pause must not be reported as a failure"
        );
        assert!(check_stream_state(&StreamState::Connecting, "capture").is_ok());
        assert!(check_stream_state(&StreamState::Unconnected, "capture").is_err());
        assert!(check_stream_state(&StreamState::Error("gone".to_string()), "capture").is_err());
    }

    #[test]
    fn a_zero_frame_roundtrip_returns_without_waiting_for_an_event() {
        pw::init();
        let mut backend = PipeWireBackend::new();
        backend.main_loop = Some(pw::main_loop::MainLoopRc::new(None).expect("main loop"));

        let started = Instant::now();
        // The first call may consume a queued startup event; the later calls
        // have nothing pending and must still return.
        for _ in 0..4 {
            backend.roundtrip(0).expect("roundtrip");
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "roundtrip(0) blocked waiting for an event"
        );
    }
}
