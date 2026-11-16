//! Real capacity samples for the media-hub election.
//!
//! [`Priority`] used to be a constant placeholder, so the election could not
//! respond to a loaded machine, a bad link or a hub that keeps failing to
//! decode and send. This module reads the local machine and the live transport
//! counters once per control tick and folds them into the tuple the daemon
//! advertises in `Claim` and `Heartbeat` frames.
//!
//! The parsers are pure and the sampler is time-injected, so the arithmetic is
//! tested without `/proc` or a network. Reading the file system is a thin
//! wrapper that degrades to a conservative value when a file is missing, which
//! keeps a daemon on a non-Linux host eligible rather than silent.

use std::sync::atomic::{AtomicU64, Ordering};

use ovvc_control::Priority;

/// The upload floor advertised when no media has been measured yet. It equals
/// the election's eligibility floor, so an idle but healthy machine is still a
/// candidate and the measurement can rise above it under load.
const IDLE_UPLOAD_KBPS: u32 = 256;
/// The matching download floor. A hub receives every leaf's uplink, so this is
/// twice the upload floor, as it is in the election configuration.
const IDLE_DOWNLOAD_KBPS: u32 = 512;
/// Opus voice packets are small and close to this size at the default profile.
/// It is only used to turn a byte delta into an approximate packet count for
/// the loss percentage, never for routing.
const AVERAGE_OPUS_BYTES: u64 = 90;

/// The shortest window a CPU or transport delta is trusted over.
///
/// The control tick asks for a sample once a second, but a busy daemon can
/// fall behind and then run several ticks back to back. A delta over a few
/// milliseconds says nothing about sustained load: a handful of busy jiffies
/// out of a handful reads as 100% CPU. Samples closer together than this keep
/// the previous value and leave the baseline in place so the next sample covers
/// the full window.
const MIN_SAMPLE_WINDOW_MS: u64 = 500;

/// One `/proc/stat` CPU sample.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuSample {
    /// Every counter on the aggregate `cpu` line, including idle time.
    pub total: u64,
    /// Idle plus I/O-wait time, which is the part that is not load.
    pub idle: u64,
}

impl CpuSample {
    /// Parse the aggregate `cpu` line.
    ///
    /// The first line is `cpu  user nice system idle iowait irq softirq steal
    /// guest guest_nice`. Guest time is already included in user time, so it
    /// is not added twice.
    pub fn parse(stat: &str) -> Option<Self> {
        let line = stat
            .lines()
            .find(|line| line.starts_with("cpu "))?
            .trim_start_matches("cpu ");
        let mut total = 0u64;
        let mut idle = 0u64;
        for (index, field) in line.split_whitespace().enumerate() {
            if index >= 8 {
                break;
            }
            let value: u64 = field.parse().ok()?;
            total = total.saturating_add(value);
            if index == 3 || index == 4 {
                idle = idle.saturating_add(value);
            }
        }
        Some(Self { total, idle })
    }

    /// Load percentage since `previous`, clamped to 0..=100.
    ///
    /// `None` when there is no usable delta, which the sampler reports as zero
    /// rather than inventing a load.
    pub fn load_pct(self, previous: Self) -> Option<u8> {
        let total = self.total.checked_sub(previous.total)?;
        let idle = self.idle.checked_sub(previous.idle)?;
        if total == 0 {
            return None;
        }
        let busy = total.saturating_sub(idle);
        Some(((busy.saturating_mul(100) / total).min(100)) as u8)
    }
}

/// Parse `/proc/meminfo` into `(total_kb, available_kb)`.
pub fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        match fields.next()? {
            "MemTotal:" => total = fields.next()?.parse::<u64>().ok(),
            "MemAvailable:" => available = fields.next()?.parse::<u64>().ok(),
            _ => {}
        }
        if total.is_some() && available.is_some() {
            break;
        }
    }
    Some((total?, available?))
}

/// Memory use percentage from a `(total_kb, available_kb)` pair.
pub fn mem_used_pct(total_kb: u64, available_kb: u64) -> u8 {
    if total_kb == 0 {
        return 0;
    }
    let used = total_kb.saturating_sub(available_kb);
    ((used.saturating_mul(100) / total_kb).min(100)) as u8
}

/// One transport counter sample, summed across live peers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransportSample {
    /// Worst measured round-trip time, or zero when none was measured.
    pub rtt_ms: u32,
    /// Cumulative lost packets reported by the transport.
    pub packets_lost: i64,
    /// Cumulative payload bytes sent.
    pub bytes_sent: u64,
    /// Cumulative payload bytes received.
    pub bytes_received: u64,
}

