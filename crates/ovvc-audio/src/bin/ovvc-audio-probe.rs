//! A smoke test for the voice engine and the audio backends.
//!
//! With the default `--backend null` the probe drives the null backend through
//! the same engine a real call uses, checks that a synthetic tone survives
//! encode, jitter and decode with usable energy, and prints one compact result
//! line. It exits non-zero when the energy check fails or any stage reports an
//! error, so CI can run it as a binary test.
//!
//! With `--backend pipewire` (Linux, built with `--features pipewire`) it
//! captures from the default PipeWire source for `--seconds`, reports frames,
//! peak, RMS, xruns and latency, and with `--play` sends the captured frames
//! back to the default sink. A missing session or default device is reported as
//! an error and exits 2, never a panic.
//!
//! `--opus false` swaps Opus for the trivial PCM codec from `ovvc-media`, which
//! is useful as a control when judging the energy number. PCM does not fit a
//! 20 ms frame inside the 1200-byte media packet, so that path uses 10 ms
//! frames. The PipeWire path always captures 20 ms frames.

use clap::Parser;
use ovvc_audio::backend::{AudioBackend, EngineConfig, NullBackend};
use ovvc_audio::engine::{AudioEngine, EngineStats};
use ovvc_audio::error::{AudioError, Result};
use ovvc_core::AuthorId;
use ovvc_media::MediaError;
use ovvc_media::call::CallId;
use ovvc_media::loopback::{LoopbackHarness, PcmCodec};

#[cfg(all(target_os = "linux", feature = "pipewire"))]
use ovvc_audio::{OpusCodec, PipeWireBackend};

/// Amplitude [`SyntheticSource`] generates, in signed 16-bit units.
///
/// [`SyntheticSource`]: ovvc_media::loopback::SyntheticSource
const TONE_AMPLITUDE: f32 = 12_000.0;

/// The loopback identity the null probe stamps on its own packets.
const PROBE_CALL: CallId = CallId([0x6f; 16]);
const PROBE_AUTHOR: AuthorId = AuthorId([0x76; 32]);

#[derive(Debug, Parser)]
#[command(
    name = "ovvc-audio-probe",
    about = "Run the ovvc voice engine against the null audio backend"
)]
struct Args {
    /// Audio backend: `null` (hardware-free) or `pipewire` (Linux, feature-gated).
    #[arg(long, default_value = "null")]
    backend: String,

    /// Length of the probe in seconds.
    #[arg(long, default_value_t = 1.0)]
    seconds: f64,

    /// Frequency of the synthetic tone in Hz. Zero is silence.
    #[arg(long, default_value_t = 1_000.0)]
    tone_hz: f32,

    /// Use the Opus codec; `--opus false` uses the PCM control codec.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    opus: bool,

    /// With the PipeWire backend, play the captured frames back afterwards.
    /// The null backend already loops its own output into the measuring sink.
    #[arg(long, default_value_t = false)]
    play: bool,
}

fn main() {
    match run(Args::parse()) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(err) => {
            eprintln!("ovvc-audio-probe: {err}");
            std::process::exit(2);
        }
    }
}

fn run(args: Args) -> Result<bool> {
    if !args.seconds.is_finite() || args.seconds <= 0.0 {
        return Err(AudioError::Config(
            "--seconds must be a positive number".to_string(),
        ));
    }
    if !args.tone_hz.is_finite() || args.tone_hz < 0.0 {
        return Err(AudioError::Config(
            "--tone-hz must be zero or greater".to_string(),
        ));
    }

    match args.backend.as_str() {
        "null" => {
            if args.opus {
                run_opus(args.seconds, args.tone_hz)
            } else {
                run_pcm(args.seconds, args.tone_hz)
            }
        }
        "pipewire" => run_pipewire(&args),
        other => Err(AudioError::Config(format!(
            "unsupported backend {other:?}; expected `null` or `pipewire`"
        ))),
    }
}

