//! Self-authenticating peer announcements published to the DHT.
//!
//! The Kademlia record is treated as untrusted transport: the value carries a
//! signature by the announcing identity, so a lookup can verify that the
//! returned addresses really belong to the author it was looking for. The DHT
//! itself never has to be trusted for authenticity, only for availability.

use ovvc_core::model::{decode_canonical, now_unix};
use ovvc_core::{AuthorId, CoreError, Identity, Result};
use serde::{Deserialize, Serialize};

const ANNOUNCE_VERSION: u8 = 1;
const ANNOUNCE_SIGNING_DOMAIN: &[u8] = b"ovvc/announce/v1\0";
const MAX_ANNOUNCEMENT_BYTES: usize = 4096;

/// Upper bound on addresses carried in one announcement.
pub const MAX_ANNOUNCED_ADDRESSES: usize = 8;

/// Longest lifetime a verifier accepts. Republishing is cheap, and a record
/// that claims to be valid for years cannot be withdrawn by its author.
pub const MAX_ANNOUNCEMENT_TTL_SECS: u64 = 7 * 24 * 3600;

/// Tolerance for clock skew when checking `expires_at` locally.
const EXPIRY_SKEW_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnouncementBody {
    pub version: u8,
    pub author: AuthorId,
    pub addresses: Vec<String>,
    pub issued_at: u64,
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Announcement {
    pub body: AnnouncementBody,
    pub signature: Vec<u8>,
}

impl Announcement {
    pub fn sign(author: &Identity, addresses: Vec<String>, ttl_secs: u64) -> Result<Self> {
        if ttl_secs == 0 || ttl_secs > MAX_ANNOUNCEMENT_TTL_SECS {
            return Err(CoreError::InvalidInput(format!(
                "announcement ttl must be between 1 and {MAX_ANNOUNCEMENT_TTL_SECS} seconds"
            )));
        }
        if !addresses_are_well_formed(&addresses) {
            return Err(CoreError::InvalidInput(
                "announcement contains a malformed address".to_string(),
            ));
        }
        let issued_at = now_unix();
        let body = AnnouncementBody {
            version: ANNOUNCE_VERSION,
            author: author.author_id(),
            addresses: addresses
                .into_iter()
                .take(MAX_ANNOUNCED_ADDRESSES)
                .collect(),
            issued_at,
            expires_at: issued_at.saturating_add(ttl_secs),
        };
        let signature = author.sign_bytes(&signing_bytes(&body)?);
        let announcement = Self { body, signature };
        // Never publish a record that our own decoder rejects.
        announcement.encode()?;
        Ok(announcement)
    }

    /// Check that this announcement was signed by `expected` and is fresh.
    pub fn verify(&self, expected: AuthorId) -> Result<()> {
        if self.body.version != ANNOUNCE_VERSION {
            return Err(CoreError::InvalidInput(format!(
                "unsupported announcement version: {}",
                self.body.version
            )));
        }
        if self.body.author != expected {
            return Err(CoreError::InvalidInput(
                "announcement author does not match lookup".to_string(),
            ));
        }
        if self.body.addresses.len() > MAX_ANNOUNCED_ADDRESSES {
            return Err(CoreError::InvalidInput(
                "announcement has too many addresses".to_string(),
            ));
        }
        if !addresses_are_well_formed(&self.body.addresses) {
            return Err(CoreError::InvalidInput(
                "announcement contains a malformed address".to_string(),
            ));
        }
        if self.body.expires_at <= self.body.issued_at {
            return Err(CoreError::InvalidInput(
                "announcement expiry is not after its issue time".to_string(),
            ));
        }
        if self.body.expires_at - self.body.issued_at
            > MAX_ANNOUNCEMENT_TTL_SECS.saturating_add(EXPIRY_SKEW_SECS)
        {
            return Err(CoreError::InvalidInput(
                "announcement lifetime is too long".to_string(),
            ));
        }
        if self.body.issued_at > now_unix().saturating_add(EXPIRY_SKEW_SECS) {
            return Err(CoreError::InvalidInput(
                "announcement was issued in the future".to_string(),
            ));
        }
        if now_unix() > self.body.expires_at.saturating_add(EXPIRY_SKEW_SECS) {
            return Err(CoreError::InvalidInput(
                "announcement has expired".to_string(),
            ));
        }
        if !expected.verify(&signing_bytes(&self.body)?, &self.signature) {
            return Err(CoreError::InvalidSignature);
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let bytes = postcard::to_allocvec(self)?;
        if bytes.len() > MAX_ANNOUNCEMENT_BYTES {
            return Err(CoreError::InvalidInput(format!(
                "announcement is too large: {} bytes",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_ANNOUNCEMENT_BYTES {
            return Err(CoreError::InvalidInput(format!(
                "announcement is too large: {} bytes",
                bytes.len()
            )));
        }
        decode_canonical(bytes)
    }
}

fn addresses_are_well_formed(addresses: &[String]) -> bool {
    addresses
        .iter()
        .all(|address| address.parse::<libp2p::Multiaddr>().is_ok())
}

fn signing_bytes(body: &AnnouncementBody) -> Result<Vec<u8>> {
    let encoded = postcard::to_allocvec(body)?;
    let mut bytes = Vec::with_capacity(ANNOUNCE_SIGNING_DOMAIN.len() + encoded.len());
    bytes.extend_from_slice(ANNOUNCE_SIGNING_DOMAIN);
    bytes.extend_from_slice(&encoded);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn author(seed: u8) -> Identity {
        Identity::from_seed([seed; 32])
    }

    #[test]
    fn announcement_roundtrip_and_tamper_detection() {
        let alice = author(1);
        let announcement =
            Announcement::sign(&alice, vec!["/ip4/127.0.0.1/tcp/7000".to_string()], 3600).unwrap();
        announcement.verify(alice.author_id()).unwrap();

        let bytes = announcement.encode().unwrap();
        let decoded = Announcement::decode(&bytes).unwrap();
        assert_eq!(decoded, announcement);

        // Tampering with the address list must break the signature.
        let mut tampered = decoded;
        tampered.body.addresses = vec!["/ip4/6.6.6.6/tcp/666".to_string()];
        assert!(matches!(
            tampered.verify(alice.author_id()),
            Err(CoreError::InvalidSignature)
        ));
    }

    #[test]
    fn announcement_rejects_wrong_author_and_expiry() {
        let alice = author(2);
        let bob = author(3);
        let announcement = Announcement::sign(&alice, Vec::new(), 3600).unwrap();

        assert!(announcement.verify(bob.author_id()).is_err());

        let expired = AnnouncementBody {
            version: ANNOUNCE_VERSION,
            author: alice.author_id(),
            addresses: Vec::new(),
            issued_at: 0,
            expires_at: 1,
        };
        let announcement = Announcement {
            signature: alice.sign_bytes(&signing_bytes(&expired).unwrap()),
            body: expired,
        };
        assert!(announcement.verify(alice.author_id()).is_err());
    }

    #[test]
    fn oversized_announcements_are_rejected() {
        assert!(Announcement::decode(&vec![0u8; MAX_ANNOUNCEMENT_BYTES + 1]).is_err());
    }

    #[test]
    fn announcement_rejects_malformed_addresses_and_absurd_lifetimes() {
        let alice = author(4);
        assert!(Announcement::sign(&alice, vec!["not a multiaddr".to_string()], 3600).is_err());
        assert!(Announcement::sign(&alice, Vec::new(), 0).is_err());
        assert!(Announcement::sign(&alice, Vec::new(), MAX_ANNOUNCEMENT_TTL_SECS + 1).is_err());

        // A signer that ignores the cap must still be rejected on verify.
        let now = now_unix();
        let body = AnnouncementBody {
            version: ANNOUNCE_VERSION,
            author: alice.author_id(),
            addresses: Vec::new(),
            issued_at: now,
            expires_at: now + MAX_ANNOUNCEMENT_TTL_SECS + EXPIRY_SKEW_SECS + 1,
        };
        let announcement = Announcement {
            signature: alice.sign_bytes(&signing_bytes(&body).unwrap()),
            body,
        };
        assert!(announcement.verify(alice.author_id()).is_err());
    }

    #[test]
    fn announcement_decoder_rejects_trailing_bytes() {
        let alice = author(5);
        let mut bytes =
            Announcement::sign(&alice, vec!["/ip4/127.0.0.1/tcp/7000".to_string()], 3600)
                .unwrap()
                .encode()
                .unwrap();
        bytes.push(0);
        assert!(Announcement::decode(&bytes).is_err());
    }
}