/// Everything the sampler needs besides its own previous state.
#[derive(Debug, Clone, Copy, Default)]
pub struct SampleInput {
    pub transport: TransportSample,
    /// Calls this daemon is currently carrying audio for.
    pub active_calls: u8,
    /// Cumulative failures (decode, encrypt, send) observed by the daemon.
    pub failures: u64,
    /// Wall-clock age of this daemon, in seconds.
    pub uptime_secs: u64,
}

/// The capacity sampler. Owns the previous sample so it can turn cumulative
/// counters into rates and a loss percentage.
#[derive(Debug, Default)]
pub struct CapacitySampler {
    previous_cpu: Option<CpuSample>,
    /// When `previous_cpu` was read, so a too-short window is ignored.
    previous_cpu_at_ms: Option<u64>,
    previous_transport: Option<(u64, u64, u64)>,
    /// Whether the rate fields hold a real measured window rather than the
    /// idle floor, so the first measurement is taken as-is and later ones are
    /// smoothed.
    transport_sampled: bool,
    previous_lost: i64,
    previous_failures: u64,
    cpu_load_pct: u8,
    /// Whether [`CapacitySampler::cpu_load_pct`] holds a real sample rather
    /// than its zero initial value, so the first sample is not smoothed down
    /// from zero.
    cpu_sampled: bool,
    mem_used_pct: u8,
    upload_kbps: u32,
    download_kbps: u32,
    rtt_ms: u32,
    worst_rtt_ms: u32,
    loss_pct: u8,
}

/// Smooth one sample against the value already held.
///
/// Every daemon reads `/proc/stat` at a slightly different phase, and a loaded
/// host can differ by tens of percent between two one-second windows. Ranking
/// on the raw value makes the peer that happened to sample a quiet window the
/// winner on every tick, which pre-empts a healthy hub for no reason. A short
/// exponential average follows sustained load while ignoring a single noisy
/// window.
fn smooth(previous: u8, sample: u8) -> u8 {
    ((u16::from(previous) * 3 + u16::from(sample)) / 4) as u8
}

/// The same smoothing for a rate. A rate is floored at the idle capacity after
/// smoothing, so an idle peer still advertises the eligibility floor.
fn smooth_rate(previous: u32, sample: u32) -> u32 {
    ((u64::from(previous) * 3 + u64::from(sample)) / 4).min(u64::from(u32::MAX)) as u32
}