#[cfg(all(target_os = "linux", feature = "pipewire"))]
fn run_pipewire(args: &Args) -> Result<bool> {
    use std::time::{Duration, Instant};

    let config = EngineConfig {
        sample_rate: 48_000,
        frame_ms: 20,
        channels: 1,
        bitrate_kbps: 16,
        jitter_depth: 3,
        ..EngineConfig::default()
    };
    config.validate()?;

    let mut backend = PipeWireBackend::new();
    backend.start(&config)?;

    let mut codec = if args.opus {
        Some(OpusCodec::new(
            config.sample_rate,
            config.frame_ms,
            config.bitrate_kbps,
        )?)
    } else {
        None
    };

    let deadline = Instant::now() + Duration::from_secs_f64(args.seconds);
    let mut frames: Vec<Vec<i16>> = Vec::new();
    let mut peak = 0i16;
    let mut sum_squares = 0f64;
    let mut sample_count = 0u64;
    let mut decoded_peak = 0i16;
    let mut decoded_sum_squares = 0f64;
    let mut decoded_sample_count = 0u64;

    while Instant::now() < deadline {
        let Some(frame) = backend.read_capture()? else {
            continue;
        };
        accumulate(&frame, &mut peak, &mut sum_squares, &mut sample_count);
        if let Some(codec) = codec.as_mut() {
            let payload = codec.encode(&frame)?;
            let decoded = codec.decode(&payload)?;
            accumulate(
                &decoded,
                &mut decoded_peak,
                &mut decoded_sum_squares,
                &mut decoded_sample_count,
            );
        }
        frames.push(frame);
    }

    let mut played = 0u64;
    if args.play {
        // Nothing drains capture during playback, so pause it rather than let
        // its ring overrun and report false xruns.
        backend.set_capture_active(false)?;
        // Prime the playback ring with two frames so a scheduler jitter in the
        // process callback does not immediately starve it, then send the rest
        // one frame per frame period.
        let prime = frames.len().min(2);
        for frame in &frames[..prime] {
            backend.write_playback(frame)?;
            played = played.saturating_add(1);
        }
        for frame in &frames[prime..] {
            backend.write_playback(frame)?;
            backend.roundtrip(1)?;
            played = played.saturating_add(1);
        }
        // Let the last queued frames reach the sink before the streams close.
        backend.roundtrip(2)?;
    }

    let backend_stats = backend.stats();
    backend.stop()?;

    let captured = backend_stats.capture_frames;
    let rms = if sample_count == 0 {
        0.0
    } else {
        (sum_squares / sample_count as f64).sqrt() as f32
    };
    let decoded_rms = if decoded_sample_count == 0 {
        0.0
    } else {
        (decoded_sum_squares / decoded_sample_count as f64).sqrt() as f32
    };
    let codec_name = if args.opus { "opus" } else { "pcm" };
    let signal = if peak > 0 { "PASS" } else { "SILENT" };

    println!(
        "backend=pipewire codec={codec_name} frame_ms={} seconds={} captured={captured} \
         peak={peak} rms={rms:.1} decoded_peak={decoded_peak} decoded_rms={decoded_rms:.1} \
         xruns={} latency_ms={} played={played} signal={signal}",
        config.frame_ms, args.seconds, backend_stats.xruns, backend_stats.latency_ms,
    );

    Ok(captured > 0)
}

#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
fn run_pipewire(_args: &Args) -> Result<bool> {
    Err(AudioError::Config(
        "this build does not include the PipeWire backend; rebuild with \
         `--features pipewire` on a Linux target after `source scripts/audio-dev-env.sh`"
            .to_string(),
    ))
}

