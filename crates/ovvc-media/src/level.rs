//! Frame level measurement and hysteresis-based speaking detection.

/// A frame at or above this level starts speaking.
pub const SPEAKING_ON_DB: f32 = -35.0;

/// A frame below this level counts towards ending speaking.
pub const SPEAKING_OFF_DB: f32 = -45.0;

/// How many consecutive frames below [`SPEAKING_OFF_DB`] end speaking.
pub const SPEAKING_HOLD_FRAMES: u16 = 10;

/// The floor reported for digital silence.
const SILENCE_DB: f32 = -120.0;

/// Root-mean-square level of a frame of signed 16-bit samples, in dBFS.
///
/// Silence and an empty frame both report [`SILENCE_DB`], never negative
/// infinity, so the value is safe to compare and serialise.
pub fn frame_rms_db(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return SILENCE_DB;
    }

    let sum: f64 = samples
        .iter()
        .map(|&sample| {
            let value = f64::from(sample);
            value * value
        })
        .sum();
    let mean_square = sum / samples.len() as f64;
    if mean_square <= 0.0 {
        return SILENCE_DB;
    }

    let db = 10.0 * (mean_square / (32_768.0 * 32_768.0)).log10();
    if db.is_finite() {
        (db as f32).max(SILENCE_DB)
    } else {
        SILENCE_DB
    }
}

/// Turns per-frame levels into a speaking flag with hysteresis.
///
/// Crossing [`SPEAKING_ON_DB`] starts speaking immediately; falling below
/// [`SPEAKING_OFF_DB`] only ends it after [`SPEAKING_HOLD_FRAMES`] consecutive
/// frames. Levels in between leave the current state alone, which keeps a
/// quiet word from flickering the indicator on every frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpeakingDetector {
    speaking: bool,
    frames_below: u16,
}

impl SpeakingDetector {
    pub const fn new() -> Self {
        Self {
            speaking: false,
            frames_below: 0,
        }
    }

    pub fn push(&mut self, samples: &[i16]) -> bool {
        self.push_db(frame_rms_db(samples))
    }

    pub fn push_db(&mut self, db: f32) -> bool {
        if db >= SPEAKING_ON_DB {
            self.speaking = true;
            self.frames_below = 0;
        } else if db < SPEAKING_OFF_DB {
            if self.speaking {
                self.frames_below = self.frames_below.saturating_add(1);
                if self.frames_below >= SPEAKING_HOLD_FRAMES {
                    self.speaking = false;
                    self.frames_below = 0;
                }
            } else {
                self.frames_below = 0;
            }
        } else {
            // Between the thresholds: hold the current state and do not let
            // an intermediate frame count towards the hold-down.
            self.frames_below = 0;
        }
        self.speaking
    }

    pub fn is_speaking(&self) -> bool {
        self.speaking
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_is_not_speaking() {
        let mut detector = SpeakingDetector::new();
        assert!(!detector.push(&[0i16; 160]));
        assert!(!detector.is_speaking());
        assert_eq!(frame_rms_db(&[0i16; 160]), SILENCE_DB);
        assert_eq!(frame_rms_db(&[]), SILENCE_DB);
    }

    #[test]
    fn a_loud_frame_starts_speaking_immediately() {
        let mut detector = SpeakingDetector::new();
        assert!(detector.push(&[20_000i16; 160]));
        assert!(detector.is_speaking());
        assert!(frame_rms_db(&[20_000i16; 160]) > SPEAKING_ON_DB);
    }

    #[test]
    fn below_off_holds_before_ending_speaking() {
        let mut detector = SpeakingDetector::new();
        detector.push_db(SPEAKING_ON_DB);
        assert!(detector.is_speaking());

        for _ in 1..SPEAKING_HOLD_FRAMES {
            assert!(detector.push_db(-80.0));
        }
        assert!(detector.is_speaking());

        assert!(!detector.push_db(-80.0));
        assert!(!detector.is_speaking());
    }

    #[test]
    fn intermediate_levels_keep_the_current_state() {
        let mut detector = SpeakingDetector::new();

        // Silent state stays silent at an intermediate level.
        assert!(!detector.push_db(-40.0));

        detector.push_db(SPEAKING_ON_DB);
        assert!(detector.is_speaking());
        assert!(detector.push_db(-40.0));
        assert!(detector.is_speaking());

        // An intermediate frame also resets the below-off run.
        for _ in 0..SPEAKING_HOLD_FRAMES - 1 {
            detector.push_db(-80.0);
        }
        assert!(detector.push_db(-40.0));
        assert!(detector.is_speaking());
        for _ in 0..SPEAKING_HOLD_FRAMES - 1 {
            assert!(detector.push_db(-80.0));
        }
        assert!(!detector.push_db(-80.0));
    }

    #[test]
    fn threshold_boundaries_behave_as_documented() {
        let mut detector = SpeakingDetector::new();
        assert!(detector.push_db(SPEAKING_ON_DB));
        // Exactly the off threshold is intermediate, not below it.
        assert!(detector.push_db(SPEAKING_OFF_DB));
        for _ in 0..SPEAKING_HOLD_FRAMES - 1 {
            assert!(detector.push_db(SPEAKING_OFF_DB - 1.0));
        }
        assert!(!detector.push_db(SPEAKING_OFF_DB - 1.0));
        assert!(!detector.is_speaking());
    }
}
