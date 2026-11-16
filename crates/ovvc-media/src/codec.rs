//! Voice profiles and profile-set codec negotiation.
//!
//! A voice channel does not pick a codec centrally. Every member advertises
//! the profiles it can encode and decode, the sets are intersected, and the
//! highest-quality profile left in the intersection wins. That keeps the
//! decision deterministic and transport-free: each peer can recompute the same
//! answer from the same roster.

use serde::{Deserialize, Serialize};

use crate::error::{MediaError, Result};

/// A quality tier of the voice codec. The ordering is by quality, so
/// `Minimal < Low < Standard`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
pub enum VoiceProfile {
    Minimal,
    Low,
    #[default]
    Standard,
}

impl VoiceProfile {
    /// Every profile, best first.
    pub const ALL: [VoiceProfile; 3] = [
        VoiceProfile::Standard,
        VoiceProfile::Low,
        VoiceProfile::Minimal,
    ];

    /// The profile's wire name.
    pub const fn name(self) -> &'static str {
        match self {
            VoiceProfile::Standard => "voice-standard",
            VoiceProfile::Low => "voice-low",
            VoiceProfile::Minimal => "voice-minimal",
        }
    }

    /// The codec family this profile uses.
    pub const fn codec(self) -> &'static str {
        match self {
            VoiceProfile::Standard | VoiceProfile::Low => "opus",
            VoiceProfile::Minimal => "codec2",
        }
    }

    pub const fn bitrate_kbps(self) -> u16 {
        match self {
            VoiceProfile::Standard => 10,
            VoiceProfile::Low => 6,
            VoiceProfile::Minimal => 2,
        }
    }

    pub const fn frame_ms(self) -> u16 {
        match self {
            VoiceProfile::Standard | VoiceProfile::Low => 20,
            VoiceProfile::Minimal => 40,
        }
    }

    pub const fn sample_rate(self) -> u32 {
        match self {
            VoiceProfile::Standard => 48_000,
            VoiceProfile::Low | VoiceProfile::Minimal => 8_000,
        }
    }

    /// Minimal exists for links too weak for continuous audio, so it is
    /// push-to-talk only.
    pub const fn ptt_only(self) -> bool {
        matches!(self, VoiceProfile::Minimal)
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "voice-standard" => Some(VoiceProfile::Standard),
            "voice-low" => Some(VoiceProfile::Low),
            "voice-minimal" => Some(VoiceProfile::Minimal),
            _ => None,
        }
    }

    /// The profile's bit in [`ProfileSet`].
    pub const fn bit(self) -> u8 {
        match self {
            VoiceProfile::Minimal => 1,
            VoiceProfile::Low => 2,
            VoiceProfile::Standard => 4,
        }
    }
}

/// A set of supported [`VoiceProfile`]s, encoded as a bit field so it can be
/// carried in a control frame without allocating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct ProfileSet(pub u8);

impl ProfileSet {
    pub const NONE: ProfileSet = ProfileSet(0);
    pub const STANDARD: ProfileSet = ProfileSet(VoiceProfile::Standard.bit());
    pub const ALL: ProfileSet = ProfileSet(
        VoiceProfile::Minimal.bit() | VoiceProfile::Low.bit() | VoiceProfile::Standard.bit(),
    );

    pub const fn contains(self, profile: VoiceProfile) -> bool {
        self.0 & profile.bit() != 0
    }

    pub fn insert(&mut self, profile: VoiceProfile) {
        self.0 |= profile.bit();
    }

    pub const fn union(self, other: Self) -> Self {
        ProfileSet(self.0 | other.0)
    }

    pub const fn intersection(self, other: Self) -> Self {
        ProfileSet(self.0 & other.0)
    }

    /// The highest-quality profile contained in the set.
    pub fn preferred(self) -> Option<VoiceProfile> {
        VoiceProfile::ALL
            .into_iter()
            .find(|profile| self.contains(*profile))
    }

    /// Iterate the contained profiles, best first.
    pub fn profiles(self) -> impl Iterator<Item = VoiceProfile> {
        VoiceProfile::ALL
            .into_iter()
            .filter(move |profile| self.contains(*profile))
    }
}

impl From<VoiceProfile> for ProfileSet {
    fn from(profile: VoiceProfile) -> Self {
        ProfileSet(profile.bit())
    }
}

/// The codec decision a call converges on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedCodec {
    pub profile: VoiceProfile,
    pub audio: String,
    pub bitrate_kbps: u16,
    pub frame_ms: u16,
    pub sample_rate: u32,
    pub ptt_only: bool,
    pub video: Option<String>,
}