fn run_opus(seconds: f64, tone_hz: f32) -> Result<bool> {
    let config = EngineConfig {
        sample_rate: 48_000,
        frame_ms: 20,
        channels: 1,
        bitrate_kbps: 16,
        jitter_depth: 1,
        ..EngineConfig::default()
    };
    config.validate()?;

    let frames = frame_count(seconds, config.frame_ms);
    let backend = NullBackend::with_tone(config.clone(), tone_hz);
    let mut engine = AudioEngine::new(PROBE_CALL, PROBE_AUTHOR, config.clone(), Box::new(backend))?;
    let stats = engine.run_for(frames)?;

    Ok(report("opus", config.frame_ms, frames, &stats))
}

fn run_pcm(seconds: f64, tone_hz: f32) -> Result<bool> {
    // 10 ms of mono PCM is 960 bytes, under the 1200-byte payload limit.
    let config = EngineConfig {
        sample_rate: 48_000,
        frame_ms: 10,
        channels: 1,
        bitrate_kbps: 16,
        jitter_depth: 1,
        ..EngineConfig::default()
    };
    config.validate()?;

    let frames = frame_count(seconds, config.frame_ms);
    let mut backend = NullBackend::with_tone(config.clone(), tone_hz);
    backend.start(&config)?;

    let mut harness = LoopbackHarness::new(
        CallId([0x70; 16]),
        AuthorId([0x72; 32]),
        Box::new(PcmCodec),
        config.jitter_depth,
    );

    let mut played = 0u64;
    let mut peak = 0i16;
    let mut sum_squares = 0f64;
    let mut sample_count = 0u64;

    for index in 0..frames {
        let Some(frame) = backend.read_capture()? else {
            break;
        };
        let timestamp = index as u64 * u64::from(config.frame_ms);
        let packet = harness
            .encode_frame(&frame, timestamp)
            .map_err(media_error)?;
        harness.receive(&packet).map_err(media_error)?;
        if let Some(samples) = harness.play().map_err(media_error)? {
            backend.write_playback(&samples)?;
            played = played.saturating_add(1);
            accumulate(&samples, &mut peak, &mut sum_squares, &mut sample_count);
        }
    }

    let backend_stats = backend.stats();
    let harness_stats = harness.stats();
    let stats = EngineStats {
        captured: backend_stats.capture_frames,
        packets: harness_stats.encoded_frames,
        played,
        concealed: harness_stats.concealed,
        lost: harness_stats.lost,
        decode_errors: 0,
        bad_packets: 0,
        peers: 0,
        mixed_frames: 0,
        peak,
        rms: if sample_count == 0 {
            0.0
        } else {
            (sum_squares / sample_count as f64).sqrt() as f32
        },
        backend_xruns: backend_stats.xruns,
    };

    Ok(report("pcm", config.frame_ms, frames, &stats))
}

fn frame_count(seconds: f64, frame_ms: u16) -> usize {
    let frames = (seconds * 1_000.0 / f64::from(frame_ms)).round() as usize;
    frames.max(1)
}

fn report(codec: &str, frame_ms: u16, frames: usize, stats: &EngineStats) -> bool {
    let expected_rms = TONE_AMPLITUDE / 2f32.sqrt();
    let pass = stats.peak > 0 && stats.rms >= expected_rms * 0.25;

    println!(
        "backend=null codec={codec} frame_ms={frame_ms} frames={frames} \
         captured={} packets={} played={} concealed={} lost={} peak={} rms={:.1} energy={}",
        stats.captured,
        stats.packets,
        stats.played,
        stats.concealed,
        stats.lost,
        stats.peak,
        stats.rms,
        if pass { "PASS" } else { "FAIL" }
    );
    pass
}

fn accumulate(samples: &[i16], peak: &mut i16, sum_squares: &mut f64, count: &mut u64) {
    for &sample in samples {
        *peak = (*peak).max(sample.saturating_abs());
        let value = f64::from(sample);
        *sum_squares += value * value;
        *count = count.saturating_add(1);
    }
}

fn media_error(err: MediaError) -> AudioError {
    AudioError::Backend(format!("media: {err}"))
}
