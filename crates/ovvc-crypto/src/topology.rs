//! Which peers a call member needs a pairwise media key with.
//!
//! The full mesh installs a cipher for every other member. The flat star
//! installs one leaf-to-hub key at each leaf and one key per leaf at the hub:
//! a leaf seals its uplink only to the hub, and the hub installs a cipher for
//! every leaf because it decrypts, mixes and re-seals each frame for its
//! recipient. No leaf holds a key for another leaf.
//!
//! This type is deliberately in `ovvc-crypto` rather than `ovvc-media`: it is
//! a key-selection rule, not a call-membership rule. The media plane decides
//! the topology, and the daemon uses this to decide which published peer keys
//! it actually installs.

use ovvc_core::{AuthorId, HubPolicy};
use ovvc_media::CallTopology;

/// The pairwise key set for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyTopology {
    /// One cipher per other member.
    AllPairs,
    /// One cipher per leaf at the hub, and one leaf-to-hub cipher at each
    /// leaf.
    Star { hub: AuthorId },
}

impl KeyTopology {
    /// The key set implied by a call topology.
    ///
    /// A two-member mesh is a direct pair and uses one pairwise key, which is
    /// the same key set as `AllPairs` for two members.
    pub fn for_call(topology: &CallTopology) -> Self {
        match topology {
            CallTopology::Mesh => Self::AllPairs,
            CallTopology::Star { hub, .. } => Self::Star { hub: *hub },
        }
    }

    /// The key set implied by a call topology and the commune hub policy.
    ///
    /// A trusted hub decrypts and mixes, so the leaf-to-hub key is enough. A
    /// ciphertext-only hub must not see the media, so every leaf keeps an
    /// all-pairs key set and the hub only relays opaque per-recipient frames.
    /// That is the paranoid star the policy allows, and it costs the leaf an
    /// uplink per recipient.
    pub fn for_call_with_hub(topology: &CallTopology, policy: HubPolicy) -> Self {
        match topology {
            CallTopology::Mesh => Self::AllPairs,
            CallTopology::Star { hub, .. } if policy.can_decrypt() => Self::Star { hub: *hub },
            CallTopology::Star { .. } => Self::AllPairs,
        }
    }

    /// The hub of a star, or `None` for an all-pairs call.
    pub const fn hub(self) -> Option<AuthorId> {
        match self {
            Self::AllPairs => None,
            Self::Star { hub } => Some(hub),
        }
    }

    /// Whether `local` needs a cipher for `peer`.
    ///
    /// A leaf needs only the hub. The hub needs every other member, because
    /// it re-seals each recipient's copy. A member outside `members` is never
    /// a key peer, and a member never needs a key for itself.
    pub fn includes(self, local: AuthorId, peer: AuthorId, members: &[AuthorId]) -> bool {
        if local == peer || !members.contains(&peer) {
            return false;
        }
        match self {
            Self::AllPairs => true,
            Self::Star { hub } => local == hub || peer == hub,
        }
    }

    /// The key peers for `local`, in deterministic member order.
    pub fn peers_for(self, local: AuthorId, members: &[AuthorId]) -> Vec<AuthorId> {
        members
            .iter()
            .copied()
            .filter(|peer| self.includes(local, *peer, members))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn author(index: u8) -> AuthorId {
        AuthorId([index; 32])
    }

    #[test]
    fn a_mesh_keys_every_other_member() {
        let members = [author(1), author(2), author(3)];
        let keys = KeyTopology::AllPairs;
        assert_eq!(keys.hub(), None);
        assert_eq!(
            keys.peers_for(author(1), &members),
            vec![author(2), author(3)]
        );
        assert_eq!(
            keys.peers_for(author(2), &members),
            vec![author(1), author(3)]
        );
    }

    #[test]
    fn a_leaf_keys_only_the_hub_and_the_hub_keys_every_leaf() {
        let hub = author(2);
        let members = [author(1), hub, author(3)];
        let keys = KeyTopology::Star { hub };
        assert_eq!(keys.hub(), Some(hub));
        assert_eq!(
            keys.peers_for(author(1), &members),
            vec![hub],
            "a leaf seals its uplink only to the hub"
        );
        assert_eq!(
            keys.peers_for(author(3), &members),
            vec![hub],
            "a leaf cannot decrypt another leaf's media"
        );
        assert_eq!(
            keys.peers_for(hub, &members),
            vec![author(1), author(3)],
            "the hub re-seals each frame for every recipient"
        );
    }

    #[test]
    fn a_two_member_call_uses_one_pairwise_key_under_either_topology() {
        let members = [author(1), author(2)];
        let direct = KeyTopology::for_call(&CallTopology::Mesh);
        let star = KeyTopology::for_call(&CallTopology::Star {
            hub: author(1),
            leaves: vec![author(2)],
        });
        assert_eq!(direct.peers_for(author(2), &members), vec![author(1)]);
        assert_eq!(star.peers_for(author(2), &members), vec![author(1)]);
    }

    #[test]
    fn a_ciphertext_only_hub_keeps_the_all_pairs_key_set() {
        let topology = CallTopology::Star {
            hub: author(2),
            leaves: vec![author(1), author(3)],
        };
        let trusted = KeyTopology::for_call_with_hub(&topology, HubPolicy::elected());
        assert_eq!(trusted, KeyTopology::Star { hub: author(2) });
        // The same star with a hub that cannot decrypt is a relay: every leaf
        // seals for every recipient, because the hub cannot re-seal.
        let relayed = KeyTopology::for_call_with_hub(&topology, HubPolicy::pairwise());
        assert_eq!(relayed, KeyTopology::AllPairs);
        let members = [author(1), author(2), author(3)];
        assert_eq!(
            relayed.peers_for(author(1), &members),
            vec![author(2), author(3)]
        );
    }

    #[test]
    fn a_star_key_set_never_contains_a_leaf_other_than_the_hub() {
        let hub = author(2);
        let members = [author(1), hub, author(3), author(4)];
        let keys = KeyTopology::for_call(&CallTopology::Star {
            hub,
            leaves: vec![author(1), author(3), author(4)],
        });
        for leaf in [author(1), author(3), author(4)] {
            assert_eq!(keys.peers_for(leaf, &members), vec![hub]);
            for peer in keys.peers_for(leaf, &members) {
                assert_ne!(peer, leaf, "a member never keys itself");
            }
        }
        assert_eq!(
            keys.peers_for(hub, &members),
            vec![author(1), author(3), author(4)]
        );
    }

    #[test]
    fn a_key_peer_outside_the_call_is_ignored() {
        let keys = KeyTopology::Star { hub: author(1) };
        let members = [author(1), author(2)];
        assert!(!keys.includes(author(2), author(9), &members));
        assert!(!keys.includes(author(2), author(2), &members));
    }
}