/// Intersect every member's profile set and pick the highest-quality common
/// profile.
///
/// An empty iterator means there are no members to negotiate for; that is
/// reported as [`MediaError::NoCommonCodec`] rather than inventing a profile.
/// Video is negotiated as a single yes/no capability: when any member asks for
/// it, the call uses VP8.
pub fn negotiate(
    profiles: impl IntoIterator<Item = ProfileSet>,
    video: bool,
) -> Result<NegotiatedCodec> {
    let mut common: Option<ProfileSet> = None;
    for set in profiles {
        common = Some(match common {
            Some(current) => current.intersection(set),
            None => set,
        });
    }

    let profile = common
        .and_then(ProfileSet::preferred)
        .ok_or(MediaError::NoCommonCodec)?;

    Ok(NegotiatedCodec {
        profile,
        audio: profile.codec().to_string(),
        bitrate_kbps: profile.bitrate_kbps(),
        frame_ms: profile.frame_ms(),
        sample_rate: profile.sample_rate(),
        ptt_only: profile.ptt_only(),
        video: if video { Some("vp8".to_string()) } else { None },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_prefers_standard() {
        assert_eq!(ProfileSet::ALL.preferred(), Some(VoiceProfile::Standard));
        assert_eq!(ProfileSet::NONE.preferred(), None);
    }

    #[test]
    fn intersection_narrows_to_lowest_common_quality() {
        let a = ProfileSet::from(VoiceProfile::Low).union(ProfileSet::from(VoiceProfile::Minimal));
        let b = ProfileSet::from(VoiceProfile::Minimal);
        let common = a.intersection(b);
        assert_eq!(common, ProfileSet::from(VoiceProfile::Minimal));
        assert_eq!(common.preferred(), Some(VoiceProfile::Minimal));
    }

    #[test]
    fn profile_bits_survive_a_round_trip() {
        let mut set = ProfileSet::NONE;
        set.insert(VoiceProfile::Low);
        set.insert(VoiceProfile::Standard);
        assert!(set.contains(VoiceProfile::Low));
        assert!(set.contains(VoiceProfile::Standard));
        assert!(!set.contains(VoiceProfile::Minimal));
        assert_eq!(
            set,
            ProfileSet(VoiceProfile::Low.bit() | VoiceProfile::Standard.bit())
        );
        assert_eq!(
            set.profiles().collect::<Vec<_>>(),
            vec![VoiceProfile::Standard, VoiceProfile::Low]
        );
    }

    #[test]
    fn names_and_attributes_match_the_contract() {
        assert_eq!(VoiceProfile::Standard.name(), "voice-standard");
        assert_eq!(VoiceProfile::Low.name(), "voice-low");
        assert_eq!(VoiceProfile::Minimal.name(), "voice-minimal");
        assert_eq!(VoiceProfile::Standard.codec(), "opus");
        assert_eq!(VoiceProfile::Low.codec(), "opus");
        assert_eq!(VoiceProfile::Minimal.codec(), "codec2");
        assert_eq!(VoiceProfile::Standard.bitrate_kbps(), 10);
        assert_eq!(VoiceProfile::Low.bitrate_kbps(), 6);
        assert_eq!(VoiceProfile::Minimal.bitrate_kbps(), 2);
        assert_eq!(VoiceProfile::Standard.frame_ms(), 20);
        assert_eq!(VoiceProfile::Low.frame_ms(), 20);
        assert_eq!(VoiceProfile::Minimal.frame_ms(), 40);
        assert_eq!(VoiceProfile::Standard.sample_rate(), 48_000);
        assert_eq!(VoiceProfile::Minimal.sample_rate(), 8_000);
        assert!(VoiceProfile::Minimal.ptt_only());
        assert!(!VoiceProfile::Low.ptt_only());
        assert!(!VoiceProfile::Standard.ptt_only());
        assert_eq!(
            VoiceProfile::from_name("voice-low"),
            Some(VoiceProfile::Low)
        );
        assert_eq!(VoiceProfile::from_name("nope"), None);
    }

    #[test]
    fn negotiation_returns_the_common_profile() {
        let codec = negotiate(
            [ProfileSet::ALL, ProfileSet::from(VoiceProfile::Low)],
            false,
        )
        .expect("low is common");
        assert_eq!(codec.profile, VoiceProfile::Low);
        assert_eq!(codec.audio, "opus");
        assert_eq!(codec.video, None);
    }

    #[test]
    fn negotiation_reports_no_common_codec_for_disjoint_sets() {
        let result = negotiate(
            [
                ProfileSet::STANDARD,
                ProfileSet::from(VoiceProfile::Minimal),
            ],
            false,
        );
        assert!(matches!(result, Err(MediaError::NoCommonCodec)));
    }

    #[test]
    fn negotiation_reports_no_common_codec_for_no_members() {
        let empty: [ProfileSet; 0] = [];
        assert!(matches!(
            negotiate(empty, false),
            Err(MediaError::NoCommonCodec)
        ));
    }

    #[test]
    fn negotiation_carries_video_when_requested() {
        let codec = negotiate([ProfileSet::ALL], true).expect("standard is available");
        assert_eq!(codec.profile, VoiceProfile::Standard);
        assert_eq!(codec.video.as_deref(), Some("vp8"));
        assert!(!codec.ptt_only);
    }
}