impl CapacitySampler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one sample into the advertised [`Priority`].
    pub fn sample(&mut self, now_ms: u64, input: SampleInput) -> Priority {
        let cpu = read_cpu();
        let memory = read_meminfo();
        self.sample_from(now_ms, input, cpu, memory)
    }

    /// The injectable half of [`sample`](Self::sample). Production passes the
    /// `/proc` reads; tests pass fixed samples so the arithmetic does not
    /// depend on the load of the machine running the suite.
    pub fn sample_from(
        &mut self,
        now_ms: u64,
        input: SampleInput,
        cpu: Option<CpuSample>,
        memory: Option<(u64, u64)>,
    ) -> Priority {
        let cpu_window_ms = self.previous_cpu_at_ms.map(|at| now_ms.saturating_sub(at));
        let cpu_window_long_enough =
            cpu_window_ms.is_none_or(|elapsed| elapsed >= MIN_SAMPLE_WINDOW_MS);
        if let Some(cpu) = cpu {
            if cpu_window_long_enough
                && let Some(previous) = self.previous_cpu
                && let Some(load) = cpu.load_pct(previous)
            {
                self.cpu_load_pct = if self.cpu_sampled {
                    smooth(self.cpu_load_pct, load)
                } else {
                    self.cpu_sampled = true;
                    load
                };
            }
            // A too-short window keeps both the value and the baseline, so
            // the next real window is measured from the earlier read.
            if cpu_window_long_enough {
                self.previous_cpu = Some(cpu);
                self.previous_cpu_at_ms = Some(now_ms);
            }
        }
        if let Some((total, available)) = memory {
            self.mem_used_pct = mem_used_pct(total, available);
        }

        let transport = input.transport;
        let (sent_delta, received_delta, elapsed_ms) = match self.previous_transport {
            Some((sent, received, at)) => (
                transport.bytes_sent.saturating_sub(sent),
                transport.bytes_received.saturating_sub(received),
                now_ms.saturating_sub(at).max(1),
            ),
            None => (0, 0, 1),
        };
        let transport_window_long_enough =
            self.previous_transport.is_none() || elapsed_ms >= MIN_SAMPLE_WINDOW_MS;
        if self.previous_transport.is_some() && transport_window_long_enough {
            let upload = kbps(sent_delta, elapsed_ms).max(IDLE_UPLOAD_KBPS);
            let download = kbps(received_delta, elapsed_ms).max(IDLE_DOWNLOAD_KBPS);
            if self.transport_sampled {
                self.upload_kbps = smooth_rate(self.upload_kbps, upload).max(IDLE_UPLOAD_KBPS);
                self.download_kbps =
                    smooth_rate(self.download_kbps, download).max(IDLE_DOWNLOAD_KBPS);
            } else {
                // The first whole window is a real measurement, not a
                // phase-noisy continuation, so do not drag it down from the
                // idle floor.
                self.transport_sampled = true;
                self.upload_kbps = upload;
                self.download_kbps = download;
            }
        } else if self.previous_transport.is_none() {
            self.upload_kbps = IDLE_UPLOAD_KBPS;
            self.download_kbps = IDLE_DOWNLOAD_KBPS;
        }

        if transport_window_long_enough {
            let lost_delta = transport
                .packets_lost
                .saturating_sub(self.previous_lost)
                .max(0) as u64;
            // The exact received packet count is not exposed by the
            // transport, so the loss percentage is estimated from the byte
            // delta. It is a health signal, never a billing or routing input.
            let estimated_packets = received_delta / AVERAGE_OPUS_BYTES;
            let sample = if lost_delta > 0 {
                let total = lost_delta.saturating_add(estimated_packets).max(1);
                (lost_delta.saturating_mul(100) / total).min(100) as u8
            } else {
                0
            };
            self.loss_pct = smooth(self.loss_pct, sample);
            self.previous_transport =
                Some((transport.bytes_sent, transport.bytes_received, now_ms));
            self.previous_lost = transport.packets_lost;
        }

        self.rtt_ms = transport.rtt_ms;
        self.worst_rtt_ms = transport.rtt_ms;
        let failures = input
            .failures
            .saturating_sub(self.previous_failures)
            .min(u8::MAX as u64) as u8;
        self.previous_failures = input.failures;

        Priority {
            trust_tier: 1,
            reachability: 2,
            rtt_ms: self.rtt_ms,
            upload_kbps: self.upload_kbps,
            battery_pct: read_battery_pct().unwrap_or(100),
            uptime_secs: input.uptime_secs,
            download_kbps: self.download_kbps,
            cpu_load_pct: self.cpu_load_pct,
            mem_used_pct: self.mem_used_pct,
            recent_failures: failures,
            cores: std::thread::available_parallelism()
                .map(|cores| cores.get().min(u8::MAX as usize) as u8)
                .unwrap_or(4),
            hardware_encode: false,
            hardware_decode: false,
            active_calls: input.active_calls,
            hub_cooldown_ms: 0,
            worst_rtt_ms: self.worst_rtt_ms,
            loss_pct: self.loss_pct,
            unreachable_members: 0,
            member_count: 1,
        }
    }
}

fn kbps(bytes: u64, elapsed_ms: u64) -> u32 {
    let bits = bytes.saturating_mul(8);
    (bits / elapsed_ms).min(u32::MAX as u64) as u32
}

/// Read the aggregate CPU line, or `None` off Linux.
pub fn read_cpu() -> Option<CpuSample> {
    std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|text| CpuSample::parse(&text))
}

/// Read memory totals, or `None` off Linux.
pub fn read_meminfo() -> Option<(u64, u64)> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| parse_meminfo(&text))
}

/// The lowest battery percentage reported by any power supply, if the host
/// exposes one. Mains-powered hosts report `None`, which the sampler maps to
/// 100.
pub fn read_battery_pct() -> Option<u8> {
    let entries = std::fs::read_dir("/sys/class/power_supply").ok()?;
    let mut lowest = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(text) = std::fs::read_to_string(path.join("capacity"))
            && let Ok(value) = text.trim().parse::<u8>()
        {
            lowest = Some(lowest.map_or(value, |current: u8| current.min(value)));
        }
    }
    lowest
}

/// A process-wide counter for failures the election should see. The daemon
/// increments it from the media paths and the sampler turns it into a rate.
#[derive(Debug, Default)]
pub struct FailureCounter {
    total: AtomicU64,
}

