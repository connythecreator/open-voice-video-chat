//! Signed guild and media policy.
//!
//! These are the durable, versioned choices a guild creator makes. The daemon
//! exposes them as a read model in v0.1, while the wire shape is fixed now, so
//! a policy written today still parses after enforcement lands and a client
//! cannot quietly invent an axis that the rest of the federation does not
//! understand.
//!
//! The design rule carried in [`GuildPolicy::with_client_floor`] is that
//! policy is a floor, not a ceiling. A
//! member's own minimum can raise an axis, never lower it, and a downgrade is
//! meant to be visible as read-only or leave rather than silent.

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

/// The version written into every new policy.
///
/// Version 2 splits the old single media bitrate cap into separate audio and
/// video caps, and adds the plugin allowlist anchors to [`GuildPolicy`]. A
/// version 1 policy is refused rather than silently reinterpreted.
pub const POLICY_VERSION: u16 = 2;

/// The largest threshold or erasure-coding set the format allows. Keeping the
/// caps small bounds the policy frame and the work a peer must do to validate
/// it.
pub const MAX_THRESHOLD_SHARES: u8 = 16;
pub const MAX_PARITY_SHARDS: u8 = 32;

/// A named starting point for a guild.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GuildProfile {
    #[default]
    Standard,
    Private,
    Paranoid,
    Custom,
}

impl GuildProfile {
    pub const ALL: [Self; 4] = [Self::Standard, Self::Private, Self::Paranoid, Self::Custom];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Private => "private",
            Self::Paranoid => "paranoid",
            Self::Custom => "custom",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|profile| profile.name() == name.trim().to_ascii_lowercase())
    }
}

/// How the guild decides who owns the log and who orders writes.
///
/// `P2PSharded` is the default and the long-term model. `ServerAuthoritative`
/// is a deliberately different trust model for guilds that want a traditional
/// server, and it must be chosen explicitly rather than arrived at by
/// accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GuildTopology {
    #[default]
    #[serde(rename = "p2p-sharded")]
    P2PSharded,
    #[serde(rename = "p2p-small")]
    P2PSmall,
    ServerAssisted,
    ServerAuthoritative,
}

impl GuildTopology {
    pub const fn name(self) -> &'static str {
        match self {
            Self::P2PSharded => "p2p-sharded",
            Self::P2PSmall => "p2p-small",
            Self::ServerAssisted => "server-assisted",
            Self::ServerAuthoritative => "server-authoritative",
        }
    }

    /// Whether this topology needs an opt-in server to function.
    pub const fn requires_server(self) -> bool {
        matches!(self, Self::ServerAssisted | Self::ServerAuthoritative)
    }

    /// Whether the server is allowed to be the ordering authority. Even here
    /// it must not own identity: signatures remain verifiable by every member.
    pub const fn server_owns_ordering(self) -> bool {
        matches!(self, Self::ServerAuthoritative)
    }
}

/// How peers find a guild.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Discovery {
    #[default]
    InviteOnly,
    PrivateRendezvous,
    PublicDht,
}

impl Discovery {
    const fn strictness(self) -> u8 {
        match self {
            Self::PublicDht => 0,
            Self::PrivateRendezvous => 1,
            Self::InviteOnly => 2,
        }
    }

    pub const fn stricter(self, other: Self) -> Self {
        if other.strictness() > self.strictness() {
            other
        } else {
            self
        }
    }
}

/// The transport floor for the durable plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TransportPolicy {
    #[default]
    Direct,
    RelayOnly,
    Tor,
    Mixnet,
}

impl TransportPolicy {
    const fn strictness(self) -> u8 {
        match self {
            Self::Direct => 0,
            Self::RelayOnly => 1,
            Self::Tor => 2,
            Self::Mixnet => 3,
        }
    }

