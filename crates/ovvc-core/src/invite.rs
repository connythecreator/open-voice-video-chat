use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::identity::Identity;
use crate::model::{AuthorId, GuildId, decode_canonical, now_unix};

const INVITE_VERSION: u8 = 1;
const INVITE_SIGNING_DOMAIN: &[u8] = b"ovvc/invite/v1\0";
const MAX_INVITE_LEN: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteBody {
    pub version: u8,
    pub guild_id: GuildId,
    pub guild_name: String,
    pub host: AuthorId,
    pub addresses: Vec<String>,
    pub created_at: u64,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub body: InviteBody,
    pub signature: Vec<u8>,
}

impl Invite {
    pub fn new(
        guild_id: GuildId,
        guild_name: String,
        host: &Identity,
        addresses: Vec<String>,
        expires_at: Option<u64>,
    ) -> Result<Self> {
        let body = InviteBody {
            version: INVITE_VERSION,
            guild_id,
            guild_name,
            host: host.author_id(),
            addresses,
            created_at: now_unix(),
            expires_at,
        };
        let signature = host.sign_bytes(&signing_bytes(&body)?);
        let invite = Self { body, signature };
        // Refuse to mint a code our own decoder would reject. The text is what
        // travels out of band, so the limit applies to the encoded form.
        invite.encode()?;
        Ok(invite)
    }

    pub fn verify(&self) -> Result<()> {
        if self.body.version != INVITE_VERSION {
            return Err(CoreError::InvalidInput(format!(
                "unsupported invite version: {}",
                self.body.version
            )));
        }
        if self.signature.len() != 64 {
            return Err(CoreError::InvalidSignature);
        }
        let signature = Signature::from_slice(&self.signature)?;
        let verifying_key = VerifyingKey::from_bytes(&self.body.host.0)?;
        verifying_key
            .verify_strict(&signing_bytes(&self.body)?, &signature)
            .map_err(|_| CoreError::InvalidSignature)?;
        if let Some(expires_at) = self.body.expires_at
            && now_unix() >= expires_at
        {
            return Err(CoreError::InvalidInput("invite has expired".to_string()));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<String> {
        let bytes = postcard::to_allocvec(self)?;
        let encoded = URL_SAFE_NO_PAD.encode(bytes);
        if encoded.len() > MAX_INVITE_LEN {
            return Err(CoreError::InvalidInput(format!(
                "invite is too large: {} characters",
                encoded.len()
            )));
        }
        Ok(encoded)
    }

    pub fn decode(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.len() > MAX_INVITE_LEN {
            return Err(CoreError::InvalidInput("invite is too large".to_string()));
        }
        let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|error| {
            CoreError::InvalidInput(format!("invalid invite encoding: {error}"))
        })?;
        decode_canonical(&bytes)
    }
}

fn signing_bytes(body: &InviteBody) -> Result<Vec<u8>> {
    let encoded = postcard::to_allocvec(body)?;
    let mut bytes = Vec::with_capacity(INVITE_SIGNING_DOMAIN.len() + encoded.len());
    bytes.extend_from_slice(INVITE_SIGNING_DOMAIN);
    bytes.extend_from_slice(&encoded);
    Ok(bytes)
}