impl FailureCounter {
    pub fn record(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn record_many(&self, count: u64) {
        if count > 0 {
            self.total.fetch_add(count, Ordering::Relaxed);
        }
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_load_uses_the_delta_not_the_cumulative_counters() {
        let first = CpuSample {
            total: 1_000,
            idle: 800,
        };
        let second = CpuSample {
            total: 1_100,
            idle: 825,
        };
        // 100 ticks elapsed, 75 of them busy.
        assert_eq!(second.load_pct(first), Some(75));
        assert_eq!(first.load_pct(first), None, "no delta is no sample");
        assert_eq!(first.load_pct(second), None, "time does not run backwards");
    }

    #[test]
    fn cpu_parse_reads_the_aggregate_line_only() {
        let stat = "cpu  100 20 30 500 10 0 0 0 0 0\ncpu0 1 2 3 4\n";
        assert_eq!(
            CpuSample::parse(stat),
            Some(CpuSample {
                total: 660,
                idle: 510,
            })
        );
        assert_eq!(CpuSample::parse("intr 1 2 3\n"), None);
    }

    #[test]
    fn memory_use_is_derived_from_available_not_free() {
        assert_eq!(
            parse_meminfo("MemTotal: 8000 kB\nMemFree: 100 kB\nMemAvailable: 2000 kB\n"),
            Some((8_000, 2_000))
        );
        assert_eq!(mem_used_pct(8_000, 2_000), 75);
        assert_eq!(mem_used_pct(0, 0), 0);
    }

    #[test]
    fn bandwidth_and_loss_come_from_transport_deltas() {
        let mut sampler = CapacitySampler::new();
        let first = sampler.sample_from(
            1_000,
            SampleInput {
                transport: TransportSample {
                    bytes_sent: 0,
                    bytes_received: 0,
                    ..TransportSample::default()
                },
                ..SampleInput::default()
            },
            None,
            None,
        );
        // The first sample only establishes the baseline, and still meets the
        // eligibility floors so an idle daemon stays a candidate.
        assert_eq!(first.upload_kbps, IDLE_UPLOAD_KBPS);

        // 1 s later 200 kB was sent and 400 kB received: 1600 kbps up and
        // 3200 kbps down. One packet was lost out of roughly 4444.
        let second = sampler.sample_from(
            2_000,
            SampleInput {
                transport: TransportSample {
                    rtt_ms: 42,
                    packets_lost: 1,
                    bytes_sent: 200_000,
                    bytes_received: 400_000,
                },
                active_calls: 1,
                failures: 2,
                uptime_secs: 300,
            },
            None,
            None,
        );
        assert_eq!(second.upload_kbps, 1_600);
        assert_eq!(second.download_kbps, 3_200);
        assert_eq!(second.rtt_ms, 42);
        assert_eq!(second.worst_rtt_ms, 42);
        assert_eq!(second.loss_pct, 0, "one lost packet is below one percent");
        assert_eq!(second.active_calls, 1);
        assert_eq!(second.recent_failures, 2);
        assert_eq!(second.uptime_secs, 300);
    }

    #[test]
    fn a_sustained_failure_and_load_sample_changes_health_and_rank() {
        let mut sampler = CapacitySampler::new();
        let first = CpuSample {
            total: 1_000,
            idle: 100,
        };
        let second = CpuSample {
            total: 2_000,
            idle: 140,
        };
        // Establish the CPU baseline, then a delta that is 96% busy.
        sampler.sample_from(1_000, SampleInput::default(), Some(first), None);
        let sample = sampler.sample_from(
            2_000,
            SampleInput {
                transport: TransportSample {
                    rtt_ms: 400,
                    packets_lost: 900,
                    bytes_sent: 10,
                    bytes_received: 10,
                },
                active_calls: 3,
                failures: 9,
                uptime_secs: 10,
            },
            Some(second),
            Some((10_000, 300)),
        );
        let config = ovvc_control::ElectionConfig::default();
        assert!(
            !sample.is_healthy(&config),
            "a loaded, lossy, failing peer must not keep the hub role: {sample:?}"
        );
        let healthy = Priority::local();
        assert!(
            Priority::local().is_healthy(&config),
            "the baseline placeholder is expected to be healthy"
        );
        assert!(
            sample < healthy,
            "the unhealthy sample must lose the ranking"
        );
    }

    #[test]
    fn failure_counter_is_monotonic() {
        let counter = FailureCounter::default();
        counter.record();
        counter.record_many(3);
        assert_eq!(counter.total(), 4);
        counter.record_many(0);
        assert_eq!(counter.total(), 4);
    }
}