    pub const fn stricter(self, other: Self) -> Self {
        if other.strictness() > self.strictness() {
            other
        } else {
            self
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::RelayOnly => "relay-only",
            Self::Tor => "tor",
            Self::Mixnet => "mixnet",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PaddingPolicy {
    #[default]
    Off,
    Buckets,
}

impl PaddingPolicy {
    const fn strictness(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Buckets => 1,
        }
    }

    pub const fn stricter(self, other: Self) -> Self {
        if other.strictness() > self.strictness() {
            other
        } else {
            self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IdentityPolicy {
    #[default]
    Named,
    Pseudonymous,
    AnonymousCredentials,
}

impl IdentityPolicy {
    const fn strictness(self) -> u8 {
        match self {
            Self::Named => 0,
            Self::Pseudonymous => 1,
            Self::AnonymousCredentials => 2,
        }
    }

    pub const fn stricter(self, other: Self) -> Self {
        if other.strictness() > self.strictness() {
            other
        } else {
            self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TimestampPolicy {
    #[default]
    Exact,
    Coarse,
}

impl TimestampPolicy {
    const fn strictness(self) -> u8 {
        match self {
            Self::Exact => 0,
            Self::Coarse => 1,
        }
    }

    pub const fn stricter(self, other: Self) -> Self {
        if other.strictness() > self.strictness() {
            other
        } else {
            self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum KeyCustody {
    #[default]
    SharedEpochKey,
    Threshold {
        k: u8,
        n: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum StoragePolicy {
    #[default]
    LocalReplicas,
    ShardedCold {
        k_data: u8,
        m: u8,
        allow_low_trust: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RetentionPolicy {
    #[default]
    Forever,
    EpochRotate,
    Ttl {
        secs: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum HardwareKeys {
    #[default]
    Off,
    Preferred,
    Required,
}

impl HardwareKeys {
    const fn strictness(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Preferred => 1,
            Self::Required => 2,
        }
    }

    pub const fn stricter(self, other: Self) -> Self {
        if other.strictness() > self.strictness() {
            other
        } else {
            self
        }
    }
}

/// Which third-party code a guild accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PluginPolicy {
    /// Only code built into the binary. The safe shipped default.
    #[default]
    BuiltInOnly,
    /// Plugins whose manifest carries an accepted signature.
    SignedAllowlist,
    /// Any plugin, including unsigned local builds. Never for strict guilds.
    Any,
}

impl PluginPolicy {
    const fn strictness(self) -> u8 {
        match self {
            Self::Any => 0,
            Self::SignedAllowlist => 1,
            Self::BuiltInOnly => 2,
        }
    }

    pub const fn stricter(self, other: Self) -> Self {
        if other.strictness() > self.strictness() {
            other
        } else {
            self
        }
    }
}

/// The full durable-plane policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuildPolicy {
    pub version: u16,
    pub profile: GuildProfile,
    pub topology: GuildTopology,
    pub discovery: Discovery,
    pub transport: TransportPolicy,
    pub padding: PaddingPolicy,
    pub author_identity: IdentityPolicy,
    pub timestamps: TimestampPolicy,
    pub key_custody: KeyCustody,
    pub storage: StoragePolicy,
    pub retention: RetentionPolicy,
    pub hardware_keys: HardwareKeys,
    pub plugins: PluginPolicy,
    /// The commune's media policy: call scope, hub mode and hub permissions.
    ///
    /// Signed policy events are still Phase 5, so every peer currently derives
    /// this from the shipped preset (the daemon may fill it from its local
    /// configuration until then). It lives inside [`GuildPolicy`] so the media
    /// plane and the durable plane describe one commune rather than two
    /// unrelated settings.
    pub media: MediaPolicy,
    /// Hash of the signed plugin allowlist document.
    ///
    /// Reserved for Phase 5, when [`PluginPolicy::SignedAllowlist`] starts
    /// being enforced. No signed allowlist document exists yet, so validation
    /// does not require a hash for `SignedAllowlist`; a guild can name the
    /// policy today and fill the anchor in when enforcement lands.
    pub plugin_allowlist_hash: Option<[u8; 32]>,
    /// Monotonic sequence of the allowlist this hash names.
    ///
    /// Also reserved for Phase 5. A sequence without a hash would be an anchor
    /// pointing at nothing, so it is rejected.
    pub plugin_allowlist_sequence: u64,
}

impl Default for GuildPolicy {
    /// The guild policy read model reports the standard preset until policy
    /// events land, so a guild that has not published a policy still shows the
    /// shipped defaults rather than no policy at all.
    fn default() -> Self {
        Self::standard()
    }
}

impl GuildPolicy {
    /// Fastest, and the only preset that exposes peer IP addresses.
    pub fn standard() -> Self {
        Self {
            version: POLICY_VERSION,
            profile: GuildProfile::Standard,
            topology: GuildTopology::P2PSharded,
            discovery: Discovery::InviteOnly,
            transport: TransportPolicy::Direct,
            padding: PaddingPolicy::Off,
            author_identity: IdentityPolicy::Named,
            timestamps: TimestampPolicy::Exact,
            key_custody: KeyCustody::SharedEpochKey,
            storage: StoragePolicy::LocalReplicas,
            retention: RetentionPolicy::Forever,
            hardware_keys: HardwareKeys::Preferred,
            plugins: PluginPolicy::BuiltInOnly,
            media: MediaPolicy::standard(),
            plugin_allowlist_hash: None,
            plugin_allowlist_sequence: 0,
        }
    }

    /// Relayed traffic, no clearnet dialling, encrypted cold shards.
    pub fn private() -> Self {
        Self {
            version: POLICY_VERSION,
            profile: GuildProfile::Private,
            topology: GuildTopology::P2PSharded,
            discovery: Discovery::InviteOnly,
            transport: TransportPolicy::RelayOnly,
            padding: PaddingPolicy::Buckets,
            author_identity: IdentityPolicy::Named,
            timestamps: TimestampPolicy::Exact,
            key_custody: KeyCustody::SharedEpochKey,
            storage: StoragePolicy::ShardedCold {
                k_data: 3,
                m: 2,
                allow_low_trust: false,
            },
            retention: RetentionPolicy::EpochRotate,
            hardware_keys: HardwareKeys::Preferred,
            plugins: PluginPolicy::SignedAllowlist,
            media: MediaPolicy::private(),
            // The anchor is filled in by Phase 5 enforcement; no signed
            // allowlist document exists yet.
            plugin_allowlist_hash: None,
            plugin_allowlist_sequence: 0,
        }
    }

    /// Tor or a mixnet only, threshold custody, no third-party code.
    pub fn paranoid() -> Self {
        Self {
            version: POLICY_VERSION,
            profile: GuildProfile::Paranoid,
            topology: GuildTopology::P2PSharded,
            discovery: Discovery::InviteOnly,
            transport: TransportPolicy::Tor,
            padding: PaddingPolicy::Buckets,
            author_identity: IdentityPolicy::Pseudonymous,
            timestamps: TimestampPolicy::Coarse,
            key_custody: KeyCustody::Threshold { k: 3, n: 5 },
            storage: StoragePolicy::ShardedCold {
                k_data: 4,
                m: 2,
                allow_low_trust: false,
            },
            retention: RetentionPolicy::EpochRotate,
            hardware_keys: HardwareKeys::Required,
            plugins: PluginPolicy::BuiltInOnly,
            media: MediaPolicy::paranoid(),
            plugin_allowlist_hash: None,
            plugin_allowlist_sequence: 0,
        }
    }

    /// A copy of `standard` with the profile marker changed, ready for the
    /// caller to adjust individual axes.
    pub fn custom() -> Self {
        let mut policy = Self::standard();
        policy.profile = GuildProfile::Custom;
        policy
    }

    pub fn for_profile(profile: GuildProfile) -> Self {
        match profile {
            GuildProfile::Standard => Self::standard(),
            GuildProfile::Private => Self::private(),
            GuildProfile::Paranoid => Self::paranoid(),
            GuildProfile::Custom => Self::custom(),
        }
    }

    /// Raise each axis to the stricter of the guild policy and the member's
    /// own minimum. The result can never be weaker than `self`.
    pub fn with_client_floor(&self, floor: &ClientPolicyFloor) -> Self {
        let mut policy = self.clone();
        if let Some(value) = floor.discovery {
            policy.discovery = policy.discovery.stricter(value);
        }
        if let Some(value) = floor.transport {
            policy.transport = policy.transport.stricter(value);
        }
        if let Some(value) = floor.padding {
            policy.padding = policy.padding.stricter(value);
        }
        if let Some(value) = floor.author_identity {
            policy.author_identity = policy.author_identity.stricter(value);
        }
        if let Some(value) = floor.timestamps {
            policy.timestamps = policy.timestamps.stricter(value);
        }
        if let Some(value) = floor.hardware_keys {
            policy.hardware_keys = policy.hardware_keys.stricter(value);
        }
        if let Some(value) = floor.plugins {
            policy.plugins = policy.plugins.stricter(value);
        }
        policy
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != POLICY_VERSION {
            return Err(CoreError::InvalidInput(format!(
                "unsupported guild policy version {}",
                self.version
            )));
        }
        // The allowlist anchors are reserved for Phase 5. A sequence without a
        // hash would point at no document, so reject that combination now even
        // though enforcement is not implemented yet.
        if self.plugin_allowlist_sequence > 0 && self.plugin_allowlist_hash.is_none() {
            return Err(CoreError::InvalidInput(
                "a plugin allowlist sequence requires a plugin allowlist hash".to_string(),
            ));
        }
        match self.key_custody {
            KeyCustody::Threshold { k, n } => {
                if k == 0 || n == 0 || k > n || n > MAX_THRESHOLD_SHARES {
                    return Err(CoreError::InvalidInput(format!(
                        "threshold custody needs 1 <= k <= n <= {MAX_THRESHOLD_SHARES}; got k={k}, n={n}"
                    )));
                }
            }
            KeyCustody::SharedEpochKey => {}
        }
        if let StoragePolicy::ShardedCold { k_data, m, .. } = self.storage
            && (k_data == 0 || m == 0 || k_data > MAX_THRESHOLD_SHARES || m > MAX_PARITY_SHARDS)
        {
            return Err(CoreError::InvalidInput(format!(
                "cold storage needs 1 <= k_data <= {MAX_THRESHOLD_SHARES} and 1 <= m <= {MAX_PARITY_SHARDS}; got k_data={k_data}, m={m}"
            )));
        }
        if let RetentionPolicy::Ttl { secs } = self.retention
            && secs == 0
        {
            return Err(CoreError::InvalidInput(
                "a TTL retention policy must be longer than zero seconds".to_string(),
            ));
        }
        self.media.validate()?;
        Ok(())
    }
}

/// The minimum a member applies on top of any guild policy.
///
/// `None` means "no opinion". This is deliberately a separate type from
/// [`GuildPolicy`]: a member has no say over key custody or shard placement,
/// but they can insist on Tor, better discovery or fewer plugins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientPolicyFloor {
    pub discovery: Option<Discovery>,
    pub transport: Option<TransportPolicy>,
    pub padding: Option<PaddingPolicy>,
    pub author_identity: Option<IdentityPolicy>,
    pub timestamps: Option<TimestampPolicy>,
    pub hardware_keys: Option<HardwareKeys>,
    pub plugins: Option<PluginPolicy>,
}

/// What topology a commune's channel calls use.
///
/// A two-member channel call is always a direct media pair, because one
/// connection is already the whole call. The election may still name a
/// coordinator today, but the hub role is unused and `VoiceSnapshot.hub` is
/// `None`. A dedicated one-to-one call type, when it exists, should bypass the
/// election entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CallScope {
    /// Direct pair at two members, mesh while small, then a star through the
    /// elected hub. This is the shipped default because the media forwarding
    /// path is newer than the all-pairs path.
    #[default]
    Auto,
    /// Every member connects to every other member. This is the paranoid mesh:
    /// no hub and no plaintext at any other peer.
    Mesh,
    /// A star through the hub as soon as the call has more than two members.
    Star,
}

impl CallScope {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Mesh => "mesh",
            Self::Star => "star",
        }
    }
}

/// Who runs the media hub for a channel call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum HubMode {
    /// The call members elect a hub and a backup.
    #[default]
    Elected,
    /// A commune server is the hub because its capacity and reachability are
    /// known, and the members elect a backup among themselves.
    Server,
    /// No hub. Only valid with [`CallScope::Mesh`].
    None,
}

impl HubMode {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Elected => "elected",
            Self::Server => "server",
            Self::None => "none",
        }
    }
}

/// What the hub is allowed to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum HubTrust {
    /// The hub decrypts, mixes and re-encrypts. It is inside the call's trust
    /// boundary, and every frontend must say so.
    #[default]
    Trusted,
    /// The hub forwards ciphertext it cannot read. This rules out mixing and
    /// is the pairwise-forwarding star a paranoid commune can choose.
    CiphertextOnly,
}

impl HubTrust {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::CiphertextOnly => "ciphertext-only",
        }
    }
}

/// The commune's hub choice and the permissions that go with it.
///
/// This is the policy the UI shows beside the elected hub: whether it may
/// decrypt, mix audio and forward video, and the minimum trust tier a
/// candidate must advertise to be eligible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubPolicy {
    pub mode: HubMode,
    pub trust: HubTrust,
    /// The hub may decrypt and mix audio per listener.
    pub allow_mixing: bool,
    /// The hub may selectively forward video.
    pub allow_video_forwarding: bool,
    /// The minimum `Priority::trust_tier` an elected hub must advertise.
    pub min_trust_tier: u8,
}

impl HubPolicy {
    /// The shipped choice: elect a trusted hub that may decrypt and mix.
    pub const fn elected() -> Self {
        Self {
            mode: HubMode::Elected,
            trust: HubTrust::Trusted,
            allow_mixing: true,
            allow_video_forwarding: true,
            min_trust_tier: 0,
        }
    }

    /// A server-assisted commune uses the server as the hub.
    pub const fn server() -> Self {
        Self {
            mode: HubMode::Server,
            ..Self::elected()
        }
    }

    /// A star whose hub forwards ciphertext and cannot read the media.
    pub const fn pairwise() -> Self {
        Self {
            mode: HubMode::Elected,
            trust: HubTrust::CiphertextOnly,
            allow_mixing: false,
            allow_video_forwarding: true,
            min_trust_tier: 0,
        }
    }

    /// The paranoid mesh has no hub at all.
    pub const fn none() -> Self {
        Self {
            mode: HubMode::None,
            trust: HubTrust::CiphertextOnly,
            allow_mixing: false,
            allow_video_forwarding: false,
            min_trust_tier: 0,
        }
    }

    /// Whether the hub is inside the call's trust boundary.
    pub const fn can_decrypt(self) -> bool {
        matches!(self.trust, HubTrust::Trusted)
    }

    /// Whether the hub may mix audio rather than only forward it.
    pub const fn can_mix(self) -> bool {
        self.can_decrypt() && self.allow_mixing
    }
}

impl Default for HubPolicy {
    fn default() -> Self {
        Self::elected()
    }
}

/// Media sessions are not stored, so they do not inherit cold storage or
/// threshold keys, but they still carry a transport, a call topology and a hub
/// choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaPolicy {
    pub version: u16,
    pub e2ee: bool,
    pub transport: TransportPolicy,
    /// Legacy topology field kept so an older configuration file still loads.
    /// New policy reads [`MediaPolicy::call_scope`] and [`MediaPolicy::hub`].
    pub topology: MediaTopology,
    pub codecs: Vec<String>,
    /// Cap for audio streams. Video has its own cap because the two have very
    /// different useful ranges.
    pub max_audio_bitrate_kbps: u32,
    /// Cap for video streams. Zero means video is disabled.
    pub max_video_bitrate_kbps: u32,
    pub allow_video_over_tor: bool,
    /// The signalling topology and the size at which a star starts.
    pub call_scope: CallScope,
    /// Who runs the hub and what it may do with the media.
    pub hub: HubPolicy,
}

/// Codecs that carry video rather than audio.
fn is_video_codec(codec: &str) -> bool {
    matches!(codec, "vp8" | "vp9" | "av1" | "h264")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MediaTopology {
    #[default]
    Mesh,
    Star,
    Sfu,
}

impl Default for MediaPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

impl MediaPolicy {
    pub fn standard() -> Self {
        Self {
            version: POLICY_VERSION,
            e2ee: true,
            transport: TransportPolicy::Direct,
            topology: MediaTopology::Mesh,
            codecs: vec![
                "opus".to_string(),
                "vp9".to_string(),
                "vp8".to_string(),
                "h264".to_string(),
            ],
            max_audio_bitrate_kbps: 512,
            max_video_bitrate_kbps: 4000,
            allow_video_over_tor: false,
            call_scope: CallScope::Auto,
            hub: HubPolicy::elected(),
        }
    }

    pub fn private() -> Self {
        Self {
            version: POLICY_VERSION,
            e2ee: true,
            transport: TransportPolicy::RelayOnly,
            topology: MediaTopology::Mesh,
            codecs: vec!["opus".to_string()],
            max_audio_bitrate_kbps: 128,
            max_video_bitrate_kbps: 0,
            allow_video_over_tor: false,
            call_scope: CallScope::Auto,
            hub: HubPolicy::elected(),
        }
    }

    pub fn paranoid() -> Self {
        Self {
            version: POLICY_VERSION,
            e2ee: true,
            transport: TransportPolicy::Tor,
            topology: MediaTopology::Mesh,
            codecs: vec!["opus".to_string(), "codec2".to_string()],
            max_audio_bitrate_kbps: 32,
            max_video_bitrate_kbps: 0,
            allow_video_over_tor: false,
            call_scope: CallScope::Mesh,
            hub: HubPolicy::none(),
        }
    }

    /// Whether this policy permits a video stream at all.
    ///
    /// A zero video cap turns video off even if a video codec is listed, and a
    /// policy that lists only audio codecs cannot negotiate video regardless
    /// of the cap. Tor and mixnet need `allow_video_over_tor` as an explicit
    /// opt-in, because video over those transports is exactly what the flag
    /// documents; without it the policy forbids video on its only transport.
    pub fn video_enabled(&self) -> bool {
        let transport_allows_video = match self.transport {
            TransportPolicy::Direct | TransportPolicy::RelayOnly => true,
            TransportPolicy::Tor | TransportPolicy::Mixnet => self.allow_video_over_tor,
        };
        transport_allows_video
            && self.max_video_bitrate_kbps > 0
            && self.codecs.iter().any(|codec| is_video_codec(codec))
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != POLICY_VERSION {
            return Err(CoreError::InvalidInput(format!(
                "unsupported media policy version {}",
                self.version
            )));
        }
        if !self.e2ee {
            return Err(CoreError::InvalidInput(
                "media policy must keep end-to-end encryption enabled".to_string(),
            ));
        }
        // A hub policy that cannot run is a configuration mistake, not a
        // preference: a mesh has no hub, a star needs one, and a hub that
        // cannot decrypt cannot mix.
        match (self.call_scope, self.hub.mode) {
            (CallScope::Mesh, HubMode::None) => {}
            (CallScope::Mesh, mode) => {
                return Err(CoreError::InvalidInput(format!(
                    "call scope mesh cannot use hub mode {}",
                    mode.name()
                )));
            }
            (CallScope::Star | CallScope::Auto, HubMode::None) => {
                return Err(CoreError::InvalidInput(
                    "call scope star or auto needs an elected or server hub".to_string(),
                ));
            }
            (CallScope::Star | CallScope::Auto, HubMode::Elected | HubMode::Server) => {}
        }
        if self.hub.trust == HubTrust::CiphertextOnly && self.hub.allow_mixing {
            return Err(CoreError::InvalidInput(
                "a ciphertext-only hub cannot mix audio".to_string(),
            ));
        }
        // The local daemon advertises trust tier 1. A floor above that would
        // validate but leave every production peer ineligible, which is a
        // policy that cannot run rather than a stricter one.
        if self.hub.min_trust_tier > 1 {
            return Err(CoreError::InvalidInput(
                "hub min_trust_tier must be 0 or 1 until trust tiers are assigned".to_string(),
            ));
        }
        // The legacy topology field must not contradict the new scope.
        if matches!(self.call_scope, CallScope::Mesh)
            && !matches!(self.topology, MediaTopology::Mesh)
        {
            return Err(CoreError::InvalidInput(
                "legacy media topology must be mesh when call_scope is mesh".to_string(),
            ));
        }
        if self.codecs.is_empty() {
            return Err(CoreError::InvalidInput(
                "media policy must name at least one codec".to_string(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for codec in &self.codecs {
            if codec.is_empty() || codec.len() > 24 {
                return Err(CoreError::InvalidInput(
                    "a media codec name must be 1 to 24 bytes".to_string(),
                ));
            }
            if !codec
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            {
                return Err(CoreError::InvalidInput(format!(
                    "media codec {codec:?} must be lowercase ascii, digits or '-'"
                )));
            }
            if !seen.insert(codec.as_str()) {
                return Err(CoreError::InvalidInput(format!(
                    "media codec {codec:?} appears more than once"
                )));
            }
        }
        if !(8..=512).contains(&self.max_audio_bitrate_kbps) {
            return Err(CoreError::InvalidInput(format!(
                "media audio bitrate {} kbps is outside 8..=512",
                self.max_audio_bitrate_kbps
            )));
        }
        if self.max_video_bitrate_kbps > 8_000 {
            return Err(CoreError::InvalidInput(format!(
                "media video bitrate {} kbps is outside 0..=8000",
                self.max_video_bitrate_kbps
            )));
        }
        if self.max_video_bitrate_kbps == 0 && self.codecs.iter().any(|codec| is_video_codec(codec))
        {
            return Err(CoreError::InvalidInput(
                "media video bitrate is zero but the codec list contains a video codec".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_match_the_documented_axes() {
        let standard = GuildPolicy::standard();
        assert_eq!(standard.transport, TransportPolicy::Direct);
        assert_eq!(standard.plugins, PluginPolicy::BuiltInOnly);
        assert_eq!(standard.key_custody, KeyCustody::SharedEpochKey);

        let private = GuildPolicy::private();
        assert_eq!(private.transport, TransportPolicy::RelayOnly);
        assert!(matches!(
            private.storage,
            StoragePolicy::ShardedCold {
                k_data: 3,
                m: 2,
                allow_low_trust: false
            }
        ));
        assert_eq!(private.plugins, PluginPolicy::SignedAllowlist);

        let paranoid = GuildPolicy::paranoid();
        assert_eq!(paranoid.transport, TransportPolicy::Tor);
        assert_eq!(paranoid.key_custody, KeyCustody::Threshold { k: 3, n: 5 });
        assert_eq!(paranoid.hardware_keys, HardwareKeys::Required);
        assert_eq!(paranoid.plugins, PluginPolicy::BuiltInOnly);
        assert_eq!(paranoid.author_identity, IdentityPolicy::Pseudonymous);
        assert_eq!(paranoid.timestamps, TimestampPolicy::Coarse);
    }

    #[test]
    fn media_presets_choose_an_explicit_call_scope_and_hub() {
        let standard = MediaPolicy::standard();
        assert_eq!(standard.call_scope, CallScope::Auto);
        assert_eq!(standard.hub.mode, HubMode::Elected);
        assert_eq!(standard.hub.trust, HubTrust::Trusted);
        assert!(standard.hub.can_mix());

        let paranoid = MediaPolicy::paranoid();
        assert_eq!(paranoid.call_scope, CallScope::Mesh);
        assert_eq!(paranoid.hub.mode, HubMode::None);
        assert!(!paranoid.hub.can_decrypt());
        assert!(!paranoid.hub.can_mix());

        let pairwise = HubPolicy::pairwise();
        assert_eq!(pairwise.mode, HubMode::Elected);
        assert!(!pairwise.can_decrypt());
        assert!(!pairwise.can_mix());
    }

    #[test]
    fn a_contradictory_hub_policy_is_rejected() {
        let mut media = MediaPolicy {
            call_scope: CallScope::Mesh,
            hub: HubPolicy::elected(),
            ..MediaPolicy::standard()
        };
        assert!(media.validate().is_err(), "mesh with a hub must be refused");

        media = MediaPolicy {
            call_scope: CallScope::Star,
            hub: HubPolicy::none(),
            ..MediaPolicy::standard()
        };
        assert!(
            media.validate().is_err(),
            "star without a hub must be refused"
        );

        media = MediaPolicy {
            call_scope: CallScope::Star,
            hub: HubPolicy {
                trust: HubTrust::CiphertextOnly,
                allow_mixing: true,
                ..HubPolicy::pairwise()
            },
            ..MediaPolicy::standard()
        };
        assert!(
            media.validate().is_err(),
            "a ciphertext-only hub cannot mix"
        );

        media = MediaPolicy {
            call_scope: CallScope::Star,
            hub: HubPolicy {
                min_trust_tier: 2,
                ..HubPolicy::elected()
            },
            ..MediaPolicy::standard()
        };
        assert!(
            media.validate().is_err(),
            "a trust floor above the local tier must be refused, not silently leave no eligible hub"
        );

        let valid = MediaPolicy {
            call_scope: CallScope::Star,
            hub: HubPolicy::pairwise(),
            ..MediaPolicy::standard()
        };
        valid.validate().unwrap();
    }

    #[test]
    fn all_presets_validate() {
        for profile in GuildProfile::ALL {
            GuildPolicy::for_profile(profile)
                .validate()
                .unwrap_or_else(|error| panic!("{} failed: {error}", profile.name()));
        }
        MediaPolicy::standard().validate().unwrap();
        MediaPolicy::private().validate().unwrap();
        MediaPolicy::paranoid().validate().unwrap();
    }

    #[test]
    fn video_needs_a_transport_that_can_carry_it() {
        let mut media = MediaPolicy {
            transport: TransportPolicy::Tor,
            codecs: vec!["opus".to_string(), "vp9".to_string()],
            max_video_bitrate_kbps: 4000,
            allow_video_over_tor: false,
            ..MediaPolicy::standard()
        };
        media.validate().unwrap();
        assert!(
            !media.video_enabled(),
            "video must not be offered over Tor without the explicit opt-in"
        );

        media.allow_video_over_tor = true;
        assert!(media.video_enabled());

        media.transport = TransportPolicy::Mixnet;
        media.allow_video_over_tor = false;
        assert!(!media.video_enabled(), "mixnet needs the same opt-in");

        media.transport = TransportPolicy::Direct;
        assert!(
            media.video_enabled(),
            "a direct transport does not need the Tor opt-in"
        );
    }

    #[test]
    fn a_client_floor_can_only_raise_an_axis() {
        let floor = ClientPolicyFloor {
            transport: Some(TransportPolicy::Tor),
            plugins: Some(PluginPolicy::BuiltInOnly),
            padding: Some(PaddingPolicy::Buckets),
            ..ClientPolicyFloor::default()
        };

        let raised = GuildPolicy::standard().with_client_floor(&floor);
        assert_eq!(raised.transport, TransportPolicy::Tor);
        assert_eq!(raised.padding, PaddingPolicy::Buckets);

        // A paranoid guild plus a lax client minimum stays paranoid.
        let lax = ClientPolicyFloor {
            transport: Some(TransportPolicy::Direct),
            plugins: Some(PluginPolicy::Any),
            ..ClientPolicyFloor::default()
        };
        let guarded = GuildPolicy::paranoid().with_client_floor(&lax);
        assert_eq!(guarded.transport, TransportPolicy::Tor);
        assert_eq!(guarded.plugins, PluginPolicy::BuiltInOnly);
    }

    #[test]
    fn invalid_thresholds_and_ttls_are_refused() {
        let mut policy = GuildPolicy::paranoid();
        policy.key_custody = KeyCustody::Threshold { k: 6, n: 5 };
        assert!(policy.validate().is_err());

        policy.key_custody = KeyCustody::Threshold { k: 0, n: 5 };
        assert!(policy.validate().is_err());

        let mut policy = GuildPolicy::private();
        policy.storage = StoragePolicy::ShardedCold {
            k_data: 0,
            m: 2,
            allow_low_trust: false,
        };
        assert!(policy.validate().is_err());

        let mut policy = GuildPolicy::standard();
        policy.retention = RetentionPolicy::Ttl { secs: 0 };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn media_policy_rejects_bad_codecs_and_bitrates() {
        let mut media = MediaPolicy::standard();
        media.codecs = vec!["Opus".to_string()];
        assert!(media.validate().is_err());

        media.codecs = vec!["opus".to_string(), "opus".to_string()];
        assert!(media.validate().is_err());

        media.codecs = vec!["opus".to_string()];
        media.max_audio_bitrate_kbps = 0;
        assert!(media.validate().is_err());

        media.max_audio_bitrate_kbps = 513;
        assert!(media.validate().is_err(), "audio is capped at 512 kbps");

        media.max_audio_bitrate_kbps = 128;
        media.max_video_bitrate_kbps = 8_001;
        assert!(media.validate().is_err(), "video is capped at 8000 kbps");

        media.max_video_bitrate_kbps = 0;
        media.codecs = vec!["opus".to_string(), "vp9".to_string()];
        assert!(
            media.validate().is_err(),
            "a zero video cap with a video codec would negotiate an unusable stream"
        );

        media.codecs = vec!["opus".to_string()];
        media.e2ee = false;
        assert!(media.validate().is_err());
    }

    #[test]
    fn the_media_caps_split_audio_and_video() {
        let standard = MediaPolicy::standard();
        assert_eq!(standard.max_audio_bitrate_kbps, 512);
        assert_eq!(standard.max_video_bitrate_kbps, 4000);
        assert!(standard.video_enabled());

        let private = MediaPolicy::private();
        assert_eq!(private.max_audio_bitrate_kbps, 128);
        assert_eq!(private.max_video_bitrate_kbps, 0);
        assert!(!private.video_enabled());

        let paranoid = MediaPolicy::paranoid();
        assert_eq!(paranoid.max_audio_bitrate_kbps, 32);
        assert_eq!(paranoid.max_video_bitrate_kbps, 0);
        assert!(!paranoid.video_enabled());
    }

    #[test]
    fn video_enabled_needs_both_a_cap_and_a_video_codec() {
        let mut media = MediaPolicy::standard();
        media.codecs = vec!["opus".to_string()];
        assert!(!media.video_enabled());
        media.max_video_bitrate_kbps = 0;
        media.codecs = vec!["opus".to_string(), "vp8".to_string()];
        assert!(!media.video_enabled());
        media.max_video_bitrate_kbps = 1;
        assert!(media.video_enabled());
    }

    #[test]
    fn a_version_one_policy_is_rejected_after_the_bump() {
        let mut guild = GuildPolicy::standard();
        guild.version = 1;
        assert!(
            guild.validate().is_err(),
            "a version 1 guild policy must not validate against version 2"
        );

        let mut media = MediaPolicy::standard();
        media.version = 1;
        assert!(
            media.validate().is_err(),
            "a version 1 media policy must not validate against version 2"
        );
    }

    #[test]
    fn an_allowlist_sequence_needs_a_hash() {
        let mut policy = GuildPolicy::private();
        policy.plugin_allowlist_hash = None;
        policy.plugin_allowlist_sequence = 1;
        assert!(policy.validate().is_err());

        policy.plugin_allowlist_hash = Some([7u8; 32]);
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn policies_round_trip_through_serde() {
        let policy = GuildPolicy::paranoid();
        let bytes = postcard::to_allocvec(&policy).unwrap();
        assert_eq!(postcard::from_bytes::<GuildPolicy>(&bytes).unwrap(), policy);
    }

    #[test]
    fn topology_describes_its_trust_model() {
        assert!(!GuildTopology::P2PSharded.requires_server());
        assert!(GuildTopology::ServerAssisted.requires_server());
        assert!(!GuildTopology::ServerAssisted.server_owns_ordering());
        assert!(GuildTopology::ServerAuthoritative.server_owns_ordering());
    }
}
